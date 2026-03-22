//! Billing middleware for auto-account creation.
//!
//! This middleware ensures a billing account exists for authenticated users
//! on their first request.

use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::Request;
use axum::response::Response;
use futures::future::BoxFuture;
use tower::{Layer, Service};

use super::service::BillingService;

/// Layer that adds billing account auto-creation.
#[derive(Clone)]
pub struct BillingAccountLayer {
    billing: Arc<BillingService>,
}

impl BillingAccountLayer {
    /// Create a new billing account layer.
    #[must_use]
    pub fn new(billing: Arc<BillingService>) -> Self {
        Self { billing }
    }
}

impl<S> Layer<S> for BillingAccountLayer {
    type Service = BillingAccountMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BillingAccountMiddleware {
            inner,
            billing: Arc::clone(&self.billing),
        }
    }
}

/// Middleware that auto-creates billing accounts for authenticated users.
#[derive(Clone)]
pub struct BillingAccountMiddleware<S> {
    inner: S,
    billing: Arc<BillingService>,
}

impl<S> Service<Request<Body>> for BillingAccountMiddleware<S>
where
    S: Service<Request<Body>, Response = Response> + Send + Clone + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let billing = Arc::clone(&self.billing);
        let mut inner = self.inner.clone();

        Box::pin(async move {
            // Try to extract user_id from request extensions (set by auth)
            if let Some(user_id) = extract_user_id(&request) {
                // Fire-and-forget account creation (background task)
                let billing_clone = Arc::clone(&billing);
                let user_id_clone = user_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = billing_clone.ensure_account(&user_id_clone).await {
                        tracing::warn!(
                            user_id = %user_id_clone,
                            error = %e,
                            "Failed to ensure billing account"
                        );
                    }
                });
            }

            inner.call(request).await
        })
    }
}

/// Try to extract `user_id` from request headers or extensions.
///
/// The auth extractor typically sets this as a header or extension.
fn extract_user_id(request: &Request<Body>) -> Option<String> {
    // Check for user_id in a custom header (set by auth middleware)
    if let Some(header) = request.headers().get("x-user-id") {
        return header.to_str().ok().map(String::from);
    }

    // Also check extensions if the auth layer put it there
    request.extensions().get::<UserId>().map(|id| id.0.clone())
}

/// User ID wrapper for request extensions.
#[derive(Clone, Debug)]
pub struct UserId(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_creation() {
        use crate::billing::BillingConfig;
        let config = BillingConfig::default();
        let service = BillingService::new(config);
        let _layer = BillingAccountLayer::new(Arc::new(service));
    }
}

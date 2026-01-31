//! Health check endpoint.
//!
//! This module provides the public health check endpoint.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

/// Health check response.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// Service status.
    pub status: &'static str,
    /// Service version.
    pub version: &'static str,
}

/// Health check handler.
///
/// Returns the current service status. This endpoint is public and
/// does not require authentication.
///
/// # Example
///
/// ```text
/// GET /health
///
/// Response: 200 OK
/// {
///   "status": "healthy",
///   "version": "0.1.0"
/// }
/// ```
pub async fn health() -> impl IntoResponse {
    let response = HealthResponse {
        status: "healthy",
        version: env!("CARGO_PKG_VERSION"),
    };

    (StatusCode::OK, Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_returns_ok() {
        let response = health().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

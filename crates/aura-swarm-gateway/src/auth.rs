//! Authentication middleware and extractors.
//!
//! This module provides the `AuthUser` extractor that validates JWT tokens
//! and extracts user identity from requests.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use aura_swarm_auth::{JwtValidator, ValidatedClaims};
use aura_swarm_control::ControlPlane;
use aura_swarm_core::UserId;

use crate::error::ApiError;
use crate::state::GatewayState;

/// An authenticated user extracted from a JWT token.
///
/// This extractor validates the `Authorization: Bearer <token>` header
/// and provides access to the user's identity.
#[derive(Debug, Clone)]
pub struct AuthUser {
    /// The zOS user ID.
    pub user_id: UserId,
    /// The raw Bearer token, forwarded to agent pods.
    pub token: String,
}

impl AuthUser {
    /// Create an `AuthUser` from validated claims and the raw token.
    #[must_use]
    pub fn from_claims(claims: &ValidatedClaims, token: &str) -> Self {
        Self {
            user_id: claims.user_id,
            token: token.to_string(),
        }
    }
}

impl<C, V> FromRequestParts<Arc<GatewayState<C, V>>> for AuthUser
where
    C: ControlPlane + 'static,
    V: JwtValidator + 'static,
{
    type Rejection = ApiError;

    fn from_request_parts<'life0, 'life1, 'async_trait>(
        parts: &'life0 mut Parts,
        state: &'life1 Arc<GatewayState<C, V>>,
    ) -> ::core::pin::Pin<
        Box<
            dyn ::core::future::Future<Output = Result<Self, Self::Rejection>>
                + ::core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            // Extract the Authorization header
            let auth_header = parts
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .ok_or(ApiError::Unauthorized)?;

            // Extract the Bearer token
            let token = auth_header
                .strip_prefix("Bearer ")
                .ok_or(ApiError::Unauthorized)?;

            // Validate the token
            let claims = state.jwt_validator.validate(token).await?;

            Ok(AuthUser::from_claims(&claims, token))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_swarm_core::UserId;
    use chrono::{Duration, Utc};
    use std::str::FromStr;

    #[test]
    fn auth_user_from_claims() {
        let user_id = UserId::from_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let claims = ValidatedClaims {
            user_id,
            expires_at: Utc::now() + Duration::hours(1),
        };

        let user = AuthUser::from_claims(&claims, "test-token");
        assert_eq!(user.user_id, user_id);
        assert_eq!(user.token, "test-token");
    }
}

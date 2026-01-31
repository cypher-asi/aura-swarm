//! Authentication middleware and extractors.
//!
//! This module provides the `AuthUser` extractor that validates JWT tokens
//! and extracts user identity from requests.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use aura_swarm_auth::{JwtValidator, ValidatedClaims};
use aura_swarm_control::ControlPlane;
use aura_swarm_core::{IdentityId, NamespaceId, SessionId, UserId};

use crate::error::ApiError;
use crate::state::GatewayState;

/// An authenticated user extracted from a JWT token.
///
/// This extractor validates the `Authorization: Bearer <token>` header
/// and provides access to the user's identity.
#[derive(Debug, Clone)]
pub struct AuthUser {
    /// The ZID identity ID.
    pub identity_id: IdentityId,
    /// The ZID namespace/tenant ID.
    pub namespace_id: NamespaceId,
    /// The ZID session ID (from the token).
    pub zid_session_id: SessionId,
    /// Whether MFA was verified for this session.
    pub mfa_verified: bool,
    /// The internal user ID derived from `identity_id`.
    pub user_id: UserId,
}

impl AuthUser {
    /// Create an `AuthUser` from validated claims.
    #[must_use]
    pub fn from_claims(claims: &ValidatedClaims) -> Self {
        // Derive a UserId from the identity_id
        // We use blake3 to convert the 16-byte UUID to a 32-byte UserId
        let mut hasher = blake3::Hasher::new();
        hasher.update(claims.identity_id.as_bytes());
        let user_id = UserId::from_bytes(*hasher.finalize().as_bytes());

        Self {
            identity_id: claims.identity_id,
            namespace_id: claims.namespace_id,
            zid_session_id: claims.session_id,
            mfa_verified: claims.mfa_verified,
            user_id,
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

            Ok(AuthUser::from_claims(&claims))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_swarm_core::IdentityId;
    use chrono::{Duration, Utc};
    use std::str::FromStr;

    #[test]
    fn auth_user_from_claims() {
        let identity_id = IdentityId::from_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let namespace_id = NamespaceId::from_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();
        let session_id = SessionId::generate();

        let claims = ValidatedClaims {
            identity_id,
            namespace_id,
            session_id,
            mfa_verified: true,
            expires_at: Utc::now() + Duration::hours(1),
        };

        let user = AuthUser::from_claims(&claims);
        assert_eq!(user.identity_id, identity_id);
        assert_eq!(user.namespace_id, namespace_id);
        assert!(user.mfa_verified);
        // user_id should be derived from identity_id
        assert_eq!(user.user_id.as_bytes().len(), 32);
    }
}

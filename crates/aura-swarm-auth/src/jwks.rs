//! JWKS (JSON Web Key Set) fetching and caching.
//!
//! This module handles fetching public keys from the zOS JWKS endpoint
//! and caching them for efficient validation.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::prelude::*;
use jsonwebtoken::DecodingKey;
use parking_lot::RwLock;
use serde::Deserialize;

use crate::error::{AuthError, Result};
use crate::AuthConfig;

/// JWKS response from the authentication server.
#[derive(Debug, Deserialize)]
pub struct JwksResponse {
    /// The list of keys.
    pub keys: Vec<JwkKey>,
}

/// A single JWK (JSON Web Key).
#[derive(Debug, Deserialize)]
pub struct JwkKey {
    /// Key type (e.g., "OKP" for Ed25519, "RSA" for RSA).
    pub kty: String,
    /// Curve (e.g., "Ed25519").
    pub crv: Option<String>,
    /// Public key (base64url encoded, for OKP keys).
    pub x: Option<String>,
    /// RSA modulus (base64url encoded).
    pub n: Option<String>,
    /// RSA exponent (base64url encoded).
    pub e: Option<String>,
    /// Key ID.
    pub kid: Option<String>,
    /// Key use (e.g., "sig").
    #[serde(rename = "use")]
    pub key_use: Option<String>,
    /// Algorithm (e.g., `EdDSA`, `RS256`).
    pub alg: Option<String>,
}

/// Cached JWKS keys with expiration.
struct CachedKeys {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Instant,
}

impl Default for CachedKeys {
    fn default() -> Self {
        Self {
            keys: HashMap::new(),
            fetched_at: Instant::now()
                .checked_sub(Duration::from_secs(3600))
                .unwrap_or_else(Instant::now),
        }
    }
}

/// JWKS key provider that fetches and caches keys.
pub struct JwksProvider {
    config: AuthConfig,
    client: reqwest::Client,
    cache: RwLock<CachedKeys>,
}

impl JwksProvider {
    /// Create a new JWKS provider with the given configuration.
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client cannot be created (should never happen with default TLS).
    #[must_use]
    pub fn new(config: AuthConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to create HTTP client");

        Self {
            config,
            client,
            cache: RwLock::new(CachedKeys::default()),
        }
    }

    /// Get a decoding key by key ID, fetching from JWKS if necessary.
    ///
    /// # Errors
    ///
    /// Returns an error if the key is not found or JWKS fetch fails.
    pub async fn get_key(&self, kid: &str) -> Result<DecodingKey> {
        {
            let cache = self.cache.read();
            let refresh_interval = Duration::from_secs(self.config.jwks_refresh_seconds);
            if cache.fetched_at.elapsed() < refresh_interval {
                if let Some(key) = cache.keys.get(kid) {
                    return Ok(key.clone());
                }
            }
        }

        self.refresh_keys().await?;

        let cache = self.cache.read();
        cache
            .keys
            .get(kid)
            .cloned()
            .ok_or_else(|| AuthError::KeyNotFound(kid.to_string()))
    }

    /// Refresh the JWKS cache by fetching from the server.
    async fn refresh_keys(&self) -> Result<()> {
        let jwks_url = self.config.jwks_url();
        tracing::debug!(url = %jwks_url, "Fetching JWKS");

        let response: JwksResponse = self
            .client
            .get(&jwks_url)
            .send()
            .await
            .map_err(|e| AuthError::JwksFetchFailed(e.to_string()))?
            .json()
            .await
            .map_err(|e| AuthError::JwksFetchFailed(e.to_string()))?;

        let mut new_keys = HashMap::new();

        for key in response.keys {
            if let Some(kid) = &key.kid {
                if let Some(decoding_key) = Self::parse_key(&key)? {
                    new_keys.insert(kid.clone(), decoding_key);
                }
            }
        }

        tracing::debug!(count = new_keys.len(), "Cached JWKS keys");

        let mut cache = self.cache.write();
        cache.keys = new_keys;
        cache.fetched_at = Instant::now();

        Ok(())
    }

    /// Parse a JWK into a `DecodingKey`.
    fn parse_key(key: &JwkKey) -> Result<Option<DecodingKey>> {
        match key.kty.as_str() {
            "OKP" => {
                let crv = key.crv.as_deref().unwrap_or("");
                if crv != "Ed25519" {
                    tracing::warn!(crv = crv, "Unsupported OKP curve");
                    return Ok(None);
                }

                let x = key
                    .x
                    .as_ref()
                    .ok_or_else(|| AuthError::InvalidToken("missing x parameter".to_string()))?;

                let public_key = BASE64_URL_SAFE_NO_PAD
                    .decode(x)
                    .map_err(|e| AuthError::InvalidToken(format!("invalid base64: {e}")))?;

                Ok(Some(DecodingKey::from_ed_der(&public_key)))
            }
            "RSA" => {
                let n = key
                    .n
                    .as_ref()
                    .ok_or_else(|| AuthError::InvalidToken("missing n parameter".to_string()))?;
                let e = key
                    .e
                    .as_ref()
                    .ok_or_else(|| AuthError::InvalidToken("missing e parameter".to_string()))?;

                Ok(Some(
                    DecodingKey::from_rsa_components(n, e).map_err(|e| {
                        AuthError::InvalidToken(format!("invalid RSA key: {e}"))
                    })?,
                ))
            }
            other => {
                tracing::warn!(kty = other, "Unknown key type");
                Ok(None)
            }
        }
    }

    /// Force a refresh of the JWKS cache.
    ///
    /// This is useful when a key rotation is detected.
    ///
    /// # Errors
    ///
    /// Returns an error if the JWKS fetch fails.
    pub async fn force_refresh(&self) -> Result<()> {
        self.refresh_keys().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ed25519_key() {
        let key = JwkKey {
            kty: "OKP".to_string(),
            crv: Some("Ed25519".to_string()),
            x: Some("11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo".to_string()),
            n: None,
            e: None,
            kid: Some("test-key".to_string()),
            key_use: Some("sig".to_string()),
            alg: Some("EdDSA".to_string()),
        };

        let result = JwksProvider::parse_key(&key).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn parse_rsa_key() {
        let key = JwkKey {
            kty: "RSA".to_string(),
            crv: None,
            x: None,
            n: Some("0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw".to_string()),
            e: Some("AQAB".to_string()),
            kid: Some("rsa-test-key".to_string()),
            key_use: Some("sig".to_string()),
            alg: Some("RS256".to_string()),
        };

        let result = JwksProvider::parse_key(&key).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn skip_unsupported_curve() {
        let key = JwkKey {
            kty: "OKP".to_string(),
            crv: Some("X25519".to_string()),
            x: Some("somekey".to_string()),
            n: None,
            e: None,
            kid: Some("test-key".to_string()),
            key_use: None,
            alg: None,
        };

        let result = JwksProvider::parse_key(&key).unwrap();
        assert!(result.is_none());
    }
}

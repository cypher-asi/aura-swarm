//! Trustee KBS client for the per-agent state DEK lifecycle.
//!
//! The control plane provisions a random 256-bit data encryption key (DEK)
//! in the Trustee KBS when a confidential agent is created, and deletes it
//! when the agent is destroyed (crypto-erase: once the DEK is gone, the
//! sealed state ciphertext on EFS is unrecoverable).
//!
//! The DEK itself never leaves this module except as the body of the
//! resource-registration request to the KBS; it is never logged and never
//! persisted by the control plane. The harness later fetches it from the
//! KBS after successful attestation (a different release phase).
//!
//! # Admin authentication
//!
//! Trustee's admin API (resource registration/deletion) authenticates with
//! a compact `EdDSA` JSON Web Token signed by the KBS admin Ed25519 private
//! key (the public half is mounted into the KBS as the
//! `kbs-auth-public-key` secret; see `deploy/08-deploy-k8s.sh`). The token
//! is sent as `Authorization: Bearer <jwt>` and carries only freshness
//! claims (`exp`/`iat`/`nbf`), matching what `kbs-client` produces.
//!
//! # Key id → resource path mapping
//!
//! KBS resource paths have exactly three segments
//! (`/kbs/v0/resource/{repository}/{type}/{tag}`), while the deterministic
//! per-agent key id has four (`swarm/agents/{agent_id}/state-key`). The
//! mapping used here (and which the harness must mirror when fetching):
//! first segment → repository, last segment → tag, middle segments joined
//! with `.` → type. So `swarm/agents/{agent_id}/state-key` becomes
//! `swarm/agents.{agent_id}/state-key`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Serialize;
use zeroize::Zeroizing;

use crate::error::{ControlError, Result};

/// Configuration for the Trustee KBS integration.
#[derive(Debug, Clone)]
pub struct KbsConfig {
    /// URL of the Trustee KBS.
    pub url: String,
    /// Path to the KBS admin Ed25519 private key (PKCS#8 PEM, as generated
    /// by the deploy scripts into `.secrets/kbs-admin.key`).
    pub admin_key_path: String,
    /// Whether the KBS DEK lifecycle is enabled. When disabled (dev mode),
    /// the no-op client is used and no DEKs are provisioned.
    pub enabled: bool,
}

impl Default for KbsConfig {
    fn default() -> Self {
        Self {
            url: "http://kbs.swarm-system.svc.cluster.local:8080".to_string(),
            admin_key_path: String::new(),
            enabled: true,
        }
    }
}

impl KbsConfig {
    /// Load configuration from environment variables.
    ///
    /// - `KBS_URL`: URL of the Trustee KBS
    /// - `KBS_ADMIN_KEY_PATH`: path to the admin Ed25519 private key (PEM)
    /// - `KBS_ENABLED`: enable/disable the DEK lifecycle (default true)
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(val) = std::env::var("KBS_URL") {
            config.url = val;
        }
        if let Ok(val) = std::env::var("KBS_ADMIN_KEY_PATH") {
            config.admin_key_path = val;
        }
        if let Ok(val) = std::env::var("KBS_ENABLED") {
            config.enabled = val.parse().unwrap_or(true);
        }

        config
    }

    /// Check if the KBS integration is properly configured.
    ///
    /// Without an admin key the control plane cannot authenticate to the
    /// KBS admin API, so the integration is considered unconfigured (dev
    /// mode → no-op client).
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.enabled && !self.url.is_empty() && !self.admin_key_path.is_empty()
    }
}

/// Trait for KBS communication (per-agent state DEK lifecycle).
///
/// This trait abstracts the KBS client interface, allowing for mock
/// implementations in tests and a no-op implementation in dev mode.
#[async_trait]
pub trait KbsClient: Send + Sync {
    /// Generate a fresh random 256-bit DEK and register it in the KBS
    /// under the given key id (e.g. `swarm/agents/{agent_id}/state-key`).
    ///
    /// The DEK is generated inside the client and never returned to the
    /// caller, logged, or persisted by the control plane.
    ///
    /// **Warning:** Trustee's resource registration is an unconditional
    /// overwrite — it cannot report "already exists". Callers that must
    /// not clobber an existing DEK (overwriting one bricks the agent's
    /// sealed state) have to use [`KbsClient::dek_exists`] first for
    /// put-if-absent semantics; see the R2 DEK backfill in `service.rs`.
    ///
    /// # Errors
    ///
    /// Returns an error if key generation or the KBS request fails.
    async fn provision_dek(&self, key_id: &str) -> Result<()>;

    /// Check whether a DEK is already registered under `key_id`.
    ///
    /// This exists because the KBS resource POST cannot report
    /// "exists" (see [`KbsClient::provision_dek`]): provision-if-absent
    /// is implemented as GET-first. Returns `Ok(true)` when the
    /// resource is present, `Ok(false)` when the KBS definitively
    /// reports it absent (404).
    ///
    /// # Errors
    ///
    /// Any response that does not prove presence or absence (auth
    /// failures, 5xx, transport errors) is an error — callers must
    /// treat "unknown" as "do not provision" so an existing DEK is
    /// never overwritten.
    async fn dek_exists(&self, key_id: &str) -> Result<bool>;

    /// Delete the DEK for the given key id from the KBS (crypto-erase).
    ///
    /// Idempotent: a missing resource (404) is treated as success.
    ///
    /// # Errors
    ///
    /// Returns an error if the KBS request fails (other than 404).
    async fn revoke_dek(&self, key_id: &str) -> Result<()>;
}

/// Freshness-only claims for the KBS admin token.
#[derive(Serialize)]
struct AdminClaims {
    exp: u64,
    iat: u64,
    nbf: u64,
}

/// HTTP client for the Trustee KBS resource admin API.
///
/// Note: deliberately no `Debug` impl — the struct holds the admin signing
/// key, and DEK material transits through its methods.
pub struct HttpKbsClient {
    client: reqwest::Client,
    base_url: String,
    admin_key: jsonwebtoken::EncodingKey,
}

/// Lifetime of a freshly minted admin token.
const ADMIN_TOKEN_TTL_SECS: u64 = 300;

impl HttpKbsClient {
    /// Create a new KBS client from configuration, loading the admin
    /// Ed25519 private key from `config.admin_key_path`.
    ///
    /// # Errors
    ///
    /// Returns `ControlError::Internal` if the key file cannot be read or
    /// parsed, or if the HTTP client cannot be created.
    pub fn new(config: &KbsConfig) -> Result<Self> {
        let pem = std::fs::read(&config.admin_key_path).map_err(|e| {
            ControlError::Internal(format!(
                "failed to read KBS admin key at {}: {e}",
                config.admin_key_path
            ))
        })?;
        let admin_key = jsonwebtoken::EncodingKey::from_ed_pem(&pem)
            .map_err(|e| ControlError::Internal(format!("invalid KBS admin key: {e}")))?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| ControlError::Internal(format!("failed to create HTTP client: {e}")))?;

        Ok(Self {
            client,
            base_url: config.url.trim_end_matches('/').to_string(),
            admin_key,
        })
    }

    /// Get the base URL of the KBS.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Build the full resource URL for a key id.
    fn resource_url(&self, key_id: &str) -> Result<String> {
        let (repository, rtype, tag) = kbs_resource_path(key_id)?;
        Ok(format!(
            "{}/kbs/v0/resource/{repository}/{rtype}/{tag}",
            self.base_url
        ))
    }

    /// Mint a short-lived `EdDSA` admin token.
    fn admin_token(&self) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| ControlError::Internal(format!("system clock before epoch: {e}")))?
            .as_secs();
        let claims = AdminClaims {
            exp: now + ADMIN_TOKEN_TTL_SECS,
            iat: now,
            nbf: now,
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
        jsonwebtoken::encode(&header, &claims, &self.admin_key)
            .map_err(|e| ControlError::Internal(format!("failed to sign KBS admin token: {e}")))
    }
}

#[async_trait]
impl KbsClient for HttpKbsClient {
    async fn provision_dek(&self, key_id: &str) -> Result<()> {
        let url = self.resource_url(key_id)?;
        let token = self.admin_token()?;

        // 256-bit DEK from the OS CSPRNG. Our copy is zeroized on drop;
        // the transport copy handed to reqwest is dropped after the
        // request body is sent. The DEK is never logged.
        let mut dek = Zeroizing::new([0u8; 32]);
        getrandom::fill(dek.as_mut())
            .map_err(|e| ControlError::Internal(format!("failed to generate DEK: {e}")))?;

        let response = self
            .client
            .post(&url)
            .bearer_auth(&token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(dek.to_vec())
            .send()
            .await
            .map_err(|e| ControlError::Internal(format!("KBS request failed: {e}")))?;

        if response.status().is_success() {
            tracing::info!(key_id = %key_id, "Provisioned state DEK in KBS");
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            tracing::error!(
                key_id = %key_id,
                status = %status,
                error = %body,
                "Failed to provision state DEK in KBS"
            );
            Err(ControlError::Internal(format!(
                "KBS resource registration failed with status {status}: {body}"
            )))
        }
    }

    async fn dek_exists(&self, key_id: &str) -> Result<bool> {
        let url = self.resource_url(key_id)?;
        let token = self.admin_token()?;

        let response = self
            .client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| ControlError::Internal(format!("KBS request failed: {e}")))?;

        let status = response.status();
        if status.is_success() {
            // The body may contain the DEK; drop it without reading.
            Ok(true)
        } else if status == reqwest::StatusCode::NOT_FOUND {
            Ok(false)
        } else {
            // 401/403 here usually means the KBS resource-read policy
            // does not admit the admin token. We cannot distinguish
            // "absent" from "present but unreadable", so surface an
            // error and let the caller skip rather than risk an
            // overwrite.
            let body = response.text().await.unwrap_or_default();
            Err(ControlError::Internal(format!(
                "KBS existence check for {key_id} returned {status}: {body}"
            )))
        }
    }

    async fn revoke_dek(&self, key_id: &str) -> Result<()> {
        let url = self.resource_url(key_id)?;
        let token = self.admin_token()?;

        let response = self
            .client
            .delete(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| ControlError::Internal(format!("KBS request failed: {e}")))?;

        let status = response.status();
        if status.is_success() {
            tracing::info!(key_id = %key_id, "Revoked state DEK in KBS (crypto-erase)");
            Ok(())
        } else if status == reqwest::StatusCode::NOT_FOUND {
            // Idempotent: already gone (e.g. retried destroy).
            tracing::debug!(key_id = %key_id, "State DEK already absent from KBS");
            Ok(())
        } else {
            let body = response.text().await.unwrap_or_default();
            tracing::error!(
                key_id = %key_id,
                status = %status,
                error = %body,
                "Failed to revoke state DEK in KBS"
            );
            Err(ControlError::Internal(format!(
                "KBS resource deletion failed with status {status}: {body}"
            )))
        }
    }
}

/// Map a key id to the three-segment KBS resource path
/// `(repository, type, tag)`.
///
/// First segment → repository, last segment → tag, middle segments joined
/// with `.` → type (see module docs). The harness uses the same mapping
/// when fetching the DEK post-attestation.
///
/// # Errors
///
/// Returns `ControlError::Internal` if the key id has fewer than three
/// segments or any empty segment.
pub fn kbs_resource_path(key_id: &str) -> Result<(String, String, String)> {
    let segments: Vec<&str> = key_id.split('/').collect();
    if segments.len() < 3 || segments.iter().any(|s| s.is_empty()) {
        return Err(ControlError::Internal(format!(
            "invalid KBS key id (expected at least repository/type/tag): {key_id}"
        )));
    }
    let repository = segments[0].to_string();
    let tag = segments[segments.len() - 1].to_string();
    let rtype = segments[1..segments.len() - 1].join(".");
    Ok((repository, rtype, tag))
}

/// A no-op KBS client for dev mode and tests.
///
/// Logs operations without contacting a KBS; no DEKs are provisioned, so
/// agents created with this client cannot complete a real attestation boot.
#[derive(Debug, Clone, Default)]
pub struct NoopKbsClient;

impl NoopKbsClient {
    /// Create a new no-op KBS client.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl KbsClient for NoopKbsClient {
    async fn provision_dek(&self, key_id: &str) -> Result<()> {
        tracing::warn!(
            key_id = %key_id,
            "NoopKbsClient: provision_dek called but no KBS configured"
        );
        Ok(())
    }

    async fn dek_exists(&self, key_id: &str) -> Result<bool> {
        // Dev mode: report "present" so the R2 DEK backfill is a no-op
        // instead of warn-spamming a provision per sealed agent.
        tracing::debug!(
            key_id = %key_id,
            "NoopKbsClient: dek_exists called but no KBS configured; reporting present"
        );
        Ok(true)
    }

    async fn revoke_dek(&self, key_id: &str) -> Result<()> {
        tracing::warn!(
            key_id = %key_id,
            "NoopKbsClient: revoke_dek called but no KBS configured"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// RFC 8410 example Ed25519 private key — test-only, never deployed.
    const TEST_ED25519_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n\
-----END PRIVATE KEY-----\n";

    fn test_client(base_url: &str) -> HttpKbsClient {
        let mut key_file = tempfile::NamedTempFile::new().unwrap();
        key_file.write_all(TEST_ED25519_PEM.as_bytes()).unwrap();
        let config = KbsConfig {
            url: base_url.to_string(),
            admin_key_path: key_file.path().to_string_lossy().into_owned(),
            enabled: true,
        };
        let client = HttpKbsClient::new(&config).unwrap();
        key_file.close().unwrap();
        client
    }

    #[test]
    fn default_config() {
        let config = KbsConfig::default();
        assert_eq!(config.url, "http://kbs.swarm-system.svc.cluster.local:8080");
        assert!(config.admin_key_path.is_empty());
        assert!(config.enabled);
    }

    #[test]
    fn is_configured_requires_admin_key() {
        let mut config = KbsConfig::default();
        assert!(!config.is_configured());

        config.admin_key_path = "/secrets/kbs-admin.key".to_string();
        assert!(config.is_configured());

        config.enabled = false;
        assert!(!config.is_configured());
    }

    #[test]
    fn resource_path_maps_agent_state_key() {
        let (repo, rtype, tag) =
            kbs_resource_path("swarm/agents/0123abcd/state-key").unwrap();
        assert_eq!(repo, "swarm");
        assert_eq!(rtype, "agents.0123abcd");
        assert_eq!(tag, "state-key");
    }

    #[test]
    fn resource_path_three_segments_is_identity() {
        let (repo, rtype, tag) = kbs_resource_path("repo/type/tag").unwrap();
        assert_eq!((repo.as_str(), rtype.as_str(), tag.as_str()), ("repo", "type", "tag"));
    }

    #[test]
    fn resource_path_rejects_short_or_empty_segments() {
        assert!(kbs_resource_path("only/two").is_err());
        assert!(kbs_resource_path("a//b/c").is_err());
        assert!(kbs_resource_path("").is_err());
    }

    #[test]
    fn http_client_loads_key_and_signs_token() {
        let client = test_client("http://kbs.example:8080/");
        assert_eq!(client.base_url(), "http://kbs.example:8080");

        let token = client.admin_token().unwrap();
        // Compact JWT: header.payload.signature
        assert_eq!(token.split('.').count(), 3);
    }

    #[test]
    fn http_client_missing_key_file_fails() {
        let config = KbsConfig {
            url: "http://kbs.example:8080".to_string(),
            admin_key_path: "Z:/does/not/exist/kbs-admin.key".to_string(),
            enabled: true,
        };
        assert!(HttpKbsClient::new(&config).is_err());
    }

    #[tokio::test]
    async fn noop_client_is_ok() {
        let client = NoopKbsClient::new();
        client.provision_dek("swarm/agents/x/state-key").await.unwrap();
        client.revoke_dek("swarm/agents/x/state-key").await.unwrap();
        // Reports "present" so the R2 backfill is a dev-mode no-op.
        assert!(client.dek_exists("swarm/agents/x/state-key").await.unwrap());
    }

    #[tokio::test]
    async fn provision_posts_dek_with_bearer_auth() {
        use wiremock::matchers::{header_exists, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/kbs/v0/resource/swarm/agents.abc123/state-key"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        client
            .provision_dek("swarm/agents/abc123/state-key")
            .await
            .unwrap();

        // The request body must be a 32-byte (256-bit) DEK.
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body.len(), 32);
    }

    #[tokio::test]
    async fn provision_surfaces_kbs_error() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let err = client
            .provision_dek("swarm/agents/abc123/state-key")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn dek_exists_maps_200_404_and_errors() {
        use wiremock::matchers::{header_exists, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/kbs/v0/resource/swarm/agents.present/state-key"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 32]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/kbs/v0/resource/swarm/agents.absent/state-key"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/kbs/v0/resource/swarm/agents.denied/state-key"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        assert!(client
            .dek_exists("swarm/agents/present/state-key")
            .await
            .unwrap());
        assert!(!client
            .dek_exists("swarm/agents/absent/state-key")
            .await
            .unwrap());
        // "Unknown" must be an error so callers never provision blind.
        let err = client
            .dek_exists("swarm/agents/denied/state-key")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn revoke_tolerates_404() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/kbs/v0/resource/swarm/agents.abc123/state-key"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        client
            .revoke_dek("swarm/agents/abc123/state-key")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn revoke_surfaces_other_errors() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client = test_client(&server.uri());
        let err = client
            .revoke_dek("swarm/agents/abc123/state-key")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }
}

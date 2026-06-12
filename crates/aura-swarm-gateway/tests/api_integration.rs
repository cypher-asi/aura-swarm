//! Gateway API integration tests.
//!
//! These tests exercise the full HTTP stack: router, middleware, auth extraction,
//! handler logic, request/response serialization, and error mapping. They use
//! the real `ControlPlaneService` with an in-memory RocksDB instance and the
//! `MockJwtValidator` from the auth crate.

use std::sync::Arc;

use axum::http::{HeaderName, HeaderValue};
use axum::Router;
use axum_test::TestServer;
use serde_json::{json, Value};

use aura_swarm_auth::MockJwtValidator;
use aura_swarm_control::ControlPlaneService;
use aura_swarm_gateway::{create_router, GatewayConfig, GatewayState};
use aura_swarm_store::RocksStore;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

const TEST_USER_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";
const OTHER_USER_UUID: &str = "660e8400-e29b-41d4-a716-446655440099";
const TEST_INTERNAL_TOKEN: &str = "test-internal-token";

fn test_token(user_uuid: &str) -> String {
    format!("test-token:{user_uuid}")
}

fn auth_header(user_uuid: &str) -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(&format!("Bearer {}", test_token(user_uuid))).unwrap(),
    )
}

fn internal_auth_header() -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(&format!("Bearer {TEST_INTERNAL_TOKEN}")).unwrap(),
    )
}

fn build_test_app() -> (TestServer, tempfile::TempDir) {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(RocksStore::open(tmpdir.path()).unwrap());
    let control = Arc::new(ControlPlaneService::with_defaults(store));
    let jwt = Arc::new(MockJwtValidator);
    let config = GatewayConfig {
        internal_token: Some(TEST_INTERNAL_TOKEN.to_string()),
        ..GatewayConfig::default()
    };
    let state = GatewayState::new(control, jwt, config);
    let app: Router = create_router(state);
    let server = TestServer::new(app).unwrap();
    (server, tmpdir)
}

// ---------------------------------------------------------------------------
// Health endpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_returns_200_with_version() {
    let (server, _tmp) = build_test_app();
    let resp = server.get("/health").await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["status"], "healthy");
    assert!(body.get("version").is_some());
}

#[tokio::test]
async fn internal_health_returns_200() {
    let (server, _tmp) = build_test_app();
    let (internal_hdr, internal_val) = internal_auth_header();
    let resp = server
        .get("/internal/health")
        .add_header(internal_hdr, internal_val)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn internal_health_without_token_returns_401() {
    let (server, _tmp) = build_test_app();
    let resp = server.get("/internal/health").await;
    resp.assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn internal_agent_lists_require_token() {
    let (server, _tmp) = build_test_app();

    server
        .get("/internal/agents/active")
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
    server
        .get("/internal/agents/all")
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn internal_agent_lists_accept_internal_token() {
    let (server, _tmp) = build_test_app();
    let (internal_hdr, internal_val) = internal_auth_header();

    let resp = server
        .get("/internal/agents/all")
        .add_header(internal_hdr.clone(), internal_val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body, json!([]));
}

// ---------------------------------------------------------------------------
// Auth middleware
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_auth_header_returns_401() {
    let (server, _tmp) = build_test_app();
    let resp = server.get("/v1/agents").await;
    resp.assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn invalid_bearer_token_returns_401() {
    let (server, _tmp) = build_test_app();
    let resp = server
        .get("/v1/agents")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer bad-token"),
        )
        .await;
    resp.assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn malformed_auth_header_returns_401() {
    let (server, _tmp) = build_test_app();
    let resp = server
        .get("/v1/agents")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("NotBearer xyz"),
        )
        .await;
    resp.assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Agent CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_agents_empty() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let resp = server
        .get("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["agents"], json!([]));
}

#[tokio::test]
async fn create_agent_success() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "test-agent"}))
        .await;
    resp.assert_status(axum::http::StatusCode::CREATED);
    let body: Value = resp.json();
    assert_eq!(body["name"], "test-agent");
    assert!(body.get("agent_id").is_some());
    assert_eq!(body["status"], "provisioning");
}

#[tokio::test]
async fn create_agent_empty_name() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": ""}))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_agent_name_too_long() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let long_name = "a".repeat(65);
    let resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": long_name}))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_agent_invalid_name_chars() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "bad name!"}))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_and_list_agents() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "agent-1"}))
        .await
        .assert_status(axum::http::StatusCode::CREATED);

    server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "agent-2"}))
        .await
        .assert_status(axum::http::StatusCode::CREATED);

    let resp = server
        .get("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["agents"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn get_agent_success() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "my-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = server
        .get(&format!("/v1/agents/{agent_id}"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["name"], "my-agent");
    assert_eq!(body["agent_id"], agent_id);
}

#[tokio::test]
async fn get_agent_not_found() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let fake_id = "aa".repeat(16);
    let resp = server
        .get(&format!("/v1/agents/{fake_id}"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_agent_invalid_id() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let resp = server
        .get("/v1/agents/not-valid-hex")
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_agent_not_owner() {
    let (server, _tmp) = build_test_app();

    let (hdr1, val1) = auth_header(TEST_USER_UUID);
    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr1.clone(), val1.clone())
        .json(&json!({"name": "private-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (hdr2, val2) = auth_header(OTHER_USER_UUID);
    let resp = server
        .get(&format!("/v1/agents/{agent_id}"))
        .add_header(hdr2.clone(), val2.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn delete_agent_requires_stopped() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "agent-to-delete"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Agent is in Provisioning state, delete should fail
    let resp = server
        .delete(&format!("/v1/agents/{agent_id}"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::CONFLICT);
}

#[tokio::test]
async fn delete_agent_success_when_stopped() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "agent-to-delete"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Drive the agent to a terminal Stopped state via the real lifecycle. The
    // scheduler can no longer force an arbitrary state straight to Stopped (a
    // pod disappearing must not terminalize a logically-active agent), so we go
    // Provisioning -> Running -> Stopping (user stop) -> Stopped (scheduler).
    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "running"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    server
        .post(&format!("/v1/agents/{agent_id}/stop"))
        .add_header(hdr.clone(), val.clone())
        .await
        .assert_status_ok();

    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "stopped"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    let resp = server
        .delete(&format!("/v1/agents/{agent_id}"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// Idempotent Create / ID Parity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_agent_with_supplied_id_returns_201() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let agent_id = "aa".repeat(16); // valid hex
    let resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "parity-agent", "agent_id": agent_id}))
        .await;
    resp.assert_status(axum::http::StatusCode::CREATED);
    let body: Value = resp.json();
    assert!(body["agent_id"].as_str().is_some());
    assert_eq!(body["name"], "parity-agent");
}

#[tokio::test]
async fn create_agent_idempotent_same_user_returns_200() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let agent_id = "bb".repeat(16);

    // First create → 201
    let resp1 = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "idem-agent", "agent_id": agent_id}))
        .await;
    resp1.assert_status(axum::http::StatusCode::CREATED);
    let body1: Value = resp1.json();
    let created_agent_id = body1["agent_id"].as_str().unwrap().to_string();
    let created_at = body1["created_at"].as_str().unwrap().to_string();

    // Second create with same ID + same user → 200 (idempotent)
    let resp2 = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "idem-agent", "agent_id": agent_id}))
        .await;
    resp2.assert_status(axum::http::StatusCode::OK);
    let body2: Value = resp2.json();
    assert_eq!(body2["agent_id"], created_agent_id);
    assert_eq!(
        body2["created_at"].as_str().unwrap(),
        created_at,
        "idempotent return should have the same created_at"
    );
}

#[tokio::test]
async fn create_agent_conflict_different_user_returns_409() {
    let (server, _tmp) = build_test_app();

    let agent_id = "cc".repeat(16);

    // User 1 creates the agent
    let (hdr1, val1) = auth_header(TEST_USER_UUID);
    let resp1 = server
        .post("/v1/agents")
        .add_header(hdr1.clone(), val1.clone())
        .json(&json!({"name": "owned-agent", "agent_id": agent_id}))
        .await;
    resp1.assert_status(axum::http::StatusCode::CREATED);

    // User 2 tries to create an agent with the same ID → 409
    let (hdr2, val2) = auth_header(OTHER_USER_UUID);
    let resp2 = server
        .post("/v1/agents")
        .add_header(hdr2.clone(), val2.clone())
        .json(&json!({"name": "stolen-agent", "agent_id": agent_id}))
        .await;
    resp2.assert_status(axum::http::StatusCode::CONFLICT);
    let body: Value = resp2.json();
    assert_eq!(body["error"]["code"], "conflict");
}

#[tokio::test]
async fn create_agent_idempotent_does_not_duplicate_in_list() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let agent_id = "dd".repeat(16);

    // Create twice with same ID
    server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "dup-agent", "agent_id": agent_id}))
        .await
        .assert_status(axum::http::StatusCode::CREATED);

    server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "dup-agent", "agent_id": agent_id}))
        .await
        .assert_status(axum::http::StatusCode::OK);

    // List should have exactly one agent
    let resp = server
        .get("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["agents"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Agent Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_start_stop() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "lifecycle-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Transition to Running via internal API (simulates scheduler callback)
    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "running"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    // Stop should succeed from Running
    let resp = server
        .post(&format!("/v1/agents/{agent_id}/stop"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["status"], "stopping");
}

#[tokio::test]
async fn lifecycle_hibernate_wake() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "hibernate-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Move to Running, then Idle (so hibernate is valid)
    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "idle"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    // Hibernate from Idle
    let resp = server
        .post(&format!("/v1/agents/{agent_id}/hibernate"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["status"], "hibernating");

    // Wake from Hibernating
    let resp = server
        .post(&format!("/v1/agents/{agent_id}/wake"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["status"], "provisioning");
}

#[tokio::test]
async fn lifecycle_invalid_transition_returns_409() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "invalid-transition"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Agent is Provisioning, hibernate is not valid
    let resp = server
        .post(&format!("/v1/agents/{agent_id}/hibernate"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::CONFLICT);
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_session_success() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "session-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Move agent to Running
    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "running"}))
        .await;

    let resp = server
        .post(&format!("/v1/agents/{agent_id}/sessions"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::CREATED);
    let body: Value = resp.json();
    assert!(body.get("session_id").is_some());
    // With the migration to the POST /v1/run contract, a created session points
    // clients at the agent's run-creation endpoint rather than a per-session WS.
    let run_url = body["run_url"].as_str().unwrap();
    assert!(run_url.starts_with("/v1/agents/"), "run_url: {run_url}");
    assert!(run_url.ends_with("/run"), "run_url: {run_url}");
}

#[tokio::test]
async fn create_session_with_config() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "config-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "running"}))
        .await;

    let resp = server
        .post(&format!("/v1/agents/{agent_id}/sessions"))
        .add_header(hdr.clone(), val.clone())
        .json(&json!({
            "config": {
                "system_prompt": "You are helpful.",
                "model": "test-model",
                "max_tokens": 4096
            }
        }))
        .await;
    resp.assert_status(axum::http::StatusCode::CREATED);
}

#[tokio::test]
async fn create_session_invalid_agent_id() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let resp = server
        .post("/v1/agents/not-hex/sessions")
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_session_success() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "get-session-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "running"}))
        .await;

    let session_resp = server
        .post(&format!("/v1/agents/{agent_id}/sessions"))
        .add_header(hdr.clone(), val.clone())
        .await;
    let session_id = session_resp.json::<Value>()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = server
        .get(&format!("/v1/sessions/{session_id}"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["session_id"], session_id);
    assert_eq!(body["status"], "active");
}

#[tokio::test]
async fn get_session_not_found() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let fake_uuid = "00000000-0000-0000-0000-000000000099";
    let resp = server
        .get(&format!("/v1/sessions/{fake_uuid}"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_sessions_success() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "list-sessions-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "running"}))
        .await;

    server
        .post(&format!("/v1/agents/{agent_id}/sessions"))
        .add_header(hdr.clone(), val.clone())
        .await
        .assert_status(axum::http::StatusCode::CREATED);

    let resp = server
        .get(&format!("/v1/agents/{agent_id}/sessions"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn close_session_success() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "close-session-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "running"}))
        .await;

    let session_resp = server
        .post(&format!("/v1/agents/{agent_id}/sessions"))
        .add_header(hdr.clone(), val.clone())
        .await;
    let session_id = session_resp.json::<Value>()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = server
        .delete(&format!("/v1/sessions/{session_id}"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// Internal endpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn internal_update_status_valid() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "internal-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "running"}))
        .await;
    resp.assert_status(axum::http::StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn internal_update_status_without_token_returns_401() {
    let (server, _tmp) = build_test_app();
    let fake_id = "aa".repeat(16);
    let resp = server
        .patch(&format!("/internal/agents/{fake_id}/status"))
        .json(&json!({"status": "running"}))
        .await;
    resp.assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn internal_update_status_invalid_id() {
    let (server, _tmp) = build_test_app();
    let (internal_hdr, internal_val) = internal_auth_header();
    let resp = server
        .patch("/internal/agents/not-hex/status")
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "running"}))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn internal_update_status_not_found() {
    let (server, _tmp) = build_test_app();
    let (internal_hdr, internal_val) = internal_auth_header();
    let fake_id = "bb".repeat(16);
    let resp = server
        .patch(&format!("/internal/agents/{fake_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "running"}))
        .await;
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn internal_update_status_with_error_message() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "error-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "error", "message": "pod crashed"}))
        .await;
    resp.assert_status(axum::http::StatusCode::NO_CONTENT);

    // Verify the error message is stored
    let get_resp = server
        .get(&format!("/v1/agents/{agent_id}"))
        .add_header(hdr.clone(), val.clone())
        .await;
    let body: Value = get_resp.json();
    assert_eq!(body["status"], "error");
    assert_eq!(body["error_message"], "pod crashed");
}

// ---------------------------------------------------------------------------
// Agent observability (placeholders)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_logs_returns_empty() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "logs-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = server
        .get(&format!("/v1/agents/{agent_id}/logs"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["logs"], json!([]));
}

#[tokio::test]
async fn get_status_returns_placeholder() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "status-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = server
        .get(&format!("/v1/agents/{agent_id}/status"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["status"], "provisioning");
    assert!(body.get("uptime_seconds").is_some());
    assert!(body.get("resource_usage").is_some());
}

// ---------------------------------------------------------------------------
// Agent state endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_agent_state_returns_lifecycle_state() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "state-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = server
        .get(&format!("/v1/agents/{agent_id}/state"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["state"], "provisioning");
    assert_eq!(body["uptime_seconds"], 0);
    assert_eq!(body["active_sessions"], 0);
}

#[tokio::test]
async fn get_agent_state_running_has_uptime() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "running-state-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "running"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    let resp = server
        .get(&format!("/v1/agents/{agent_id}/state"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["state"], "running");
    assert!(body["uptime_seconds"].as_u64().is_some());
}

#[tokio::test]
async fn get_agent_state_includes_error_message() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "error-state-agent"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "error", "message": "OOM killed"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    let resp = server
        .get(&format!("/v1/agents/{agent_id}/state"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["state"], "error");
    assert_eq!(body["error_message"], "OOM killed");
}

// ---------------------------------------------------------------------------
// Error response format
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_response_has_correct_format() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let resp = server
        .get("/v1/agents/bad-id")
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    let body: Value = resp.json();
    assert!(body.get("error").is_some());
    assert_eq!(body["error"]["code"], "bad_request");
    assert!(body["error"]["message"].as_str().is_some());
}

#[tokio::test]
async fn not_found_error_format() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let fake_id = "cc".repeat(16);

    let resp = server
        .get(&format!("/v1/agents/{fake_id}"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
    let body: Value = resp.json();
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn forbidden_error_format() {
    let (server, _tmp) = build_test_app();
    let (hdr1, val1) = auth_header(TEST_USER_UUID);

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr1.clone(), val1.clone())
        .json(&json!({"name": "private"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (hdr2, val2) = auth_header(OTHER_USER_UUID);
    let resp = server
        .get(&format!("/v1/agents/{agent_id}"))
        .add_header(hdr2.clone(), val2.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
    let body: Value = resp.json();
    assert_eq!(body["error"]["code"], "forbidden");
}

// ---------------------------------------------------------------------------
// Secrets pass-through (Swarm TEE phase 6)
// ---------------------------------------------------------------------------

/// Create an agent owned by `TEST_USER_UUID` and park it in a
/// non-active state (`hibernating` via the internal status callback) so
/// endpoint resolution deterministically yields "no pod" without any
/// network calls.
async fn create_hibernating_agent(server: &TestServer) -> String {
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "vault-agent"}))
        .await;
    create_resp.assert_status(axum::http::StatusCode::CREATED);
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (internal_hdr, internal_val) = internal_auth_header();
    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr, internal_val)
        .json(&json!({"status": "hibernating"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    agent_id
}

#[tokio::test]
async fn secrets_routes_require_auth() {
    let (server, _tmp) = build_test_app();
    let fake_id = "dd".repeat(16);

    server
        .get(&format!("/v1/agents/{fake_id}/secrets"))
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
    server
        .get(&format!("/v1/agents/{fake_id}/secrets/api-key"))
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
    server
        .put(&format!("/v1/agents/{fake_id}/secrets/api-key"))
        .json(&json!({"value": "v"}))
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
    server
        .delete(&format!("/v1/agents/{fake_id}/secrets/api-key"))
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn secrets_not_owner_forbidden() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_hibernating_agent(&server).await;

    let (hdr2, val2) = auth_header(OTHER_USER_UUID);
    let resp = server
        .get(&format!("/v1/agents/{agent_id}/secrets"))
        .add_header(hdr2.clone(), val2.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);

    let resp = server
        .put(&format!("/v1/agents/{agent_id}/secrets/api-key"))
        .add_header(hdr2.clone(), val2.clone())
        .json(&json!({"value": "stolen"}))
        .await;
    resp.assert_status(axum::http::StatusCode::FORBIDDEN);
}

/// With no running pod the proxy must surface `503 agent_unavailable`
/// (same wake/error semantics as the files proxy: no implicit wake).
#[tokio::test]
async fn secrets_agent_not_running_returns_503() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_hibernating_agent(&server).await;
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let resp = server
        .get(&format!("/v1/agents/{agent_id}/secrets"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = resp.json();
    assert_eq!(body["error"]["code"], "agent_unavailable");

    let resp = server
        .put(&format!("/v1/agents/{agent_id}/secrets/api-key"))
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"value": "v", "description": "d"}))
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);

    let resp = server
        .delete(&format!("/v1/agents/{agent_id}/secrets/api-key"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

/// Secret names are validated before being interpolated into the
/// proxied pod path (path-injection guard).
#[tokio::test]
async fn secrets_invalid_name_rejected() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_hibernating_agent(&server).await;
    let (hdr, val) = auth_header(TEST_USER_UUID);

    // "a b" (URL-encoded space) and an encoded traversal both fail the
    // name charset check with 400 before any endpoint resolution.
    for bad in ["a%20b", "..%2F..%2Fetc"] {
        let resp = server
            .get(&format!("/v1/agents/{agent_id}/secrets/{bad}"))
            .add_header(hdr.clone(), val.clone())
            .await;
        resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let body: Value = resp.json();
        assert_eq!(body["error"]["code"], "bad_request");
    }
}

#[tokio::test]
async fn secrets_invalid_agent_id_rejected() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let resp = server
        .get("/v1/agents/not-hex/secrets")
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Process triggers (Swarm TEE phase 8)
// ---------------------------------------------------------------------------

/// Create an agent owned by `TEST_USER_UUID`, optionally with a fixed
/// agent id, and return the agent id.
async fn create_agent(server: &TestServer, agent_id: Option<&str>) -> String {
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let mut body = json!({"name": "trigger-agent"});
    if let Some(id) = agent_id {
        body["agent_id"] = json!(id);
    }
    let resp = server
        .post("/v1/agents")
        .add_header(hdr, val)
        .json(&body)
        .await;
    resp.json::<Value>()["agent_id"].as_str().unwrap().to_string()
}

fn trigger_set() -> Value {
    json!([
        {"process_id": "p1", "cron": "*/5 * * * *", "enabled": true,
         "next_run_at": "2030-01-01T00:00:00Z"},
        {"process_id": "p2", "cron": "0 0 * * *", "enabled": false}
    ])
}

#[tokio::test]
async fn trigger_registration_requires_internal_token() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;

    // No token → 401.
    server
        .put(&format!("/internal/agents/{agent_id}/process-triggers"))
        .json(&trigger_set())
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
    server
        .delete(&format!("/internal/agents/{agent_id}/process-triggers/p1"))
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);

    // An owner JWT is NOT an internal token → still 401.
    let (hdr, val) = auth_header(TEST_USER_UUID);
    server
        .put(&format!("/internal/agents/{agent_id}/process-triggers"))
        .add_header(hdr, val)
        .json(&trigger_set())
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn trigger_replace_sync_and_owner_read() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;
    let (internal_hdr, internal_val) = internal_auth_header();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    // Register two triggers.
    let resp = server
        .put(&format!("/internal/agents/{agent_id}/process-triggers"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&trigger_set())
        .await;
    resp.assert_status_ok();
    assert_eq!(resp.json::<Value>()["registered"], 2);

    // Owner read sees both, with only metadata fields.
    let resp = server
        .get(&format!("/v1/agents/{agent_id}/process-triggers"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    let triggers = body["triggers"].as_array().unwrap();
    assert_eq!(triggers.len(), 2);
    let p1 = triggers.iter().find(|t| t["process_id"] == "p1").unwrap();
    assert_eq!(p1["cron"], "*/5 * * * *");
    assert_eq!(p1["enabled"], true);
    assert_eq!(p1["next_run_at"], "2030-01-01T00:00:00Z");
    assert!(p1.get("prompt").is_none());

    // Replace with a smaller set: p2 must be unregistered.
    let resp = server
        .put(&format!("/internal/agents/{agent_id}/process-triggers"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!([
            {"process_id": "p1", "cron": "*/10 * * * *", "enabled": true}
        ]))
        .await;
    resp.assert_status_ok();

    let resp = server
        .get(&format!("/v1/agents/{agent_id}/process-triggers"))
        .add_header(hdr.clone(), val.clone())
        .await;
    let body: Value = resp.json();
    let triggers = body["triggers"].as_array().unwrap();
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0]["process_id"], "p1");
    assert_eq!(triggers[0]["cron"], "*/10 * * * *");
}

/// Extra fields beyond the allowed metadata (e.g. a prompt) are
/// stripped at the trust boundary, never stored or echoed back.
#[tokio::test]
async fn trigger_registration_strips_unexpected_fields() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;
    let (internal_hdr, internal_val) = internal_auth_header();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    server
        .put(&format!("/internal/agents/{agent_id}/process-triggers"))
        .add_header(internal_hdr, internal_val)
        .json(&json!([
            {"process_id": "p1", "cron": "*/5 * * * *", "enabled": true,
             "prompt": "IN-TEE-SECRET", "config": {"k": "v"}, "last_run_at": "2020-01-01T00:00:00Z"}
        ]))
        .await
        .assert_status_ok();

    let resp = server
        .get(&format!("/v1/agents/{agent_id}/process-triggers"))
        .add_header(hdr, val)
        .await;
    let raw = resp.text();
    assert!(!raw.contains("IN-TEE-SECRET"));
    assert!(!raw.contains("prompt"));
    let body: Value = serde_json::from_str(&raw).unwrap();
    // The agent cannot inject gateway-side bookkeeping either.
    assert!(body["triggers"][0].get("last_run_at").is_none());
}

#[tokio::test]
async fn trigger_registration_validates_cron_and_agent() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;
    let (internal_hdr, internal_val) = internal_auth_header();

    // Invalid cron → 400.
    let resp = server
        .put(&format!("/internal/agents/{agent_id}/process-triggers"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!([{"process_id": "p1", "cron": "not a cron", "enabled": true}]))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

    // Invalid process id → 400.
    let resp = server
        .put(&format!("/internal/agents/{agent_id}/process-triggers"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!([{"process_id": "a/../b", "cron": "*/5 * * * *", "enabled": true}]))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);

    // Unknown agent → 404.
    let ghost = "ee".repeat(16);
    let resp = server
        .put(&format!("/internal/agents/{ghost}/process-triggers"))
        .add_header(internal_hdr, internal_val)
        .json(&trigger_set())
        .await;
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn trigger_internal_delete_single() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;
    let (internal_hdr, internal_val) = internal_auth_header();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    server
        .put(&format!("/internal/agents/{agent_id}/process-triggers"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&trigger_set())
        .await
        .assert_status_ok();

    server
        .delete(&format!("/internal/agents/{agent_id}/process-triggers/p2"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    // Deleting again → 404.
    server
        .delete(&format!("/internal/agents/{agent_id}/process-triggers/p2"))
        .add_header(internal_hdr, internal_val)
        .await
        .assert_status(axum::http::StatusCode::NOT_FOUND);

    let resp = server
        .get(&format!("/v1/agents/{agent_id}/process-triggers"))
        .add_header(hdr, val)
        .await;
    assert_eq!(resp.json::<Value>()["triggers"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn trigger_owner_read_requires_auth_and_ownership() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;

    // No token → 401.
    server
        .get(&format!("/v1/agents/{agent_id}/process-triggers"))
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);

    // Different user → 403.
    let (hdr2, val2) = auth_header(OTHER_USER_UUID);
    server
        .get(&format!("/v1/agents/{agent_id}/process-triggers"))
        .add_header(hdr2, val2)
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);
}

/// Destroying an agent must unregister its triggers: recreate an agent
/// under the same (caller-supplied) id and verify the registered set
/// is empty.
#[tokio::test]
async fn trigger_cleanup_on_agent_destroy() {
    let (server, _tmp) = build_test_app();
    let fixed_id = "ab".repeat(16);
    let agent_id = create_agent(&server, Some(&fixed_id)).await;
    let (internal_hdr, internal_val) = internal_auth_header();
    let (hdr, val) = auth_header(TEST_USER_UUID);

    server
        .put(&format!("/internal/agents/{agent_id}/process-triggers"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&trigger_set())
        .await
        .assert_status_ok();

    // Drive to Stopped through the real lifecycle, then destroy.
    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "running"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    server
        .post(&format!("/v1/agents/{agent_id}/stop"))
        .add_header(hdr.clone(), val.clone())
        .await
        .assert_status_ok();
    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr.clone(), internal_val.clone())
        .json(&json!({"status": "stopped"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    server
        .delete(&format!("/v1/agents/{agent_id}"))
        .add_header(hdr.clone(), val.clone())
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    // Recreate with the same id: no stale triggers may survive.
    let recreated = create_agent(&server, Some(&fixed_id)).await;
    assert_eq!(recreated, agent_id);
    let resp = server
        .get(&format!("/v1/agents/{agent_id}/process-triggers"))
        .add_header(hdr, val)
        .await;
    resp.assert_status_ok();
    assert_eq!(resp.json::<Value>()["triggers"], json!([]));
}

// ---------------------------------------------------------------------------
// Tier changes (Swarm TEE phase 10)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn change_tier_requires_auth() {
    let (server, _tmp) = build_test_app();
    let fake_id = "aa".repeat(16);
    server
        .post(&format!("/v1/agents/{fake_id}/tier"))
        .json(&json!({"tier": "pro"}))
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn change_tier_not_owner_forbidden() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;

    let (hdr2, val2) = auth_header(OTHER_USER_UUID);
    server
        .post(&format!("/v1/agents/{agent_id}/tier"))
        .add_header(hdr2, val2)
        .json(&json!({"tier": "pro"}))
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn change_tier_invalid_inputs() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;
    let (hdr, val) = auth_header(TEST_USER_UUID);

    // Unknown tier name → 400.
    let resp = server
        .post(&format!("/v1/agents/{agent_id}/tier"))
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"tier": "mega"}))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(resp.json::<Value>()["error"]["code"], "bad_request");

    // Invalid agent id → 400.
    server
        .post("/v1/agents/not-hex/tier")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"tier": "pro"}))
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);

    // Unknown agent → 404.
    let ghost = "ef".repeat(16);
    server
        .post(&format!("/v1/agents/{ghost}/tier"))
        .add_header(hdr, val)
        .json(&json!({"tier": "pro"}))
        .await
        .assert_status(axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn change_tier_same_tier_noop_returns_200() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr, internal_val)
        .json(&json!({"status": "running"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    // Default create is "standard": a standard→standard request is a no-op.
    let resp = server
        .post(&format!("/v1/agents/{agent_id}/tier"))
        .add_header(hdr, val)
        .json(&json!({"tier": "standard"}))
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["changed"], false);
    assert_eq!(body["pod_recreated"], false);
    assert_eq!(body["previous_tier"], "standard");
    assert_eq!(body["tier"], "standard");
    assert_eq!(body["status"], "running");
}

#[tokio::test]
async fn change_tier_hibernating_is_record_only() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_hibernating_agent(&server).await;
    let (hdr, val) = auth_header(TEST_USER_UUID);

    let resp = server
        .post(&format!("/v1/agents/{agent_id}/tier"))
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"tier": "pro"}))
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["changed"], true);
    assert_eq!(body["pod_recreated"], false);
    assert_eq!(body["previous_tier"], "standard");
    assert_eq!(body["tier"], "pro");
    assert_eq!(body["status"], "hibernating");

    // The record carries the new tier; it applies on next wake.
    let resp = server
        .get(&format!("/v1/agents/{agent_id}"))
        .add_header(hdr, val)
        .await;
    let body: Value = resp.json();
    assert_eq!(body["tier"], "pro");
    assert_eq!(body["status"], "hibernating");
    assert_eq!(body["spec"]["cpu_millicores"], 2000);
}

#[tokio::test]
async fn change_tier_running_recreates_pod() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr, internal_val)
        .json(&json!({"status": "running"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    let resp = server
        .post(&format!("/v1/agents/{agent_id}/tier"))
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"tier": "pro"}))
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["changed"], true);
    assert_eq!(body["pod_recreated"], true);
    assert_eq!(body["tier"], "pro");
    assert_eq!(body["status"], "provisioning");

    let resp = server
        .get(&format!("/v1/agents/{agent_id}"))
        .add_header(hdr, val)
        .await;
    let body: Value = resp.json();
    assert_eq!(body["tier"], "pro");
    assert_eq!(body["status"], "provisioning");
}

#[tokio::test]
async fn change_tier_mid_transition_returns_409() {
    let (server, _tmp) = build_test_app();
    // Fresh create leaves the agent in Provisioning.
    let agent_id = create_agent(&server, None).await;
    let (hdr, val) = auth_header(TEST_USER_UUID);

    server
        .post(&format!("/v1/agents/{agent_id}/tier"))
        .add_header(hdr, val)
        .json(&json!({"tier": "pro"}))
        .await
        .assert_status(axum::http::StatusCode::CONFLICT);
}

// ---------------------------------------------------------------------------
// Usage / cost stats (Swarm TEE phase 11)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_routes_require_auth() {
    let (server, _tmp) = build_test_app();
    let fake_id = "ee".repeat(16);

    server
        .get(&format!("/v1/agents/{fake_id}/usage"))
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
    server
        .get("/v1/usage")
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn agent_usage_not_owner_forbidden() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;

    let (hdr, val) = auth_header(OTHER_USER_UUID);
    server
        .get(&format!("/v1/agents/{agent_id}/usage"))
        .add_header(hdr, val)
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn agent_usage_invalid_range_returns_400() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;
    let (hdr, val) = auth_header(TEST_USER_UUID);

    // Unparseable timestamp.
    server
        .get(&format!("/v1/agents/{agent_id}/usage?from=yesterday"))
        .add_header(hdr.clone(), val.clone())
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);

    // from after to.
    server
        .get(&format!(
            "/v1/agents/{agent_id}/usage?from=2026-01-02T00:00:00Z&to=2026-01-01T00:00:00Z"
        ))
        .add_header(hdr, val)
        .await
        .assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn agent_usage_happy_path_reflects_lifecycle() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    // create (PodScheduled) → Running → hibernate (PodTerminated + Hibernated).
    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr, internal_val)
        .json(&json!({"status": "running"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    server
        .post(&format!("/v1/agents/{agent_id}/hibernate"))
        .add_header(hdr.clone(), val.clone())
        .await
        .assert_status_ok();

    let resp = server
        .get(&format!("/v1/agents/{agent_id}/usage"))
        .add_header(hdr, val)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();

    assert_eq!(body["agent_id"], agent_id);
    assert!(body.get("from").is_some());
    assert!(body.get("to").is_some());
    // One closed interval at the standard tier rate (priced at event time).
    let intervals = body["intervals"].as_array().unwrap();
    assert_eq!(intervals.len(), 1);
    assert_eq!(intervals[0]["tier"], "standard");
    assert_eq!(intervals[0]["hourly_price_cents"], 8);
    assert!(body["awake_seconds"].as_u64().is_some());
    assert!(body["cost_cents"].as_u64().is_some());
    assert_eq!(body["wakes"], 0);
    assert_eq!(body["triggers_fired"], 0);
    assert_eq!(body["tier_changes"], 0);
    // Raw events: PodScheduled, PodTerminated(hibernate), Hibernated.
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["kind"]["kind"], "pod_scheduled");
    assert_eq!(events[1]["kind"]["kind"], "pod_terminated");
    assert_eq!(events[1]["kind"]["reason"], "hibernate");
    assert_eq!(events[2]["kind"]["kind"], "hibernated");
}

#[tokio::test]
async fn user_usage_scoped_to_jwt_subject() {
    let (server, _tmp) = build_test_app();

    // One agent per user.
    let my_agent = create_agent(&server, None).await;
    let (other_hdr, other_val) = auth_header(OTHER_USER_UUID);
    server
        .post("/v1/agents")
        .add_header(other_hdr.clone(), other_val.clone())
        .json(&json!({"name": "their-agent"}))
        .await
        .assert_status(axum::http::StatusCode::CREATED);

    let (hdr, val) = auth_header(TEST_USER_UUID);
    let resp = server.get("/v1/usage").add_header(hdr, val).await;
    resp.assert_status_ok();
    let body: Value = resp.json();

    let agents = body["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "only the caller's agents are included");
    assert_eq!(agents[0]["agent_id"], my_agent);
    assert_eq!(agents[0]["tier"], "standard");
    assert!(body["total_awake_seconds"].as_u64().is_some());
    assert!(body["total_cost_cents"].as_u64().is_some());

    // The other user sees only theirs.
    let resp = server
        .get("/v1/usage")
        .add_header(other_hdr, other_val)
        .await;
    let body: Value = resp.json();
    let agents = body["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["name"], "their-agent");
}

#[tokio::test]
async fn status_returns_real_usage_metrics() {
    let (server, _tmp) = build_test_app();
    let agent_id = create_agent(&server, None).await;
    let (hdr, val) = auth_header(TEST_USER_UUID);
    let (internal_hdr, internal_val) = internal_auth_header();

    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .add_header(internal_hdr, internal_val)
        .json(&json!({"status": "running"}))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);

    let resp = server
        .get(&format!("/v1/agents/{agent_id}/status"))
        .add_header(hdr, val)
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();

    assert_eq!(body["status"], "running");
    assert_eq!(body["tier"], "standard");
    // The pod was just scheduled: tiny real uptime, derived from the
    // open billable interval (not the bogus created_at placeholder).
    assert!(body["uptime_seconds"].as_u64().unwrap() < 60);
    assert!(body["awake_seconds_24h"].as_u64().unwrap() < 60);
    assert!(body["estimated_cost_cents_24h"].as_u64().is_some());
    assert_eq!(body["wakes_24h"], 0);
    assert_eq!(body["triggers_fired_24h"], 0);
    // Backward-compatible placeholder fields now carry real values:
    // allocated memory while the pod is active.
    assert_eq!(body["resource_usage"]["memory_mb"], 2048);
    assert_eq!(body["resource_usage"]["cpu_percent"], 0.0);
}

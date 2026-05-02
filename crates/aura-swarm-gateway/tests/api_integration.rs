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

fn test_token(user_uuid: &str) -> String {
    format!("test-token:{user_uuid}")
}

fn auth_header(user_uuid: &str) -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(&format!("Bearer {}", test_token(user_uuid))).unwrap(),
    )
}

fn build_test_app() -> (TestServer, tempfile::TempDir) {
    let tmpdir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(RocksStore::open(tmpdir.path()).unwrap());
    let control = Arc::new(ControlPlaneService::with_defaults(store));
    let jwt = Arc::new(MockJwtValidator);
    let config = GatewayConfig::default();
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
    let resp = server.get("/internal/health").await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["status"], "ok");
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

    let create_resp = server
        .post("/v1/agents")
        .add_header(hdr.clone(), val.clone())
        .json(&json!({"name": "agent-to-delete"}))
        .await;
    let agent_id = create_resp.json::<Value>()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Transition to stopped via internal API
    server
        .patch(&format!("/internal/agents/{agent_id}/status"))
        .json(&json!({"status": "stopped"}))
        .await
        .assert_status_ok();

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
        .json(&json!({"status": "running"}))
        .await
        .assert_status_ok();

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
        .json(&json!({"status": "idle"}))
        .await
        .assert_status_ok();

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
        .json(&json!({"status": "running"}))
        .await;

    let resp = server
        .post(&format!("/v1/agents/{agent_id}/sessions"))
        .add_header(hdr.clone(), val.clone())
        .await;
    resp.assert_status(axum::http::StatusCode::CREATED);
    let body: Value = resp.json();
    assert!(body.get("session_id").is_some());
    assert!(body["ws_url"]
        .as_str()
        .unwrap()
        .starts_with("/v1/sessions/"));
    assert!(body["ws_url"].as_str().unwrap().ends_with("/ws"));
}

#[tokio::test]
async fn create_session_with_config() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

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
        .json(&json!({"status": "running"}))
        .await;
    resp.assert_status_ok();
    let body: Value = resp.json();
    assert_eq!(body["Ok"]["success"], true);
    assert_eq!(body["Ok"]["status"], "running");
}

#[tokio::test]
async fn internal_update_status_invalid_id() {
    let (server, _tmp) = build_test_app();
    let resp = server
        .patch("/internal/agents/not-hex/status")
        .json(&json!({"status": "running"}))
        .await;
    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn internal_update_status_not_found() {
    let (server, _tmp) = build_test_app();
    let fake_id = "bb".repeat(16);
    let resp = server
        .patch(&format!("/internal/agents/{fake_id}/status"))
        .json(&json!({"status": "running"}))
        .await;
    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn internal_update_status_with_error_message() {
    let (server, _tmp) = build_test_app();
    let (hdr, val) = auth_header(TEST_USER_UUID);

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
        .json(&json!({"status": "error", "message": "pod crashed"}))
        .await;
    resp.assert_status_ok();

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
        .json(&json!({"status": "running"}))
        .await
        .assert_status_ok();

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
        .json(&json!({"status": "error", "message": "OOM killed"}))
        .await
        .assert_status_ok();

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

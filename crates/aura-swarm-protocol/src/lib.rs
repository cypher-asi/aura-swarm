//! Shared protocol types for the aura-harness runtime contract.
//!
//! This crate defines the canonical message format for communication between
//! clients and the aura-harness reasoning engine. Both the CLI and gateway
//! depend on these types to ensure protocol consistency.
//!
//! A run is started with [`RuntimeRequest`] via `POST /v1/run`, which returns
//! a [`RuntimeRunResponse`] carrying the `run_id`. The caller then opens
//! `WS /stream/:run_id` and exchanges [`InboundMessage`] / [`OutboundMessage`]
//! frames over that socket (chat runs are bidirectional; automaton runs are
//! event-only).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

// =============================================================================
// Inbound Messages (Client -> Harness)
// =============================================================================

/// Client -> Harness: top-level message envelope.
///
/// Sent over `WS /stream/:run_id` after a run is created with
/// [`RuntimeRequest`]. There is no longer a `session_init` first frame —
/// session configuration now lives in the [`RuntimeRequest`] body posted to
/// `POST /v1/run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundMessage {
    /// Send a user message for processing.
    UserMessage {
        /// The user's message text.
        content: String,
    },
    /// Cancel the current turn.
    Cancel,
    /// External tool callback response (client -> harness).
    ToolCallbackResponse(ToolCallbackResponse),
}

// =============================================================================
// Outbound Messages (Harness -> Client)
// =============================================================================

/// Harness -> Client: top-level message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutboundMessage {
    /// Session initialized and ready.
    SessionReady(SessionReady),
    /// Start of an assistant message (turn).
    AssistantMessageStart {
        /// Identifier for this assistant message.
        message_id: String,
    },
    /// Incremental text content.
    TextDelta {
        /// UTF-8 text chunk appended to the assistant reply.
        text: String,
    },
    /// Incremental thinking content.
    ThinkingDelta {
        /// UTF-8 chunk of the model's reasoning stream.
        thinking: String,
    },
    /// A tool use has started.
    ToolUseStart {
        /// Unique id for this tool invocation.
        id: String,
        /// Registered name of the tool.
        name: String,
    },
    /// Result of a tool execution.
    ToolResult {
        /// Name of the tool that produced this result.
        name: String,
        /// Serialized tool output or error text.
        result: String,
        /// True when `result` represents a tool failure.
        is_error: bool,
    },
    /// End of an assistant message (turn complete).
    AssistantMessageEnd(AssistantMessageEnd),
    /// An error occurred.
    Error(ErrorMsg),
    /// External tool callback request (harness -> client).
    ToolCallbackRequest(ToolCallbackRequest),
}

/// Payload for `session_ready`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReady {
    /// Server-assigned session identifier.
    pub session_id: String,
    /// Built-in tools available in this session.
    pub tools: Vec<ToolInfo>,
}

/// Minimal tool info in the session_ready response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Tool identifier.
    pub name: String,
    /// Short description for the model.
    pub description: String,
}

/// Payload for `assistant_message_end`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessageEnd {
    /// Identifier for this assistant message (turn).
    pub message_id: String,
    /// Why generation stopped (e.g. `end_turn`).
    pub stop_reason: String,
    /// Token usage for this turn.
    pub usage: SessionUsage,
    /// Files created, modified, or deleted during this turn.
    #[serde(default)]
    pub files_changed: FilesChanged,
}

/// Token usage information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionUsage {
    /// Input tokens for this reporting period.
    pub input_tokens: u64,
    /// Output tokens for this reporting period.
    pub output_tokens: u64,
    /// Running total of input tokens for the session.
    #[serde(default)]
    pub cumulative_input_tokens: u64,
    /// Running total of output tokens for the session.
    #[serde(default)]
    pub cumulative_output_tokens: u64,
    /// Approximate fraction of the context window in use.
    #[serde(default)]
    pub context_utilization: f32,
    /// Model that produced this usage.
    #[serde(default)]
    pub model: String,
    /// API provider name (e.g. anthropic).
    #[serde(default)]
    pub provider: String,
}

/// Summary of file mutations during a turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilesChanged {
    /// Paths of files created during the turn.
    #[serde(default)]
    pub created: Vec<String>,
    /// Paths of files modified during the turn.
    #[serde(default)]
    pub modified: Vec<String>,
    /// Paths of files deleted during the turn.
    #[serde(default)]
    pub deleted: Vec<String>,
}

/// Error message payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorMsg {
    /// Stable machine-readable error code.
    pub code: String,
    /// Human-readable error message.
    pub message: String,
    /// Whether the client may retry or continue the session.
    pub recoverable: bool,
}

// =============================================================================
// Tool Callback Protocol
// =============================================================================

/// Tool callback request from harness to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallbackRequest {
    /// Correlates this request with the client's [`ToolCallbackResponse`].
    pub callback_id: String,
    /// Name of the external tool to invoke.
    pub tool_name: String,
    /// JSON arguments passed to the tool.
    pub input: serde_json::Value,
}

/// Tool callback response from client to harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallbackResponse {
    /// Must match the `callback_id` from the corresponding request.
    pub callback_id: String,
    /// Tool output text or serialized error detail.
    pub result: String,
    /// When true, `result` describes a tool or execution error.
    pub is_error: bool,
}

// =============================================================================
// Runtime Run Request (`POST /v1/run`)
// =============================================================================
//
// These types are wire-compatible *mirrors* of the canonical harness types in
// `aura_protocol::runtime_request` (aura-os). A run is created by POSTing a
// [`RuntimeRequest`] to the pod's `/v1/run` endpoint; the harness responds with
// a [`RuntimeRunResponse`] carrying the `run_id`, after which the caller opens
// `WS /stream/:run_id`.
//
// The swarm only ever *constructs* the chat-shaped subset of this contract
// (it does not author the heavy policy / capability bundles), so the
// kernel-enforced bundles (`agent_permissions`, `tool_permissions`,
// `agent_capabilities`) and the rich sub-objects the swarm never builds
// (`persona`, `provider_overrides`) are modeled as opaque
// `Option<serde_json::Value>` that simply round-trip through the gateway.

/// Canonical body of `POST /v1/run`.
///
/// Returned synchronously with [`RuntimeRunResponse`]. Build one with
/// [`RuntimeRequest::chat`] and populate the model / workspace / auth fields
/// via the builder setters or public fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRequest {
    /// Discriminated union carrying the data unique to each request type.
    /// Defaults to a chat run so the gateway can accept an empty / partial
    /// body on `POST /v1/agents/:id/run`.
    #[serde(rename = "type", default)]
    pub r#type: RuntimeRequestType,
    /// Who is this agent — template + partition + persona + skills + prompt.
    #[serde(default)]
    pub agent_identity: AgentIdentity,
    /// What model to drive the agent with.
    #[serde(default)]
    pub model: ModelSelection,
    /// Where the agent runs (workspace + project path + git repo/branch).
    #[serde(default)]
    pub workspace: WorkspaceLocation,
    /// Project context (project id + billing partition). `None` for ad-hoc chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectContext>,
    /// Opaque policy bundle — what the agent is allowed to do. The swarm does
    /// not construct this; it is forwarded verbatim when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_permissions: Option<serde_json::Value>,
    /// Opaque per-tool on/off overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_permissions: Option<serde_json::Value>,
    /// Opaque capability bundle — tools / integrations / intent classifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_capabilities: Option<serde_json::Value>,
    /// Bearer JWT forwarded to the model proxy + domain API calls. `None` is
    /// valid only in dev (auth disabled on the pod).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_jwt: Option<String>,
    /// Originating end-user id. The harness rejects an empty value with code
    /// `invalid_workspace`, but the gateway always overrides it from the
    /// authenticated caller, so it carries a serde default to let an
    /// untrusted client body omit it.
    #[serde(default)]
    pub user_id: String,
}

impl RuntimeRequest {
    /// Construct a bare chat run request for the given end-user id.
    ///
    /// Populate `model`, `workspace`, `auth_jwt`, etc. via the builder setters
    /// or by mutating the public fields directly.
    #[must_use]
    pub fn chat(user_id: impl Into<String>) -> Self {
        Self {
            r#type: RuntimeRequestType::Chat {
                conversation_messages: Vec::new(),
            },
            agent_identity: AgentIdentity::default(),
            model: ModelSelection::default(),
            workspace: WorkspaceLocation::default(),
            project: None,
            agent_permissions: None,
            tool_permissions: None,
            agent_capabilities: None,
            auth_jwt: None,
            user_id: user_id.into(),
        }
    }

    /// Set the bearer JWT forwarded to the pod.
    #[must_use]
    pub fn with_auth_jwt(mut self, jwt: impl Into<String>) -> Self {
        self.auth_jwt = Some(jwt.into());
        self
    }

    /// Set the model selection.
    #[must_use]
    pub fn with_model(mut self, model: ModelSelection) -> Self {
        self.model = model;
        self
    }

    /// Set the workspace location.
    #[must_use]
    pub fn with_workspace(mut self, workspace: WorkspaceLocation) -> Self {
        self.workspace = workspace;
        self
    }

    /// Set the agent identity.
    #[must_use]
    pub fn with_agent_identity(mut self, agent_identity: AgentIdentity) -> Self {
        self.agent_identity = agent_identity;
        self
    }
}

/// Discriminated union carrying the data unique to each run type.
///
/// Serializes with an internal `kind` tag and a `params` content object, e.g.
/// `{"kind":"chat","params":{"conversation_messages":[]}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum RuntimeRequestType {
    /// Bidirectional chat session. The WS stream stays open and the client
    /// sends `user_message` frames over it.
    Chat {
        /// Prior conversation messages to hydrate (empty for a new session).
        /// Opaque to the swarm; forwarded verbatim.
        #[serde(default)]
        conversation_messages: Vec<serde_json::Value>,
    },
    /// Dev-loop automaton — long-running, no client messages after kickoff.
    DevLoop {},
    /// Single-task automaton — runs one task to completion, then exits.
    TaskRun {
        /// Task UUID the automaton should execute.
        task_id: String,
        /// Reason text persisted on the previous attempt's failure.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prior_failure: Option<String>,
        /// Recent work-log entries the agent should re-see.
        #[serde(default)]
        work_log: Vec<String>,
    },
}

impl Default for RuntimeRequestType {
    fn default() -> Self {
        Self::Chat {
            conversation_messages: Vec::new(),
        }
    }
}

/// "Who is this agent" bundle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentIdentity {
    /// Stable template agent UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    /// Partitioned harness agent id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_id: Option<String>,
    /// Opaque persona fields. The swarm does not construct this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<serde_json::Value>,
    /// Operator-curated skill names.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Operator-authored system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

/// "What model to drive the agent with."
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelSelection {
    /// Model identifier (e.g. `"claude-opus-4-7"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Maximum tokens per model response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Maximum agentic steps per turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Opaque per-session provider overrides. The swarm does not construct this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_overrides: Option<serde_json::Value>,
}

/// "Where the agent runs."
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceLocation {
    /// Workspace directory path (under the pod's `workspaces` base).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Absolute path to the real project directory on the host filesystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    /// Optional remote-git source URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_repo_url: Option<String>,
    /// Optional remote-git branch paired with `git_repo_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
}

/// "Which project + which billing partition."
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectContext {
    /// Project UUID for domain tool calls.
    pub project_id: String,
    /// Opaque typed project descriptor. The swarm does not construct this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_info: Option<serde_json::Value>,
    /// Organization UUID for the `X-Aura-Org-Id` billing header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aura_org_id: Option<String>,
    /// Storage session UUID for the `X-Aura-Session-Id` billing header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aura_session_id: Option<String>,
    /// Project-agent UUID for the `X-Aura-Agent-Id` billing header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aura_agent_id: Option<String>,
}

/// Response body of `POST /v1/run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRunResponse {
    /// Stable identifier for the spawned run.
    pub run_id: String,
    /// The relative WS path the client should open to attach to the run.
    pub event_stream_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_runtime_request_serializes_and_round_trips() {
        let req = RuntimeRequest::chat("user-123")
            .with_auth_jwt("jwt-abc")
            .with_model(ModelSelection {
                id: Some("claude-opus-4-7".into()),
                ..Default::default()
            });

        let json = serde_json::to_string(&req).unwrap();
        // The discriminated-union body must use the harness `type`/`kind`/`params` shape.
        assert!(
            json.contains("\"type\":{\"kind\":\"chat\""),
            "unexpected chat tag shape: {json}"
        );
        assert!(json.contains("\"conversation_messages\":[]"), "{json}");
        assert!(json.contains("\"user_id\":\"user-123\""), "{json}");
        assert!(json.contains("\"auth_jwt\":\"jwt-abc\""), "{json}");
        // Opaque bundles the swarm never builds must be omitted entirely.
        assert!(!json.contains("agent_permissions"), "{json}");
        assert!(!json.contains("agent_capabilities"), "{json}");

        let back: RuntimeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.user_id, "user-123");
        assert_eq!(back.auth_jwt.as_deref(), Some("jwt-abc"));
        assert!(matches!(
            back.r#type,
            RuntimeRequestType::Chat { ref conversation_messages } if conversation_messages.is_empty()
        ));
    }

    #[test]
    fn runtime_request_deserializes_empty_body_as_chat_with_blank_user() {
        // The gateway accepts an empty `POST /v1/agents/:id/run` body and
        // overrides user_id from the authed caller, so both `type` and
        // `user_id` must carry serde defaults.
        let req: RuntimeRequest = serde_json::from_str("{}").unwrap();
        assert!(req.user_id.is_empty());
        assert!(matches!(req.r#type, RuntimeRequestType::Chat { .. }));
    }

    #[test]
    fn runtime_request_forwards_opaque_policy_and_billing_bundles() {
        // Fields the swarm never authors (permissions / capabilities /
        // project) must round-trip verbatim so the gateway forwards them
        // to the pod instead of dropping them.
        let body = serde_json::json!({
            "type": {"kind": "chat", "params": {"conversation_messages": []}},
            "user_id": "client-supplied",
            "agent_permissions": {"capabilities": ["fs_read"]},
            "tool_permissions": {"fs_write": false},
            "agent_capabilities": {"installed_tools": [{"name": "search"}]},
            "project": {
                "project_id": "proj-1",
                "aura_org_id": "org-1",
                "aura_session_id": "sess-1",
                "aura_agent_id": "agent-1"
            }
        });

        let mut req: RuntimeRequest = serde_json::from_value(body).unwrap();
        // Gateway override step.
        req.user_id = "authed-user".to_string();
        req.auth_jwt = Some("jwt".to_string());

        let forwarded = serde_json::to_value(&req).unwrap();
        assert_eq!(forwarded["user_id"], "authed-user");
        assert_eq!(forwarded["auth_jwt"], "jwt");
        assert_eq!(forwarded["agent_permissions"]["capabilities"][0], "fs_read");
        assert_eq!(forwarded["tool_permissions"]["fs_write"], false);
        assert_eq!(
            forwarded["agent_capabilities"]["installed_tools"][0]["name"],
            "search"
        );
        assert_eq!(forwarded["project"]["aura_org_id"], "org-1");
        assert_eq!(forwarded["project"]["aura_session_id"], "sess-1");
        assert_eq!(forwarded["project"]["aura_agent_id"], "agent-1");
    }

    #[test]
    fn runtime_run_response_round_trips() {
        let json = r#"{"run_id":"run-9","event_stream_url":"/stream/run-9"}"#;
        let resp: RuntimeRunResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.run_id, "run-9");
        assert_eq!(resp.event_stream_url, "/stream/run-9");
    }

    #[test]
    fn user_message_serializes() {
        let msg = InboundMessage::UserMessage {
            content: "Hello".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"user_message\""));
        assert!(json.contains("\"content\":\"Hello\""));
    }

    #[test]
    fn session_ready_deserializes() {
        let json = r#"{"type":"session_ready","session_id":"s1","tools":[{"name":"fs_read","description":"Read file"}]}"#;
        let msg: OutboundMessage = serde_json::from_str(json).unwrap();
        match msg {
            OutboundMessage::SessionReady(ready) => {
                assert_eq!(ready.session_id, "s1");
                assert_eq!(ready.tools.len(), 1);
            }
            _ => panic!("Expected SessionReady"),
        }
    }

    #[test]
    fn assistant_message_end_deserializes() {
        let json = r#"{
            "type":"assistant_message_end",
            "message_id":"m1",
            "stop_reason":"end_turn",
            "usage":{"input_tokens":100,"output_tokens":50,"model":"claude","provider":"anthropic"},
            "files_changed":{"created":[],"modified":["main.rs"],"deleted":[]}
        }"#;
        let msg: OutboundMessage = serde_json::from_str(json).unwrap();
        match msg {
            OutboundMessage::AssistantMessageEnd(end) => {
                assert_eq!(end.stop_reason, "end_turn");
                assert_eq!(end.usage.input_tokens, 100);
                assert_eq!(end.files_changed.modified, vec!["main.rs"]);
            }
            _ => panic!("Expected AssistantMessageEnd"),
        }
    }

    #[test]
    fn tool_callback_roundtrip() {
        let req = ToolCallbackRequest {
            callback_id: "cb-1".into(),
            tool_name: "task_done".into(),
            input: serde_json::json!({"notes": "done"}),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ToolCallbackRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.callback_id, "cb-1");
        assert_eq!(parsed.tool_name, "task_done");
    }

    #[test]
    fn cancel_serializes() {
        let msg = InboundMessage::Cancel;
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"cancel\""));
    }

    #[test]
    fn error_deserializes() {
        let json = r#"{"type":"error","code":"turn_error","message":"Something failed","recoverable":true}"#;
        let msg: OutboundMessage = serde_json::from_str(json).unwrap();
        match msg {
            OutboundMessage::Error(e) => {
                assert_eq!(e.code, "turn_error");
                assert!(e.recoverable);
            }
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn thinking_delta_deserializes() {
        let json = r#"{"type":"thinking_delta","thinking":"Let me consider..."}"#;
        let msg: OutboundMessage = serde_json::from_str(json).unwrap();
        match msg {
            OutboundMessage::ThinkingDelta { thinking } => {
                assert_eq!(thinking, "Let me consider...");
            }
            _ => panic!("Expected ThinkingDelta"),
        }
    }

    #[test]
    fn tool_call_deserializes() {
        let json = r#"{"type":"tool_use_start","id":"call-1","name":"fs_read"}"#;
        let msg: OutboundMessage = serde_json::from_str(json).unwrap();
        match msg {
            OutboundMessage::ToolUseStart { id, name } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "fs_read");
            }
            _ => panic!("Expected ToolUseStart"),
        }
    }

    #[test]
    fn heartbeat_deserializes() {
        let json =
            r#"{"type":"tool_result","name":"fs_read","result":"contents","is_error":false}"#;
        let msg: OutboundMessage = serde_json::from_str(json).unwrap();
        match msg {
            OutboundMessage::ToolResult {
                name,
                result,
                is_error,
            } => {
                assert_eq!(name, "fs_read");
                assert_eq!(result, "contents");
                assert!(!is_error);
            }
            _ => panic!("Expected ToolResult"),
        }
    }

    #[test]
    fn assistant_message_start_deserializes() {
        let json = r#"{"type":"assistant_message_start","message_id":"msg-42"}"#;
        let msg: OutboundMessage = serde_json::from_str(json).unwrap();
        match msg {
            OutboundMessage::AssistantMessageStart { message_id } => {
                assert_eq!(message_id, "msg-42");
            }
            _ => panic!("Expected AssistantMessageStart"),
        }
    }

    #[test]
    fn text_delta_deserializes() {
        let json = r#"{"type":"text_delta","text":"Hello world"}"#;
        let msg: OutboundMessage = serde_json::from_str(json).unwrap();
        match msg {
            OutboundMessage::TextDelta { text } => {
                assert_eq!(text, "Hello world");
            }
            _ => panic!("Expected TextDelta"),
        }
    }

    #[test]
    fn inbound_tool_callback_response_serializes() {
        let msg = InboundMessage::ToolCallbackResponse(ToolCallbackResponse {
            callback_id: "cb-99".into(),
            result: "done".into(),
            is_error: false,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"tool_callback_response\""));
        assert!(json.contains("\"callback_id\":\"cb-99\""));
    }

    #[test]
    fn session_usage_defaults() {
        let usage = SessionUsage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.cumulative_input_tokens, 0);
        assert_eq!(usage.cumulative_output_tokens, 0);
        assert_eq!(usage.context_utilization, 0.0);
        assert!(usage.model.is_empty());
        assert!(usage.provider.is_empty());
    }
}

//! Application state and event loop.
//!
//! This module manages the TUI application state and coordinates between
//! the UI, HTTP client, and WebSocket connection.
//!
//! Supports real-time streaming display where text appears token-by-token
//! as it arrives from the agent.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::client::{ClientError, GatewayClient};
use crate::tool_format::{format_tool_args, format_tool_result};
use crate::types::{Agent, AgentState, ChatMessage};
use crate::ws::{self, WsEvent, WsSender};

/// Input mode for special operations.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum InputMode {
    /// Normal operation mode.
    #[default]
    Normal,
    /// Creating a new agent (prompting for name).
    CreatingAgent,
    /// Confirming agent deletion.
    ConfirmingDelete,
}

/// Per-agent streaming state, saved when switching away from a streaming agent.
#[derive(Debug, Clone, Default)]
struct StreamingState {
    streaming_text_buffer: String,
    streaming_message_idx: Option<usize>,
    is_streaming: bool,
    current_request_id: Option<String>,
}

/// Application state.
pub struct App {
    /// HTTP client for the gateway.
    client: GatewayClient,
    /// List of agents.
    pub agents: Vec<Agent>,
    /// Currently selected agent index.
    pub selected_agent: Option<usize>,
    /// Chat messages for the current session.
    pub messages: Vec<ChatMessage>,
    /// Cached messages per agent (by agent_id).
    message_cache: HashMap<String, Vec<ChatMessage>>,
    /// Current input buffer.
    pub input: String,
    /// Cursor position in input.
    pub cursor_position: usize,
    /// Current input mode.
    pub input_mode: InputMode,
    /// WebSocket senders per connected agent (by agent_id).
    ws_senders: HashMap<String, WsSender>,
    /// Active session IDs per connected agent (by agent_id).
    session_ids: HashMap<String, String>,
    /// Chat scroll position.
    pub chat_scroll: usize,
    /// Status message to display.
    pub status_message: Option<String>,
    /// Whether the app should quit.
    pub should_quit: bool,
    /// Error message to display.
    pub error_message: Option<String>,
    /// Last refresh error to display to user.
    pub refresh_error: Option<String>,
    /// Cached streaming state per agent, used when the connected agent is not
    /// the currently selected (viewed) agent.
    streaming_state_cache: HashMap<String, StreamingState>,

    // =========================================================================
    // Streaming State
    // =========================================================================
    /// Current streaming request ID.
    current_request_id: Option<String>,
    /// Buffer for assembling streaming text (updated on every delta).
    streaming_text_buffer: String,
    /// Index of the in-progress streaming message in `messages` vec.
    /// This allows us to update the message in-place on every delta.
    streaming_message_idx: Option<usize>,
    /// Whether currently receiving a streaming response.
    pub is_streaming: bool,
    /// Whether in command mode (Esc pressed).
    /// When true, can use q to quit, n/d/s/t/r for agent actions.
    /// When false, typing goes to input, Up/Down navigate agents.
    pub command_mode: bool,
    /// Animation frame counter for loading indicators.
    pub animation_frame: usize,
    /// Saved chat input when entering a dialog mode.
    saved_chat_input: Option<(String, usize)>,
}

impl App {
    /// Create a new application.
    #[must_use]
    pub fn new(client: GatewayClient) -> Self {
        Self {
            client,
            agents: Vec::new(),
            selected_agent: None,
            messages: Vec::new(),
            message_cache: HashMap::new(),
            input: String::new(),
            cursor_position: 0,
            input_mode: InputMode::Normal,
            ws_senders: HashMap::new(),
            session_ids: HashMap::new(),
            chat_scroll: 0,
            status_message: None,
            should_quit: false,
            error_message: None,
            refresh_error: None,
            streaming_state_cache: HashMap::new(),
            current_request_id: None,
            streaming_text_buffer: String::new(),
            streaming_message_idx: None,
            is_streaming: false,
            command_mode: false,
            animation_frame: 0,
            saved_chat_input: None,
        }
    }

    /// Enter a dialog mode, saving the current chat input.
    pub fn enter_dialog_mode(&mut self, mode: InputMode) {
        // Save current input state
        self.saved_chat_input = Some((std::mem::take(&mut self.input), self.cursor_position));
        self.cursor_position = 0;
        self.input_mode = mode;
    }

    /// Exit dialog mode, restoring the saved chat input.
    pub fn exit_dialog_mode(&mut self) {
        self.input_mode = InputMode::Normal;
        // Restore saved input
        if let Some((input, cursor)) = self.saved_chat_input.take() {
            self.input = input;
            self.cursor_position = cursor;
        } else {
            self.clear_input();
        }
    }

    /// Tick the animation frame (call on each render).
    pub fn tick_animation(&mut self) {
        self.animation_frame = self.animation_frame.wrapping_add(1);
    }

    /// Get current spinner character for loading animation.
    #[must_use]
    pub fn spinner_char(&self) -> &'static str {
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        SPINNER[self.animation_frame % SPINNER.len()]
    }

    /// Get the currently selected agent.
    #[must_use]
    pub fn selected_agent(&self) -> Option<&Agent> {
        self.selected_agent.and_then(|i| self.agents.get(i))
    }

    /// Get the currently selected agent's ID.
    fn selected_agent_id(&self) -> Option<&str> {
        self.selected_agent().map(|a| a.agent_id.as_str())
    }

    /// Get the gateway URL for display.
    #[must_use]
    pub fn gateway_url(&self) -> &str {
        self.client.base_url()
    }

    /// Set the status message (also clears any error).
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
        self.error_message = None;
    }

    /// Set the error message.
    pub fn set_error(&mut self, message: impl Into<String>) {
        self.error_message = Some(message.into());
    }

    /// Clear the error message.
    pub fn clear_error(&mut self) {
        self.error_message = None;
    }

    // =========================================================================
    // Agent List Navigation
    // =========================================================================

    /// Save current messages to cache for the current agent.
    fn save_messages_to_cache(&mut self) {
        if let Some(agent) = self.selected_agent() {
            let agent_id = agent.agent_id.clone();
            if !self.messages.is_empty() {
                self.message_cache
                    .insert(agent_id, std::mem::take(&mut self.messages));
            }
        }
    }

    /// Load messages from cache for the given agent.
    fn load_messages_from_cache(&mut self, agent_id: &str) {
        self.messages = self
            .message_cache
            .get(agent_id)
            .cloned()
            .unwrap_or_default();
        self.chat_scroll = 0;
    }

    /// Switch to viewing a different agent's chat.
    fn switch_agent_view(&mut self, new_index: usize) {
        self.save_streaming_state();
        self.save_messages_to_cache();

        self.selected_agent = Some(new_index);

        if let Some(agent) = self.agents.get(new_index) {
            let id = agent.agent_id.clone();
            self.load_messages_from_cache(&id);
            self.restore_streaming_state(&id);
        }
    }

    /// Save the current global streaming state into the per-agent cache.
    fn save_streaming_state(&mut self) {
        if let Some(agent) = self.selected_agent() {
            if self.is_streaming || self.streaming_message_idx.is_some() {
                let agent_id = agent.agent_id.clone();
                self.streaming_state_cache.insert(
                    agent_id,
                    StreamingState {
                        streaming_text_buffer: std::mem::take(&mut self.streaming_text_buffer),
                        streaming_message_idx: self.streaming_message_idx.take(),
                        is_streaming: self.is_streaming,
                        current_request_id: self.current_request_id.take(),
                    },
                );
            }
        }
        self.is_streaming = false;
        self.streaming_text_buffer.clear();
        self.streaming_message_idx = None;
        self.current_request_id = None;
    }

    /// Restore per-agent streaming state back into the global fields.
    fn restore_streaming_state(&mut self, agent_id: &str) {
        if let Some(state) = self.streaming_state_cache.remove(agent_id) {
            self.streaming_text_buffer = state.streaming_text_buffer;
            self.streaming_message_idx = state.streaming_message_idx;
            self.is_streaming = state.is_streaming;
            self.current_request_id = state.current_request_id;
        }
    }

    /// Move selection up in the agent list.
    pub fn select_prev_agent(&mut self) {
        if self.agents.is_empty() {
            return;
        }

        let new_index = match self.selected_agent {
            Some(0) => self.agents.len() - 1,
            Some(i) => i - 1,
            None => 0,
        };

        self.switch_agent_view(new_index);
    }

    /// Move selection down in the agent list.
    pub fn select_next_agent(&mut self) {
        if self.agents.is_empty() {
            return;
        }

        let new_index = match self.selected_agent {
            Some(i) if i >= self.agents.len() - 1 => 0,
            Some(i) => i + 1,
            None => 0,
        };

        self.switch_agent_view(new_index);
    }

    // =========================================================================
    // Chat Scrolling
    // =========================================================================

    /// Scroll chat up (view older messages).
    pub fn scroll_chat_up(&mut self, amount: usize) {
        self.chat_scroll = self.chat_scroll.saturating_add(amount);
    }

    /// Scroll chat down (view newer messages).
    pub fn scroll_chat_down(&mut self, amount: usize) {
        self.chat_scroll = self.chat_scroll.saturating_sub(amount);
    }

    // =========================================================================
    // Input Handling
    // =========================================================================

    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor_position, c);
        self.cursor_position += 1;
    }

    /// Delete the character before the cursor.
    pub fn delete_char(&mut self) {
        if self.cursor_position > 0 {
            self.cursor_position -= 1;
            self.input.remove(self.cursor_position);
        }
    }

    /// Delete the character at the cursor.
    pub fn delete_char_forward(&mut self) {
        if self.cursor_position < self.input.len() {
            self.input.remove(self.cursor_position);
        }
    }

    /// Move cursor left.
    pub fn move_cursor_left(&mut self) {
        self.cursor_position = self.cursor_position.saturating_sub(1);
    }

    /// Move cursor right.
    pub fn move_cursor_right(&mut self) {
        if self.cursor_position < self.input.len() {
            self.cursor_position += 1;
        }
    }

    /// Move cursor to the start.
    pub fn move_cursor_start(&mut self) {
        self.cursor_position = 0;
    }

    /// Move cursor to the end.
    pub fn move_cursor_end(&mut self) {
        self.cursor_position = self.input.len();
    }

    /// Clear the input.
    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor_position = 0;
    }

    /// Take the current input (clears it).
    pub fn take_input(&mut self) -> String {
        let input = std::mem::take(&mut self.input);
        self.cursor_position = 0;
        input
    }

    // =========================================================================
    // API Operations
    // =========================================================================

    /// Refresh the agent list from the API.
    pub async fn refresh_agents(&mut self) -> Result<(), ClientError> {
        self.agents = self.client.list_agents().await?;

        // Auto-select first agent if nothing selected, or adjust if out of bounds
        if self.agents.is_empty() {
            self.selected_agent = None;
        } else if self.selected_agent.is_none() {
            // Auto-select first agent
            self.selected_agent = Some(0);
        } else if let Some(i) = self.selected_agent {
            if i >= self.agents.len() {
                self.selected_agent = Some(self.agents.len() - 1);
            }
        }

        Ok(())
    }

    /// Create a new agent.
    pub async fn create_agent(&mut self, name: &str) -> Result<(), ClientError> {
        let agent = self.client.create_agent(name, None).await?;
        self.set_status(format!("Created agent: {}", agent.name));
        self.refresh_agents().await?;

        // Select the newly created agent
        if let Some(i) = self
            .agents
            .iter()
            .position(|a| a.agent_id == agent.agent_id)
        {
            self.selected_agent = Some(i);
        }

        Ok(())
    }

    /// Delete the selected agent.
    pub async fn delete_selected_agent(&mut self) -> Result<(), ClientError> {
        if let Some(agent) = self.selected_agent() {
            let name = agent.name.clone();
            let id = agent.agent_id.clone();
            self.client.delete_agent(&id).await?;
            self.set_status(format!("Deleted agent: {name}"));
            self.refresh_agents().await?;
        }
        Ok(())
    }

    /// Start the selected agent.
    pub async fn start_selected_agent(&mut self) -> Result<(), ClientError> {
        if let Some(agent) = self.selected_agent() {
            let id = agent.agent_id.clone();
            let result = self.client.start_agent(&id).await?;
            self.set_status(format!("Agent starting: {:?}", result.status));
            self.refresh_agents().await?;
        }
        Ok(())
    }

    /// Stop the selected agent.
    pub async fn stop_selected_agent(&mut self) -> Result<(), ClientError> {
        if let Some(agent) = self.selected_agent() {
            let id = agent.agent_id.clone();
            let result = self.client.stop_agent(&id).await?;
            self.set_status(format!("Agent stopping: {:?}", result.status));
            self.refresh_agents().await?;
        }
        Ok(())
    }

    /// Restart the selected agent.
    pub async fn restart_selected_agent(&mut self) -> Result<(), ClientError> {
        if let Some(agent) = self.selected_agent() {
            let id = agent.agent_id.clone();
            let result = self.client.restart_agent(&id).await?;
            self.set_status(format!("Agent restarting: {:?}", result.status));
            self.refresh_agents().await?;
        }
        Ok(())
    }

    /// Hibernate the selected agent.
    pub async fn hibernate_selected_agent(&mut self) -> Result<(), ClientError> {
        if let Some(agent) = self.selected_agent() {
            let id = agent.agent_id.clone();
            let result = self.client.hibernate_agent(&id).await?;
            self.set_status(format!("Agent hibernating: {:?}", result.status));
            self.refresh_agents().await?;
        }
        Ok(())
    }

    /// Wake the selected agent.
    pub async fn wake_selected_agent(&mut self) -> Result<(), ClientError> {
        if let Some(agent) = self.selected_agent() {
            let id = agent.agent_id.clone();
            let result = self.client.wake_agent(&id).await?;
            self.set_status(format!("Agent waking: {:?}", result.status));
            self.refresh_agents().await?;
        }
        Ok(())
    }

    // =========================================================================
    // WebSocket Session
    // =========================================================================

    /// Ensure the selected agent is ready (running/idle), waking or starting it if necessary.
    ///
    /// This method will:
    /// - Do nothing if agent is already Running or Idle
    /// - Wake the agent if Hibernating
    /// - Start the agent if Stopped
    /// - Wait for the agent to reach Running/Idle state
    ///
    /// Returns Ok(true) if action was taken (wake/start), Ok(false) if already ready.
    pub async fn ensure_agent_ready(&mut self) -> Result<bool, String> {
        let agent = self.selected_agent().ok_or("No agent selected")?;

        match agent.status {
            AgentState::Running | AgentState::Idle => {
                // Already ready
                Ok(false)
            }
            AgentState::Hibernating => {
                // Wake the agent
                self.set_status("Waking agent...");
                self.wake_selected_agent()
                    .await
                    .map_err(|e| e.to_string())?;

                // Wait for agent to be ready
                self.wait_for_agent_ready().await?;
                Ok(true)
            }
            AgentState::Stopped => {
                // Start the agent
                self.set_status("Starting agent...");
                self.start_selected_agent()
                    .await
                    .map_err(|e| e.to_string())?;

                // Wait for agent to be ready
                self.wait_for_agent_ready().await?;
                Ok(true)
            }
            AgentState::Provisioning | AgentState::Stopping => {
                // Already transitioning, wait for it
                self.set_status("Waiting for agent...");
                self.wait_for_agent_ready().await?;
                Ok(true)
            }
            AgentState::Error => {
                // Try to restart the agent
                self.set_status("Restarting failed agent...");
                self.start_selected_agent()
                    .await
                    .map_err(|e| e.to_string())?;

                // Wait for agent to be ready
                self.wait_for_agent_ready().await?;
                Ok(true)
            }
        }
    }

    /// Wait for the selected agent to reach Running or Idle state.
    ///
    /// Polls every 500ms, up to 60 seconds.
    async fn wait_for_agent_ready(&mut self) -> Result<(), String> {
        use std::time::{Duration, Instant};

        let timeout = Duration::from_secs(60);
        let poll_interval = Duration::from_millis(500);
        let start = Instant::now();

        loop {
            // Refresh agent list to get current state
            self.refresh_agents().await.map_err(|e| e.to_string())?;

            let agent = self.selected_agent().ok_or("Agent no longer exists")?;

            match agent.status {
                AgentState::Running | AgentState::Idle => {
                    return Ok(());
                }
                AgentState::Error => {
                    let error = agent
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Unknown error".to_string());
                    return Err(format!("Agent failed: {error}"));
                }
                AgentState::Provisioning | AgentState::Stopping => {
                    // Still transitioning, keep waiting
                    let status = agent.status.as_str();
                    self.set_status(format!("Agent {status}..."));
                }
                _ => {
                    // Unexpected state
                    return Err(format!("Agent in unexpected state: {:?}", agent.status));
                }
            }

            if start.elapsed() > timeout {
                return Err("Timeout waiting for agent to be ready".to_string());
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Connect to the selected agent's chat session.
    ///
    /// Does NOT disconnect other agents — multiple concurrent connections are
    /// supported so users can chat with several agents at once.
    pub async fn connect_to_agent(&mut self) -> Result<mpsc::Receiver<WsEvent>, String> {
        let agent = self.selected_agent().ok_or("No agent selected")?;

        if !matches!(agent.status, AgentState::Running | AgentState::Idle) {
            return Err(format!("Agent is not running (status: {:?})", agent.status));
        }

        let agent_id = agent.agent_id.clone();

        // If this specific agent already has a connection, tear it down first
        if self.ws_senders.contains_key(&agent_id) {
            self.disconnect_agent(&agent_id).await;
        }

        let session = self
            .client
            .create_session(&agent_id)
            .await
            .map_err(|e| e.to_string())?;

        let session_id = session.session_id.clone();
        let ws_url = self.client.ws_url(&session_id);

        let (sender, receiver) = ws::connect(&ws_url, self.client.token())
            .await
            .map_err(|e| e.to_string())?;

        sender
            .send_session_init(aura_swarm_protocol::SessionInit::default())
            .await
            .map_err(|e| e.to_string())?;

        self.ws_senders.insert(agent_id.clone(), sender);
        self.session_ids.insert(agent_id.clone(), session_id);

        self.load_messages_from_cache(&agent_id);

        self.set_status("Connected to agent");

        Ok(receiver)
    }

    /// Ensure agent is ready, then connect to it.
    ///
    /// This is the preferred method for connecting - it will automatically
    /// wake/start agents that are hibernating/stopped.
    pub async fn ensure_ready_and_connect(&mut self) -> Result<mpsc::Receiver<WsEvent>, String> {
        // First ensure agent is ready (wake/start if needed)
        self.ensure_agent_ready().await?;

        // Now connect
        self.connect_to_agent().await
    }

    /// Disconnect a specific agent's WebSocket session.
    async fn disconnect_agent(&mut self, agent_id: &str) {
        self.ws_senders.remove(agent_id);
        self.streaming_state_cache.remove(agent_id);

        if let Some(session_id) = self.session_ids.remove(agent_id) {
            let _ = self.client.close_session(&session_id).await;
        }
    }

    /// Disconnect the currently selected agent.
    pub async fn disconnect_selected(&mut self) {
        if let Some(agent_id) = self.selected_agent_id().map(String::from) {
            if self.ws_senders.contains_key(&agent_id) {
                if !self.messages.is_empty() {
                    self.message_cache
                        .insert(agent_id.clone(), std::mem::take(&mut self.messages));
                }

                self.disconnect_agent(&agent_id).await;

                self.is_streaming = false;
                self.streaming_message_idx = None;
                self.streaming_text_buffer.clear();
                self.current_request_id = None;
                self.set_status("Disconnected");
            }
        }
    }

    /// Disconnect all agent sessions (used on app exit).
    pub async fn disconnect(&mut self) {
        if let Some(agent_id) = self.selected_agent_id().map(String::from) {
            if !self.messages.is_empty() {
                self.message_cache
                    .insert(agent_id, std::mem::take(&mut self.messages));
            }
        }

        let session_ids: Vec<String> = self.session_ids.drain().map(|(_, v)| v).collect();
        for session_id in session_ids {
            let _ = self.client.close_session(&session_id).await;
        }

        self.ws_senders.clear();
        self.streaming_state_cache.clear();
        self.is_streaming = false;
        self.streaming_message_idx = None;
        self.streaming_text_buffer.clear();
        self.current_request_id = None;
        self.set_status("Disconnected");
    }

    /// Send a chat message using the Aura runtime protocol.
    ///
    /// Sends to the currently *selected* agent's WebSocket connection.
    pub async fn send_message(&mut self, content: String) -> Result<(), String> {
        let agent_id = self
            .selected_agent_id()
            .ok_or("No agent selected")?
            .to_string();
        let sender = self
            .ws_senders
            .get(&agent_id)
            .ok_or("Not connected to this agent")?;

        self.messages.push(ChatMessage::user(&content));

        sender
            .send_prompt(&content)
            .await
            .map_err(|e| e.to_string())?;

        self.current_request_id = Some(content.clone());
        self.streaming_text_buffer.clear();
        self.streaming_message_idx = None;
        self.is_streaming = true;
        self.chat_scroll = 0;

        Ok(())
    }

    /// Handle streaming WebSocket events with real-time display updates.
    ///
    /// Returns `true` if the UI should be redrawn (for real-time streaming).
    pub fn handle_ws_event(&mut self, event: WsEvent) -> bool {
        match event {
            WsEvent::Connected => {
                self.set_status("WebSocket connected");
                true
            }
            WsEvent::SessionReady { session_id, tools } => {
                let tool_count = tools.len();
                if let Some(agent_id) = self.selected_agent_id().map(String::from) {
                    self.session_ids.insert(agent_id, session_id);
                }
                self.set_status(format!("Session ready ({tool_count} tools available)"));
                true
            }
            WsEvent::TurnStart => {
                self.is_streaming = true;
                self.streaming_text_buffer.clear();
                self.streaming_message_idx = None;
                self.set_status("Agent responding... (Esc to cancel)");
                true
            }
            WsEvent::TextDelta(text) => {
                // Append to buffer
                self.streaming_text_buffer.push_str(&text);

                // Real-time: Update or create the streaming message immediately
                self.update_streaming_message_live();

                // Return true to trigger immediate UI redraw
                true
            }
            WsEvent::ThinkingDelta(thinking) => {
                // Show thinking content in the chat with dimmed styling
                // Add thinking block header if this is the start of thinking
                if !self.streaming_text_buffer.ends_with("*thinking...*\n")
                    && !self.streaming_text_buffer.contains("💭")
                {
                    self.streaming_text_buffer.push_str("\n💭 *thinking...*\n");
                }

                // Append thinking content (dimmed in markdown via italics)
                self.streaming_text_buffer
                    .push_str(&format!("*{thinking}*"));
                self.update_streaming_message_live();

                // Show truncated thinking in status bar
                let preview = if thinking.len() > 60 {
                    format!("{}...", &thinking[..60])
                } else {
                    thinking
                };
                self.set_status(format!("Thinking: {preview}"));
                true
            }
            WsEvent::ToolStart { tool_name, args } => {
                // End any thinking block before showing tool
                if self.streaming_text_buffer.contains("💭")
                    && !self.streaming_text_buffer.ends_with("\n\n")
                {
                    self.streaming_text_buffer.push_str("\n\n");
                }

                // Format tool call as a compact bullet point (no newline yet - result will follow)
                let args_display = format_tool_args(&tool_name, &args);
                let display = if args_display.is_empty() {
                    format!("`{tool_name}`")
                } else {
                    format!("`{args_display}`")
                };

                // Add the tool display without newline - result will complete the line
                self.streaming_text_buffer.push_str(&display);
                self.update_streaming_message_live();

                // Show detailed status
                let status_detail = if args_display.is_empty() {
                    tool_name.clone()
                } else if args_display.len() > 50 {
                    format!("{tool_name}: {}...", &args_display[..50])
                } else {
                    format!("{tool_name}: {args_display}")
                };
                self.set_status(format!("Running: {status_detail}"));
                true
            }
            WsEvent::ToolComplete {
                tool_name,
                result,
                is_error,
            } => {
                // Update status
                let status = if is_error { "Error" } else { "Done" };
                self.set_status(format!("{tool_name}: {status}"));

                // Parse and format the result compactly
                let display_result = format_tool_result(&result);

                // Format result inline with tool, or on next lines if multi-line
                let formatted_result = if display_result.lines().count() > 1 {
                    // Multi-line: show in code block on next line
                    format!("\n```\n{display_result}\n```\n")
                } else if display_result.is_empty()
                    || display_result == "OK"
                    || display_result == "ok"
                {
                    // Simple success, just add checkmark and newline
                    if is_error {
                        " x\n".to_string()
                    } else {
                        " +\n".to_string()
                    }
                } else {
                    // Single line result - show inline
                    let marker = if is_error { " x " } else { " + " };
                    format!("{marker}{display_result}\n")
                };

                self.streaming_text_buffer.push_str(&formatted_result);
                self.update_streaming_message_live();
                true
            }
            WsEvent::TurnComplete(info) => {
                // Finalize: remove cursor, keep final text
                self.finalize_streaming_message();

                self.is_streaming = false;
                self.current_request_id = None;
                self.streaming_message_idx = None;

                self.set_status(format!(
                    "Complete ({} steps, {} in / {} out tokens)",
                    info.steps, info.input_tokens, info.output_tokens
                ));
                self.chat_scroll = 0;
                true
            }
            WsEvent::Error { message, code } => {
                self.is_streaming = false;
                self.current_request_id = None;
                self.streaming_message_idx = None;
                self.set_error(format!("Error: {message} (code: {code:?})"));
                true
            }
            WsEvent::ToolCallbackRequest {
                callback_id,
                tool_name,
                ..
            } => {
                tracing::info!(
                    callback_id = %callback_id,
                    tool_name = %tool_name,
                    "Received tool callback request (not yet handled by CLI)"
                );
                self.set_status(format!("Tool callback: {tool_name} (id: {callback_id})"));
                true
            }
            WsEvent::Disconnected => {
                self.is_streaming = false;
                self.streaming_message_idx = None;
                self.set_status("Disconnected");
                true
            }
        }
    }

    /// Handle a WebSocket event tagged with the originating agent_id.
    ///
    /// If the event belongs to the currently *selected* (viewed) agent, apply
    /// it live to `self.messages`. Otherwise, route it to the background
    /// message + streaming caches so switching back to that agent later shows
    /// the correct history.
    pub fn handle_ws_event_for_agent(&mut self, agent_id: &str, event: WsEvent) -> bool {
        // Clean up connection bookkeeping on disconnect regardless of which
        // agent it is or whether it is selected.
        if matches!(event, WsEvent::Disconnected) {
            self.ws_senders.remove(agent_id);
            self.session_ids.remove(agent_id);
        }

        let is_selected = self
            .selected_agent_id()
            .map_or(false, |id| id == agent_id);

        if is_selected {
            return self.handle_ws_event(event);
        }

        self.handle_background_ws_event(agent_id, event)
    }

    /// Accumulate a WS event into the per-agent caches (message_cache +
    /// streaming_state_cache) for an agent that is not currently viewed.
    fn handle_background_ws_event(&mut self, agent_id: &str, event: WsEvent) -> bool {
        let cached_msgs = self.message_cache.entry(agent_id.to_string()).or_default();
        let streaming = self
            .streaming_state_cache
            .entry(agent_id.to_string())
            .or_default();

        match event {
            WsEvent::Connected | WsEvent::SessionReady { .. } => {}
            WsEvent::TurnStart => {
                streaming.is_streaming = true;
                streaming.streaming_text_buffer.clear();
                streaming.streaming_message_idx = None;
            }
            WsEvent::TextDelta(text) => {
                streaming.streaming_text_buffer.push_str(&text);
                Self::update_cached_streaming_message(cached_msgs, streaming);
            }
            WsEvent::ThinkingDelta(thinking) => {
                if !streaming.streaming_text_buffer.ends_with("*thinking...*\n")
                    && !streaming.streaming_text_buffer.contains("💭")
                {
                    streaming
                        .streaming_text_buffer
                        .push_str("\n💭 *thinking...*\n");
                }
                streaming
                    .streaming_text_buffer
                    .push_str(&format!("*{thinking}*"));
                Self::update_cached_streaming_message(cached_msgs, streaming);
            }
            WsEvent::ToolStart { tool_name, args } => {
                if streaming.streaming_text_buffer.contains("💭")
                    && !streaming.streaming_text_buffer.ends_with("\n\n")
                {
                    streaming.streaming_text_buffer.push_str("\n\n");
                }
                let args_display = format_tool_args(&tool_name, &args);
                let display = if args_display.is_empty() {
                    format!("`{tool_name}`")
                } else {
                    format!("`{args_display}`")
                };
                streaming.streaming_text_buffer.push_str(&display);
                Self::update_cached_streaming_message(cached_msgs, streaming);
            }
            WsEvent::ToolComplete {
                tool_name: _,
                result,
                is_error,
            } => {
                let display_result = format_tool_result(&result);
                let formatted_result = if display_result.lines().count() > 1 {
                    format!("\n```\n{display_result}\n```\n")
                } else if display_result.is_empty()
                    || display_result == "OK"
                    || display_result == "ok"
                {
                    if is_error {
                        " x\n".to_string()
                    } else {
                        " +\n".to_string()
                    }
                } else {
                    let marker = if is_error { " x " } else { " + " };
                    format!("{marker}{display_result}\n")
                };
                streaming.streaming_text_buffer.push_str(&formatted_result);
                Self::update_cached_streaming_message(cached_msgs, streaming);
            }
            WsEvent::TurnComplete(_) => {
                if let Some(idx) = streaming.streaming_message_idx {
                    if let Some(msg) = cached_msgs.get_mut(idx) {
                        msg.content = std::mem::take(&mut streaming.streaming_text_buffer);
                    }
                }
                streaming.is_streaming = false;
                streaming.streaming_message_idx = None;
                streaming.current_request_id = None;
            }
            WsEvent::Error { .. } => {
                streaming.is_streaming = false;
                streaming.streaming_message_idx = None;
                streaming.current_request_id = None;
            }
            WsEvent::Disconnected => {
                streaming.is_streaming = false;
                streaming.streaming_message_idx = None;
            }
            WsEvent::ToolCallbackRequest { .. } => {}
        }
        false
    }

    /// Update the in-progress assistant message inside a cached message list.
    fn update_cached_streaming_message(
        cached: &mut Vec<ChatMessage>,
        streaming: &mut StreamingState,
    ) {
        let content = streaming.streaming_text_buffer.clone();
        match streaming.streaming_message_idx {
            Some(idx) => {
                if let Some(msg) = cached.get_mut(idx) {
                    msg.content = content;
                }
            }
            None => {
                cached.push(ChatMessage::assistant(content));
                streaming.streaming_message_idx = Some(cached.len() - 1);
            }
        }
    }

    /// Update the streaming message in-place for real-time display.
    ///
    /// Called on every `TextDelta` - this is the key to real-time streaming.
    fn update_streaming_message_live(&mut self) {
        // Content with blinking cursor indicator
        let content = format!("{}▌", self.streaming_text_buffer);

        match self.streaming_message_idx {
            Some(idx) => {
                // Update existing message in-place
                if let Some(msg) = self.messages.get_mut(idx) {
                    msg.content = content;
                }
            }
            None => {
                // First delta: create the streaming message
                self.messages.push(ChatMessage::assistant(content));
                self.streaming_message_idx = Some(self.messages.len() - 1);
            }
        }

        // Keep scrolled to bottom
        self.chat_scroll = 0;
    }

    /// Finalize the streaming message (remove cursor indicator).
    fn finalize_streaming_message(&mut self) {
        if let Some(idx) = self.streaming_message_idx {
            if let Some(msg) = self.messages.get_mut(idx) {
                // Remove cursor, use final buffer content
                msg.content = std::mem::take(&mut self.streaming_text_buffer);
            }
        }
    }

    /// Check if UI needs high-frequency redraws (during streaming).
    #[must_use]
    pub fn needs_immediate_redraw(&self) -> bool {
        self.is_streaming
    }

    /// Check if the currently selected agent has an active connection.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.selected_agent_id()
            .map_or(false, |id| self.ws_senders.contains_key(id))
    }

    /// Cancel the current streaming response.
    ///
    /// Returns `true` if a cancel was sent, `false` if not streaming.
    pub async fn cancel_streaming(&mut self) -> bool {
        if !self.is_streaming {
            return false;
        }

        let agent_id = match self.selected_agent_id() {
            Some(id) => id.to_string(),
            None => return false,
        };

        if let Some(sender) = self.ws_senders.get(&agent_id) {
            if let Err(e) = sender.cancel().await {
                self.set_error(format!("Failed to cancel: {e}"));
                return false;
            }
            self.set_status("Cancelling...");
            true
        } else {
            false
        }
    }
}

/// Refresh interval for agent list.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client() -> GatewayClient {
        GatewayClient::new("http://test.local", "test-token").unwrap()
    }

    #[test]
    fn app_initial_state() {
        let app = App::new(test_client());
        assert!(!app.command_mode);
        assert!(app.agents.is_empty());
        assert_eq!(app.chat_scroll, 0);
        assert_eq!(app.input, "");
        assert_eq!(app.cursor_position, 0);
        assert!(!app.should_quit);
        assert!(!app.is_streaming);
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn input_insert_delete() {
        let mut app = App::new(test_client());
        app.insert_char('a');
        app.insert_char('b');
        app.insert_char('c');
        assert_eq!(app.input, "abc");
        assert_eq!(app.cursor_position, 3);

        app.delete_char();
        assert_eq!(app.input, "ab");
        assert_eq!(app.cursor_position, 2);
    }

    #[test]
    fn input_move_cursor() {
        let mut app = App::new(test_client());
        for c in "hello".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.cursor_position, 5);

        app.move_cursor_left();
        assert_eq!(app.cursor_position, 4);

        app.insert_char('X');
        assert_eq!(app.input, "hellXo");
        assert_eq!(app.cursor_position, 5);
    }

    #[test]
    fn input_clear() {
        let mut app = App::new(test_client());
        app.insert_char('h');
        app.insert_char('i');
        assert_eq!(app.input, "hi");

        app.clear_input();
        assert_eq!(app.input, "");
        assert_eq!(app.cursor_position, 0);
    }

    #[test]
    fn take_input_clears() {
        let mut app = App::new(test_client());
        for c in "msg".chars() {
            app.insert_char(c);
        }
        let taken = app.take_input();
        assert_eq!(taken, "msg");
        assert_eq!(app.input, "");
        assert_eq!(app.cursor_position, 0);
    }

    #[test]
    fn scroll_saturating() {
        let mut app = App::new(test_client());
        assert_eq!(app.chat_scroll, 0);

        app.scroll_chat_down(5);
        assert_eq!(app.chat_scroll, 0, "saturating_sub at 0 stays 0");

        app.scroll_chat_up(3);
        assert_eq!(app.chat_scroll, 3);

        app.scroll_chat_down(1);
        assert_eq!(app.chat_scroll, 2);
    }

    #[test]
    fn select_agent_empty() {
        let mut app = App::new(test_client());
        app.select_next_agent();
        assert!(app.selected_agent.is_none());

        app.select_prev_agent();
        assert!(app.selected_agent.is_none());
    }

    #[test]
    fn set_status_clears_error() {
        let mut app = App::new(test_client());
        app.set_error("oops");
        assert_eq!(app.error_message.as_deref(), Some("oops"));

        app.set_status("ok");
        assert_eq!(app.status_message.as_deref(), Some("ok"));
        assert!(app.error_message.is_none(), "set_status should clear error");
    }

    #[test]
    fn dialog_mode_saves_input() {
        let mut app = App::new(test_client());
        for c in "draft".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.cursor_position, 5);

        app.enter_dialog_mode(InputMode::CreatingAgent);
        assert_eq!(app.input_mode, InputMode::CreatingAgent);
        assert_eq!(app.input, "");
        assert_eq!(app.cursor_position, 0);

        app.exit_dialog_mode();
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.input, "draft");
        assert_eq!(app.cursor_position, 5);
    }

    #[test]
    fn delete_char_at_start_is_noop() {
        let mut app = App::new(test_client());
        app.delete_char();
        assert_eq!(app.input, "");
        assert_eq!(app.cursor_position, 0);
    }

    #[test]
    fn delete_char_forward() {
        let mut app = App::new(test_client());
        for c in "abc".chars() {
            app.insert_char(c);
        }
        app.move_cursor_left();
        app.move_cursor_left();
        assert_eq!(app.cursor_position, 1);

        app.delete_char_forward();
        assert_eq!(app.input, "ac");
        assert_eq!(app.cursor_position, 1);
    }

    #[test]
    fn delete_char_forward_at_end_is_noop() {
        let mut app = App::new(test_client());
        app.insert_char('a');
        app.delete_char_forward();
        assert_eq!(app.input, "a");
    }

    #[test]
    fn move_cursor_boundaries() {
        let mut app = App::new(test_client());
        app.move_cursor_left();
        assert_eq!(app.cursor_position, 0);

        app.insert_char('x');
        app.move_cursor_right();
        assert_eq!(app.cursor_position, 1, "can't go past end");
    }

    #[test]
    fn tick_animation_wraps() {
        let mut app = App::new(test_client());
        for _ in 0..20 {
            let ch = app.spinner_char();
            assert!(!ch.is_empty());
            app.tick_animation();
        }
    }

    #[test]
    fn clear_error() {
        let mut app = App::new(test_client());
        app.set_error("fail");
        assert!(app.error_message.is_some());
        app.clear_error();
        assert!(app.error_message.is_none());
    }

    #[test]
    fn move_cursor_start_end() {
        let mut app = App::new(test_client());
        for c in "hello".chars() {
            app.insert_char(c);
        }
        app.move_cursor_start();
        assert_eq!(app.cursor_position, 0);
        app.move_cursor_end();
        assert_eq!(app.cursor_position, 5);
    }
}

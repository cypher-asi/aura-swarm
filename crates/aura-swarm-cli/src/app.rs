//! Application state and event loop.
//!
//! This module manages the TUI application state and coordinates between
//! the UI, HTTP client, and WebSocket connection.
//!
//! Supports real-time streaming display where text appears token-by-token
//! as it arrives from the agent.

use std::time::Duration;

use base64::Engine;
use tokio::sync::mpsc;

use crate::client::{ClientError, GatewayClient};
use crate::types::{Agent, AgentState, ChatMessage};
use crate::ws::{self, WsEvent, WsSender};

// =============================================================================
// Tool Result Formatting
// =============================================================================

/// Parsed tool result from the agent runtime.
#[derive(Debug)]
struct ToolResult {
    tool: String,
    ok: bool,
    stdout: String,
    stderr: String,
}

impl ToolResult {
    /// Parse a tool result JSON string.
    fn parse(json: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        let obj = v.as_object()?;
        
        let tool = obj.get("tool")?.as_str()?.to_string();
        let ok = obj.get("ok")?.as_bool()?;
        let stdout_b64 = obj.get("stdout")?.as_str().unwrap_or("");
        let stderr_b64 = obj.get("stderr")?.as_str().unwrap_or("");
        
        // Decode base64
        let stdout = decode_base64(stdout_b64);
        let stderr = decode_base64(stderr_b64);
        
        Some(Self { tool, ok, stdout, stderr })
    }
    
    /// Format the result for display.
    fn format(&self) -> String {
        if !self.ok {
            // Error case - show stderr
            let error_msg = if self.stderr.is_empty() {
                "Unknown error".to_string()
            } else {
                self.stderr.trim().to_string()
            };
            return format!("Error: {error_msg}");
        }
        
        // Format based on tool type
        match self.tool.as_str() {
            "fs.ls" | "fs_ls" => self.format_ls(),
            "fs.read" | "fs_read" => self.format_read(),
            "fs.write" | "fs_write" => self.format_write(),
            "cmd.run" | "cmd_run" => self.format_cmd(),
            _ => self.format_generic(),
        }
    }
    
    fn format_ls(&self) -> String {
        if self.stdout.trim().is_empty() {
            "(empty directory)".to_string()
        } else {
            // Parse as file listing - each line is a file/dir
            let entries: Vec<&str> = self.stdout.lines().collect();
            if entries.is_empty() {
                "(empty directory)".to_string()
            } else {
                let count = entries.len();
                let preview: String = entries.iter().take(10).map(|e| format!("  {e}")).collect::<Vec<_>>().join("\n");
                if count > 10 {
                    format!("{preview}\n  ... and {} more", count - 10)
                } else {
                    preview
                }
            }
        }
    }
    
    fn format_read(&self) -> String {
        if self.stdout.is_empty() {
            "(empty file)".to_string()
        } else {
            // Show file contents, truncated if long
            let content = self.stdout.trim();
            let lines: Vec<&str> = content.lines().collect();
            if lines.len() > 20 {
                let preview: String = lines.iter().take(20).cloned().collect::<Vec<_>>().join("\n");
                format!("{preview}\n... ({} more lines)", lines.len() - 20)
            } else {
                content.to_string()
            }
        }
    }
    
    fn format_write(&self) -> String {
        // stdout usually contains "Wrote N bytes to filename"
        if self.stdout.is_empty() {
            "File written".to_string()
        } else {
            self.stdout.trim().to_string()
        }
    }
    
    fn format_cmd(&self) -> String {
        let mut output = String::new();
        
        if !self.stdout.is_empty() {
            output.push_str(self.stdout.trim());
        }
        
        if !self.stderr.is_empty() {
            if !output.is_empty() {
                output.push_str("\n\n");
            }
            output.push_str("stderr:\n");
            output.push_str(self.stderr.trim());
        }
        
        if output.is_empty() {
            "(no output)".to_string()
        } else {
            // Truncate very long output
            if output.len() > 2000 {
                format!("{}...\n(truncated)", &output[..2000])
            } else {
                output
            }
        }
    }
    
    fn format_generic(&self) -> String {
        if !self.stdout.is_empty() {
            let content = self.stdout.trim();
            if content.len() > 500 {
                format!("{}...", &content[..500])
            } else {
                content.to_string()
            }
        } else {
            "OK".to_string()
        }
    }
}

/// Decode a base64 string to UTF-8 text.
fn decode_base64(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| input.to_string()) // Fall back to raw if not valid base64
}

/// Format a tool result for display.
fn format_tool_result(result: &str) -> String {
    ToolResult::parse(result)
        .map(|r| r.format())
        .unwrap_or_else(|| {
            // Fallback: just show the raw result, truncated
            if result.len() > 500 {
                format!("{}...", &result[..500])
            } else {
                result.to_string()
            }
        })
}

/// Format tool arguments like a shell command for display.
/// Returns empty string if there's nothing meaningful to show.
fn format_tool_args(tool_name: &str, args: &serde_json::Value) -> String {
    // Skip if args is null or empty
    if args.is_null() {
        return String::new();
    }
    
    let obj = args.as_object();
    if obj.map_or(false, |o| o.is_empty()) {
        return String::new();
    }
    
    // Extract common argument patterns for cleaner display
    match tool_name {
        "fs.ls" | "fs_ls" => {
            let path = obj.and_then(|o| o.get("path")).and_then(|v| v.as_str()).unwrap_or(".");
            format!("ls {path}")
        }
        "fs.read" | "fs_read" => {
            let path = obj.and_then(|o| o.get("path")).and_then(|v| v.as_str());
            path.map(|p| format!("cat {p}")).unwrap_or_default()
        }
        "fs.write" | "fs_write" => {
            let path = obj.and_then(|o| o.get("path")).and_then(|v| v.as_str());
            let content = obj.and_then(|o| o.get("content")).and_then(|v| v.as_str()).unwrap_or("");
            match path {
                Some(p) => {
                    let preview = if content.len() > 30 {
                        format!("{}...", &content[..30])
                    } else {
                        content.to_string()
                    };
                    format!("write {p} \"{preview}\"")
                }
                None => String::new(),
            }
        }
        "fs.mkdir" | "fs_mkdir" => {
            obj.and_then(|o| o.get("path")).and_then(|v| v.as_str())
                .map(|p| format!("mkdir {p}"))
                .unwrap_or_default()
        }
        "fs.rm" | "fs_rm" => {
            obj.and_then(|o| o.get("path")).and_then(|v| v.as_str())
                .map(|p| format!("rm {p}"))
                .unwrap_or_default()
        }
        "cmd.run" | "cmd_run" => {
            let cmd = obj.and_then(|o| o.get("command")).and_then(|v| v.as_str())
                .or_else(|| obj.and_then(|o| o.get("cmd")).and_then(|v| v.as_str()));
            cmd.map(|c| format!("$ {c}")).unwrap_or_default()
        }
        _ => {
            // Generic: show tool name with compact args
            let compact = serde_json::to_string(args).unwrap_or_default();
            if compact == "{}" || compact.is_empty() {
                String::new()
            } else if compact.len() > 60 {
                format!("{tool_name} {}...", &compact[..60])
            } else {
                format!("{tool_name} {compact}")
            }
        }
    }
}

/// Which UI column has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// Left column: Agent list panel.
    #[default]
    Agents,
    /// Right column: Chat area with input.
    Chat,
}

impl Focus {
    /// Toggle to the other column.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Agents => Self::Chat,
            Self::Chat => Self::Agents,
        }
    }

    /// Toggle to the other column (same as next for two columns).
    #[must_use]
    pub const fn prev(self) -> Self {
        self.next()
    }
}

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
    /// Current input buffer.
    pub input: String,
    /// Cursor position in input.
    pub cursor_position: usize,
    /// Which panel has focus.
    pub focus: Focus,
    /// Current input mode.
    pub input_mode: InputMode,
    /// WebSocket sender (if connected).
    ws_sender: Option<WsSender>,
    /// Current session ID (if connected).
    current_session_id: Option<String>,
    /// Chat scroll position.
    pub chat_scroll: usize,
    /// Status message to display.
    pub status_message: Option<String>,
    /// Whether the app should quit.
    pub should_quit: bool,
    /// Error message to display.
    pub error_message: Option<String>,
    /// Whether WebSocket is connected.
    pub ws_connected: bool,
    /// Last refresh error to display to user.
    pub refresh_error: Option<String>,

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
    /// Whether in insert mode for chat input (vim-like).
    /// When true, keys go to input. When false, can use commands like 'q' to quit.
    pub chat_insert_mode: bool,
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
            input: String::new(),
            cursor_position: 0,
            focus: Focus::Agents,
            input_mode: InputMode::Normal,
            ws_sender: None,
            current_session_id: None,
            chat_scroll: 0,
            status_message: None,
            should_quit: false,
            error_message: None,
            ws_connected: false,
            refresh_error: None,
            // Streaming state
            current_request_id: None,
            streaming_text_buffer: String::new(),
            streaming_message_idx: None,
            is_streaming: false,
            // Start in insert mode for chat
            chat_insert_mode: true,
            // Animation
            animation_frame: 0,
            // No saved input initially
            saved_chat_input: None,
        }
    }
    
    /// Enter a dialog mode, saving the current chat input.
    pub fn enter_dialog_mode(&mut self, mode: InputMode) {
        // Save current input state
        self.saved_chat_input = Some((
            std::mem::take(&mut self.input),
            self.cursor_position,
        ));
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
        self.selected_agent
            .and_then(|i| self.agents.get(i))
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

    /// Move selection up in the agent list.
    pub fn select_prev_agent(&mut self) {
        if self.agents.is_empty() {
            return;
        }

        self.selected_agent = Some(match self.selected_agent {
            Some(0) => self.agents.len() - 1,
            Some(i) => i - 1,
            None => 0,
        });
    }

    /// Move selection down in the agent list.
    pub fn select_next_agent(&mut self) {
        if self.agents.is_empty() {
            return;
        }

        self.selected_agent = Some(match self.selected_agent {
            Some(i) if i >= self.agents.len() - 1 => 0,
            Some(i) => i + 1,
            None => 0,
        });
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

        // Adjust selection if needed
        if let Some(i) = self.selected_agent {
            if i >= self.agents.len() {
                self.selected_agent = if self.agents.is_empty() {
                    None
                } else {
                    Some(self.agents.len() - 1)
                };
            }
        }

        Ok(())
    }

    /// Create a new agent.
    pub async fn create_agent(&mut self, name: &str) -> Result<(), ClientError> {
        let agent = self.client.create_agent(name).await?;
        self.set_status(format!("Created agent: {}", agent.name));
        self.refresh_agents().await?;

        // Select the newly created agent
        if let Some(i) = self.agents.iter().position(|a| a.agent_id == agent.agent_id) {
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

    /// Connect to the selected agent's chat session.
    pub async fn connect_to_agent(&mut self) -> Result<mpsc::Receiver<WsEvent>, String> {
        let agent = self.selected_agent().ok_or("No agent selected")?;

        // Check agent is in a runnable state
        if !matches!(agent.status, AgentState::Running | AgentState::Idle) {
            return Err(format!(
                "Agent is not running (status: {:?})",
                agent.status
            ));
        }

        let agent_id = agent.agent_id.clone();

        // Create session
        let session = self
            .client
            .create_session(&agent_id)
            .await
            .map_err(|e| e.to_string())?;

        let session_id = session.session_id.clone();
        let ws_url = self.client.ws_url(&session_id);

        // Connect WebSocket
        let (sender, receiver) = ws::connect(&ws_url, self.client.token())
            .await
            .map_err(|e| e.to_string())?;

        self.ws_sender = Some(sender);
        self.current_session_id = Some(session_id);
        self.messages.clear();
        self.chat_scroll = 0;
        self.ws_connected = true;

        self.set_status("Connected to agent");

        Ok(receiver)
    }

    /// Disconnect from the current session.
    pub async fn disconnect(&mut self) {
        if let Some(session_id) = self.current_session_id.take() {
            let _ = self.client.close_session(&session_id).await;
        }

        self.ws_sender = None;
        self.ws_connected = false;
        self.set_status("Disconnected");
    }

    /// Send a chat message using the Aura runtime protocol.
    ///
    /// Sends a prompt request and prepares the app for receiving streaming deltas.
    pub async fn send_message(&mut self, content: String) -> Result<(), String> {
        let sender = self.ws_sender.as_ref().ok_or("Not connected")?;
        let agent = self.selected_agent().ok_or("No agent selected")?;
        let agent_id = agent.agent_id.clone();

        // Add user message to local display
        self.messages.push(ChatMessage::user(&content));

        // Send prompt request (server handles tool execution)
        let request_id = sender
            .send_prompt(&content, Some(&agent_id), None)
            .await
            .map_err(|e| e.to_string())?;

        // Prepare streaming state
        self.current_request_id = Some(request_id);
        self.streaming_text_buffer.clear();
        self.streaming_message_idx = None; // Will be set on first delta
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
                self.ws_connected = true;
                self.set_status("WebSocket connected");
                true
            }
            WsEvent::TurnStart => {
                self.is_streaming = true;
                self.streaming_text_buffer.clear();
                self.streaming_message_idx = None;
                self.set_status("Agent responding... (Esc to cancel)");
                true
            }
            WsEvent::StepStart { step } => {
                // Update status to show step progress
                self.set_status(format!("Step {step}... (Esc to cancel)"));
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
            WsEvent::ThinkingDelta(_thinking) => {
                // Optionally show thinking indicator in status bar
                self.set_status("Agent thinking...");
                true
            }
            WsEvent::ToolStart { tool_name, args } => {
                // Show that the server is executing a tool
                self.set_status(format!("Executing: {tool_name}..."));
                
                // Format tool call like a command
                let args_display = format_tool_args(&tool_name, &args);
                let display = if args_display.is_empty() {
                    tool_name.clone() // Just show tool name when no args
                } else {
                    args_display
                };
                self.streaming_text_buffer.push_str(&format!("\n\n🔧 `{display}`\n"));
                self.update_streaming_message_live();
                true
            }
            WsEvent::ToolComplete { tool_name, result, is_error } => {
                // Show tool execution result with proper formatting
                let icon = if is_error { "❌" } else { "✅" };
                let status = if is_error { "Error" } else { "Success" };
                self.set_status(format!("{tool_name}: {status}"));
                
                // Parse and format the result for clean display
                let display_result = format_tool_result(&result);
                self.streaming_text_buffer.push_str(&format!("\n{icon} **Result:**\n```\n{display_result}\n```\n\n"));
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
            WsEvent::Cancelled { request_id } => {
                self.is_streaming = false;
                if self.current_request_id.as_deref() == Some(&request_id) {
                    self.current_request_id = None;
                }
                // Keep partial response but mark as cancelled
                self.finalize_streaming_message();
                self.streaming_message_idx = None;
                self.set_status("Cancelled");
                true
            }
            WsEvent::Disconnected => {
                self.ws_connected = false;
                self.ws_sender = None;
                self.current_session_id = None;
                self.is_streaming = false;
                self.streaming_message_idx = None;
                self.set_status("Disconnected");
                true
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

    /// Check if connected to a session.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.ws_sender.is_some() && self.ws_connected
    }

    /// Cancel the current streaming response.
    ///
    /// Returns `true` if a cancel was sent, `false` if not streaming.
    pub async fn cancel_streaming(&mut self) -> bool {
        if let (Some(request_id), Some(sender)) =
            (self.current_request_id.as_ref(), self.ws_sender.as_ref())
        {
            if let Err(e) = sender.cancel(request_id).await {
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

//! Aura Swarm CLI - Terminal UI for managing agents.
//!
//! This is the entry point for the `aswarm` binary.

mod app;
mod client;
mod markdown;
mod types;
mod ui;
mod ws;

use std::io;
use std::time::Duration;

use clap::Parser;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use app::{App, InputMode, REFRESH_INTERVAL};
use client::GatewayClient;
use types::AgentState;
use ws::WsEvent;

/// Aura Swarm CLI - Terminal UI for managing agents.
#[derive(Parser, Debug)]
#[command(name = "aswarm")]
#[command(author, version, about, long_about = None)]
struct Args {
    /// JWT token for authentication.
    #[arg(long, env = "AURA_SWARM_TOKEN")]
    token: String,

    /// Gateway URL.
    #[arg(
        long,
        env = "AURA_SWARM_GATEWAY",
        default_value = "http://localhost:8080"
    )]
    gateway: String,

    /// Enable debug logging.
    #[arg(long, default_value = "false")]
    debug: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse arguments
    let args = Args::parse();

    // Initialize logging
    if args.debug {
        tracing_subscriber::fmt()
            .with_env_filter("aura_swarm_cli=debug,warn")
            .with_writer(std::io::stderr)
            .init();
    }

    // Create client
    let client = GatewayClient::new(&args.gateway, &args.token);

    // Setup terminal with mouse capture enabled
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app
    let mut app = App::new(client);

    // Initial agent load
    if let Err(e) = app.refresh_agents().await {
        app.set_error(format!("Failed to load agents: {e}"));
    }

    // Run the event loop
    let result = run_event_loop(&mut terminal, &mut app).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

    result
}

/// Main event loop with real-time streaming support.
///
/// The event loop immediately redraws on every WebSocket event for smooth streaming.
async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    // Channel for WebSocket events
    let (ws_tx, mut ws_rx) = mpsc::channel::<WsEvent>(128);

    // Refresh timer
    let mut refresh_interval = tokio::time::interval(REFRESH_INTERVAL);

    loop {
        // Tick animation frame
        app.tick_animation();

        // Render
        terminal.draw(|f| ui::render(f, app))?;

        // Use a shorter tick rate during streaming for responsiveness
        let tick_rate = if app.needs_immediate_redraw() {
            Duration::from_millis(80) // ~12fps during streaming for smooth spinner
        } else {
            Duration::from_millis(100) // Normal rate when idle
        };

        // Handle events
        tokio::select! {
            // Terminal events - poll with short timeout
            () = tokio::time::sleep(tick_rate) => {
                while event::poll(Duration::from_millis(0)).unwrap_or(false) {
                    if let Ok(evt) = event::read() {
                        handle_input(app, evt, &ws_tx).await?;
                    }
                }
            }

            // WebSocket events - immediate redraw for real-time streaming
            Some(event) = ws_rx.recv() => {
                let needs_redraw = app.handle_ws_event(event);
                if needs_redraw {
                    // Redraw immediately - this is what makes streaming feel real-time
                    terminal.draw(|f| ui::render(f, app))?;
                }
            }

            // Periodic refresh (only when not streaming)
            _ = refresh_interval.tick() => {
                if app.input_mode == InputMode::Normal && !app.is_streaming {
                    if let Err(e) = app.refresh_agents().await {
                        app.refresh_error = Some(format!("Refresh failed: {}", e));
                        tracing::warn!("Failed to refresh agents: {}", e);
                    } else {
                        app.refresh_error = None;
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Cleanup: disconnect if connected
    app.disconnect().await;

    Ok(())
}

/// Handle input events.
async fn handle_input(
    app: &mut App,
    event: Event,
    ws_tx: &mpsc::Sender<WsEvent>,
) -> anyhow::Result<()> {
    match event {
        Event::Key(key) => {
            // Only handle key press events
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }

            // Handle based on input mode
            match app.input_mode {
                InputMode::Normal => {
                    handle_normal_mode(app, key.code, key.modifiers, ws_tx).await?;
                }
                InputMode::CreatingAgent => {
                    handle_create_agent_mode(app, key.code).await?;
                }
                InputMode::ConfirmingDelete => {
                    handle_confirm_delete_mode(app, key.code).await?;
                }
            }
        }
        Event::Mouse(mouse) => {
            // Handle mouse scroll for chat panel
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    app.scroll_chat_up(3);
                }
                MouseEventKind::ScrollDown => {
                    app.scroll_chat_down(3);
                }
                _ => {}
            }
        }
        _ => {}
    }

    Ok(())
}

/// Handle input in normal mode.
///
/// Unified input model:
/// - Up/Down always navigate agents
/// - Typing goes directly to input (when not in command mode)
/// - ESC enters command mode where q/n/d/s/t/r work
async fn handle_normal_mode(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    ws_tx: &mpsc::Sender<WsEvent>,
) -> anyhow::Result<()> {
    // ESC toggles command mode
    if code == KeyCode::Esc {
        if app.command_mode {
            // Exit command mode back to input mode
            app.command_mode = false;
        } else {
            // Enter command mode, or handle streaming/errors first
            if app.is_streaming {
                app.cancel_streaming().await;
            } else if app.error_message.is_some() {
                app.clear_error();
            } else {
                // Enter command mode
                app.command_mode = true;
            }
        }
        return Ok(());
    }

    // Page up/down always work for scrolling
    match code {
        KeyCode::PageUp => {
            app.scroll_chat_up(10);
            return Ok(());
        }
        KeyCode::PageDown => {
            app.scroll_chat_down(10);
            return Ok(());
        }
        _ => {}
    }

    // Up/Down always navigate agents
    match code {
        KeyCode::Up => {
            app.select_prev_agent();
            return Ok(());
        }
        KeyCode::Down => {
            app.select_next_agent();
            return Ok(());
        }
        _ => {}
    }

    if app.command_mode {
        // Command mode: single-key commands for agent actions
        handle_command_mode(app, code, ws_tx).await?;
    } else {
        // Input mode: typing goes to input
        handle_input_mode(app, code, modifiers, ws_tx).await?;
    }

    Ok(())
}

/// Handle input in command mode (ESC was pressed).
/// Single-key commands for agent management.
async fn handle_command_mode(
    app: &mut App,
    code: KeyCode,
    ws_tx: &mpsc::Sender<WsEvent>,
) -> anyhow::Result<()> {
    match code {
        KeyCode::Char('q') => {
            app.should_quit = true;
        }
        KeyCode::Char('n') => {
            // Create new agent - enter dialog mode
            app.enter_dialog_mode(InputMode::CreatingAgent);
        }
        KeyCode::Char('d') => {
            // Delete agent (confirm first)
            if app.selected_agent().is_some() {
                app.enter_dialog_mode(InputMode::ConfirmingDelete);
            }
        }
        KeyCode::Char('s') => {
            // Start agent
            if let Err(e) = app.start_selected_agent().await {
                app.set_error(e.to_string());
            }
        }
        KeyCode::Char('t') => {
            // Stop agent
            if let Err(e) = app.stop_selected_agent().await {
                app.set_error(e.to_string());
            }
        }
        KeyCode::Char('r') => {
            // Restart agent
            if let Err(e) = app.restart_selected_agent().await {
                app.set_error(e.to_string());
            }
        }
        KeyCode::Char('h') => {
            // Hibernate agent
            if let Err(e) = app.hibernate_selected_agent().await {
                app.set_error(e.to_string());
            }
        }
        KeyCode::Char('w') => {
            // Wake agent
            if let Err(e) = app.wake_selected_agent().await {
                app.set_error(e.to_string());
            }
        }
        KeyCode::Char('c') => {
            // Connect to agent (auto-wake/start if needed)
            if app.selected_agent().is_some() && !app.is_connected() {
                // Show immediate feedback about agent state
                show_agent_wake_status(app);
                
                match app.ensure_ready_and_connect().await {
                    Ok(mut rx) => {
                        let ws_tx = ws_tx.clone();
                        tokio::spawn(async move {
                            while let Some(event) = rx.recv().await {
                                if ws_tx.send(event).await.is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    Err(e) => {
                        app.set_error(e);
                    }
                }
            }
        }
        KeyCode::Char('x') => {
            // Disconnect from agent
            if app.is_connected() {
                app.disconnect().await;
            }
        }
        KeyCode::Char('j') => {
            // Scroll chat down (vim-style)
            app.scroll_chat_down(1);
        }
        KeyCode::Char('k') => {
            // Scroll chat up (vim-style)
            app.scroll_chat_up(1);
        }
        KeyCode::Enter => {
            // Exit command mode (convenient way to get back to typing)
            app.command_mode = false;
        }
        _ => {}
    }

    Ok(())
}

/// Handle input in normal input mode (typing goes to input).
async fn handle_input_mode(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    ws_tx: &mpsc::Sender<WsEvent>,
) -> anyhow::Result<()> {
    match code {
        KeyCode::Enter => {
            if !app.input.is_empty() {
                if app.is_connected() {
                    let input = app.take_input();
                    if let Err(e) = app.send_message(input).await {
                        app.set_error(e);
                    }
                } else if app.selected_agent().is_some() {
                    // Show immediate feedback about agent state
                    show_agent_wake_status(app);
                    
                    // Auto-connect (wake/start if needed) and send
                    match app.ensure_ready_and_connect().await {
                        Ok(mut rx) => {
                            let ws_tx = ws_tx.clone();
                            tokio::spawn(async move {
                                while let Some(event) = rx.recv().await {
                                    if ws_tx.send(event).await.is_err() {
                                        break;
                                    }
                                }
                            });
                            // Now send the message
                            let input = app.take_input();
                            if let Err(e) = app.send_message(input).await {
                                app.set_error(e);
                            }
                        }
                        Err(e) => {
                            app.set_error(e);
                        }
                    }
                } else {
                    app.set_error("No agent selected");
                }
            } else if !app.is_connected() && app.selected_agent().is_some() {
                // Show immediate feedback about agent state
                show_agent_wake_status(app);
                
                // Connect on Enter if not connected and input is empty (auto-wake/start)
                match app.ensure_ready_and_connect().await {
                    Ok(mut rx) => {
                        let ws_tx = ws_tx.clone();
                        tokio::spawn(async move {
                            while let Some(event) = rx.recv().await {
                                if ws_tx.send(event).await.is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    Err(e) => {
                        app.set_error(e);
                    }
                }
            }
        }
        KeyCode::Char(c) => {
            // Ctrl+A: move to start
            if modifiers.contains(KeyModifiers::CONTROL) && c == 'a' {
                app.move_cursor_start();
            }
            // Ctrl+E: move to end
            else if modifiers.contains(KeyModifiers::CONTROL) && c == 'e' {
                app.move_cursor_end();
            }
            // Ctrl+U: clear input
            else if modifiers.contains(KeyModifiers::CONTROL) && c == 'u' {
                app.clear_input();
            }
            // Ctrl+W: delete word
            else if modifiers.contains(KeyModifiers::CONTROL) && c == 'w' {
                while app.cursor_position > 0 {
                    app.delete_char();
                    if app.cursor_position > 0 {
                        let prev_char = app.input.chars().nth(app.cursor_position - 1);
                        if prev_char == Some(' ') {
                            break;
                        }
                    }
                }
            } else {
                app.insert_char(c);
            }
        }
        KeyCode::Backspace => {
            app.delete_char();
        }
        KeyCode::Delete => {
            app.delete_char_forward();
        }
        KeyCode::Left => {
            app.move_cursor_left();
        }
        KeyCode::Right => {
            app.move_cursor_right();
        }
        KeyCode::Home => {
            app.move_cursor_start();
        }
        KeyCode::End => {
            app.move_cursor_end();
        }
        _ => {}
    }

    Ok(())
}

/// Handle input in create agent mode.
async fn handle_create_agent_mode(app: &mut App, code: KeyCode) -> anyhow::Result<()> {
    match code {
        KeyCode::Esc => {
            // Exit dialog mode (restores chat input)
            app.exit_dialog_mode();
        }
        KeyCode::Enter => {
            if !app.input.is_empty() {
                let name = app.take_input();
                if let Err(e) = app.create_agent(&name).await {
                    app.set_error(e.to_string());
                }
            }
            // Exit dialog mode (restores chat input)
            app.exit_dialog_mode();
        }
        KeyCode::Char(c) => {
            // Only allow valid agent name characters
            if c.is_alphanumeric() || c == '-' || c == '_' {
                app.insert_char(c);
            }
        }
        KeyCode::Backspace => {
            app.delete_char();
        }
        _ => {}
    }

    Ok(())
}

/// Handle input in confirm delete mode.
async fn handle_confirm_delete_mode(app: &mut App, code: KeyCode) -> anyhow::Result<()> {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            if let Err(e) = app.delete_selected_agent().await {
                app.set_error(e.to_string());
            }
            // Exit dialog mode (restores chat input)
            app.exit_dialog_mode();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            // Exit dialog mode (restores chat input)
            app.exit_dialog_mode();
        }
        _ => {}
    }

    Ok(())
}

/// Show immediate status feedback when connecting to an agent that may need waking/starting.
fn show_agent_wake_status(app: &mut App) {
    if let Some(agent) = app.selected_agent() {
        match agent.status {
            AgentState::Running | AgentState::Idle => {
                app.set_status("Connecting...");
            }
            AgentState::Hibernating => {
                app.set_status(format!("Waking agent '{}'...", agent.name));
            }
            AgentState::Stopped => {
                app.set_status(format!("Starting agent '{}'...", agent.name));
            }
            AgentState::Provisioning => {
                app.set_status(format!("Agent '{}' is provisioning, please wait...", agent.name));
            }
            AgentState::Stopping => {
                app.set_status(format!("Agent '{}' is stopping, please wait...", agent.name));
            }
            AgentState::Error => {
                app.set_status(format!("Restarting failed agent '{}'...", agent.name));
            }
        }
    }
}

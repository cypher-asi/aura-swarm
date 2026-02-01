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

use app::{App, Focus, InputMode, REFRESH_INTERVAL};
use client::GatewayClient;
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
async fn handle_normal_mode(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    ws_tx: &mpsc::Sender<WsEvent>,
) -> anyhow::Result<()> {
    // Check if we can use command keys (q to quit)
    // Only allowed when: in Agents panel, OR in Chat panel but NOT in insert mode
    let can_use_commands = app.focus == Focus::Agents || !app.chat_insert_mode;

    // Global keybindings
    match code {
        KeyCode::Char('q') if can_use_commands => {
            app.should_quit = true;
            return Ok(());
        }
        KeyCode::Tab => {
            app.focus = app.focus.next();
            // Enter insert mode when switching to Chat
            if app.focus == Focus::Chat {
                app.chat_insert_mode = true;
            }
            return Ok(());
        }
        KeyCode::BackTab => {
            app.focus = app.focus.prev();
            // Enter insert mode when switching to Chat
            if app.focus == Focus::Chat {
                app.chat_insert_mode = true;
            }
            return Ok(());
        }
        KeyCode::Esc => {
            // In Chat panel: first exit insert mode, then handle other Esc actions
            if app.focus == Focus::Chat && app.chat_insert_mode {
                app.chat_insert_mode = false;
                return Ok(());
            }
            // Cancel streaming, clear error, input, or disconnect (in priority order)
            if app.is_streaming {
                app.cancel_streaming().await;
            } else if app.error_message.is_some() {
                app.clear_error();
            } else if !app.input.is_empty() {
                app.clear_input();
            } else if app.is_connected() {
                app.disconnect().await;
            }
            return Ok(());
        }
        _ => {}
    }

    // Panel-specific keybindings
    match app.focus {
        Focus::Agents => {
            handle_agents_panel_input(app, code, ws_tx).await?;
        }
        Focus::Chat => {
            // Right column: handle both chat scrolling and input
            handle_chat_column_input(app, code, modifiers, ws_tx).await?;
        }
    }

    Ok(())
}

/// Handle input for the agents panel.
async fn handle_agents_panel_input(
    app: &mut App,
    code: KeyCode,
    ws_tx: &mpsc::Sender<WsEvent>,
) -> anyhow::Result<()> {
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.select_prev_agent();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.select_next_agent();
        }
        KeyCode::Enter => {
            // Connect to agent
            if app.selected_agent().is_some() {
                match app.connect_to_agent().await {
                    Ok(mut rx) => {
                        // Spawn task to forward WebSocket events to the main channel
                        let ws_tx = ws_tx.clone();
                        tokio::spawn(async move {
                            while let Some(event) = rx.recv().await {
                                if ws_tx.send(event).await.is_err() {
                                    break;
                                }
                            }
                        });
                        app.focus = Focus::Chat;
                    }
                    Err(e) => {
                        app.set_error(e);
                    }
                }
            }
        }
        KeyCode::Char('n') => {
            // Create new agent - enter dialog mode (saves chat input)
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
        _ => {}
    }

    Ok(())
}

/// Handle input for the chat column (right side: chat + input).
/// Vim-like: must be in insert mode to type. Press 'i' to enter insert mode, Esc to exit.
async fn handle_chat_column_input(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    ws_tx: &mpsc::Sender<WsEvent>,
) -> anyhow::Result<()> {
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

    // If not in insert mode, only 'i' or Enter enters insert mode
    if !app.chat_insert_mode {
        match code {
            KeyCode::Char('i') => {
                app.chat_insert_mode = true;
            }
            KeyCode::Enter => {
                // Enter also enters insert mode and connects if needed
                app.chat_insert_mode = true;
                if !app.is_connected() && app.selected_agent().is_some() {
                    match app.connect_to_agent().await {
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
            KeyCode::Up | KeyCode::Char('k') => {
                app.scroll_chat_up(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.scroll_chat_down(1);
            }
            _ => {}
        }
        return Ok(());
    }

    // In insert mode: handle text input
    match code {
        KeyCode::Enter => {
            if !app.input.is_empty() {
                if app.is_connected() {
                    let input = app.take_input();
                    if let Err(e) = app.send_message(input).await {
                        app.set_error(e);
                    }
                } else {
                    app.set_error("Not connected to agent. Press Enter to connect first.");
                }
            } else if !app.is_connected() && app.selected_agent().is_some() {
                // Connect on Enter if not connected
                match app.connect_to_agent().await {
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

//! Aura Swarm Deploy TUI — `aswarm-deploy`.
//!
//! A terminal UI for running the staged bash deploy scripts in `deploy/`.
//! The left column shows a polished, curated step-progress feed (mirrored from
//! `deploy/_lib.sh`); the right column shows what the machine/process is doing
//! right now: a live raw-output firehose on top and periodic, read-only
//! infra-state snapshots on the bottom.
//!
//! The scripts are bash, so this must run where `bash` and your cloud tooling
//! (kubectl/terraform/aws) live. On Windows, run it under WSL.

mod app;
mod runner;
mod scripts;
mod ui;
mod watch;

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use app::{App, Screen};
use runner::RunHandle;

/// Aura Swarm deploy-script runner TUI.
#[derive(Parser, Debug)]
#[command(name = "aswarm-deploy")]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Directory containing the deploy scripts.
    #[arg(long, env = "AURA_DEPLOY_DIR", default_value = "deploy")]
    deploy_dir: PathBuf,

    /// Read-only command polled for the live infra-state pane.
    #[arg(long, env = "AURA_DEPLOY_WATCH")]
    watch: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let deploy_dir = resolve_deploy_dir(&args.deploy_dir)?;
    let scripts = scripts::discover(&deploy_dir)
        .with_context(|| format!("failed to scan {}", deploy_dir.display()))?;
    if scripts.is_empty() {
        anyhow::bail!("no *.sh scripts found in {}", deploy_dir.display());
    }

    let watch_cmd = args.watch.unwrap_or_else(watch::default_watch);
    let app = App::new(deploy_dir, scripts, watch_cmd);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, app).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: App,
) -> anyhow::Result<()> {
    let mut runner: Option<RunHandle> = None;

    let (snap_tx, mut snap_rx) = mpsc::channel::<String>(8);
    let mut snapshot_in_flight = false;

    let mut events = EventStream::new();
    let mut snapshot_timer = tokio::time::interval(Duration::from_secs(5));
    snapshot_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        terminal.draw(|f| ui::render(f, &app))?;

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(evt)) => handle_terminal_event(&mut app, &evt, &mut runner),
                    Some(Err(_)) | None => break,
                }
            }

            maybe_run = recv_run_event(&mut runner) => {
                if let Some(event) = maybe_run {
                    app.apply_event(event);
                }
            }

            Some(text) = snap_rx.recv() => {
                snapshot_in_flight = false;
                app.set_snapshot(text);
            }

            _ = snapshot_timer.tick() => {
                if app.screen != Screen::Picker && !snapshot_in_flight {
                    snapshot_in_flight = true;
                    let cmd = app.watch_cmd.clone();
                    let tx = snap_tx.clone();
                    tokio::spawn(async move {
                        let out = watch::run_snapshot(cmd).await;
                        let _ = tx.send(out).await;
                    });
                }
            }

            () = tokio::time::sleep(Duration::from_millis(120)) => {
                app.spinner_tick = app.spinner_tick.wrapping_add(1);
            }
        }

        if app.request_run {
            app.request_run = false;
            start_run(&mut app, &mut runner);
        }

        if app.should_quit {
            break;
        }
    }

    if let Some(mut h) = runner.take() {
        h.cancel();
    }
    Ok(())
}

/// Resolve the deploy-scripts directory.
///
/// Tries the path as given (absolute, or relative to the current dir) and, for
/// a relative path, walks up the ancestors of the working directory so the TUI
/// can be launched from anywhere inside the repo, not just its root.
fn resolve_deploy_dir(requested: &Path) -> anyhow::Result<PathBuf> {
    if let Ok(p) = requested.canonicalize() {
        if p.is_dir() {
            return Ok(p);
        }
    }
    if requested.is_relative() {
        let cwd = std::env::current_dir().context("cannot read the current directory")?;
        for base in cwd.ancestors() {
            if let Ok(p) = base.join(requested).canonicalize() {
                if p.is_dir() {
                    return Ok(p);
                }
            }
        }
    }
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    anyhow::bail!(
        "deploy dir '{}' not found (searched from cwd: {cwd}). \
         Pass --deploy-dir <path> or run from inside the repo.",
        requested.display()
    )
}

/// Await the next runner event, or pend forever when nothing is running.
async fn recv_run_event(runner: &mut Option<RunHandle>) -> Option<runner::RunEvent> {
    match runner.as_mut() {
        Some(h) => h.events.recv().await,
        None => std::future::pending().await,
    }
}

fn start_run(app: &mut App, runner: &mut Option<RunHandle>) {
    let Some(script) = app.selected_script().cloned() else {
        return;
    };
    let cli_args: Vec<String> = app
        .args_input
        .split_whitespace()
        .map(ToString::to_string)
        .collect();
    match runner::spawn(&script, &cli_args, &app.deploy_dir) {
        Ok(handle) => {
            app.begin_run(script.file_name.clone());
            *runner = Some(handle);
        }
        Err(e) => app.error = Some(e.to_string()),
    }
}

fn handle_terminal_event(app: &mut App, event: &Event, runner: &mut Option<RunHandle>) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(app, key.code, key.modifiers, runner);
        }
        Event::Mouse(m) => match m.kind {
            MouseEventKind::ScrollUp => app.scroll_up(3),
            MouseEventKind::ScrollDown => app.scroll_down(3),
            _ => {}
        },
        _ => {}
    }
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers, runner: &mut Option<RunHandle>) {
    let ctrl_c = mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c');
    if ctrl_c {
        if let Some(h) = runner.as_mut() {
            h.cancel();
        }
        app.should_quit = true;
        return;
    }

    match app.screen {
        Screen::Picker => handle_picker(app, code),
        Screen::Running => handle_running(app, code, runner),
        Screen::Finished => handle_finished(app, code, runner),
    }
}

fn handle_picker(app: &mut App, code: KeyCode) {
    if app.editing_args {
        match code {
            KeyCode::Char(c) => app.args_input.push(c),
            KeyCode::Backspace => {
                app.args_input.pop();
            }
            KeyCode::Enter | KeyCode::Esc => app.editing_args = false,
            _ => {}
        }
        return;
    }

    match code {
        KeyCode::Up => app.select_prev(),
        KeyCode::Down => app.select_next(),
        KeyCode::Char('e') => app.editing_args = true,
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Enter => app.request_run = true,
        _ => {}
    }
}

fn handle_running(app: &mut App, code: KeyCode, runner: &mut Option<RunHandle>) {
    match code {
        KeyCode::Esc => {
            if let Some(h) = runner.as_mut() {
                h.cancel();
            }
        }
        KeyCode::Char('q') => {
            if let Some(h) = runner.as_mut() {
                h.cancel();
            }
            app.should_quit = true;
        }
        _ => handle_scroll(app, code),
    }
}

fn handle_finished(app: &mut App, code: KeyCode, runner: &mut Option<RunHandle>) {
    match code {
        KeyCode::Enter | KeyCode::Esc => {
            *runner = None;
            app.back_to_picker();
        }
        KeyCode::Char('q') => app.should_quit = true,
        _ => handle_scroll(app, code),
    }
}

fn handle_scroll(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Tab => app.toggle_focus(),
        KeyCode::Up | KeyCode::Char('k') => app.scroll_up(1),
        KeyCode::Down | KeyCode::Char('j') => app.scroll_down(1),
        KeyCode::PageUp => app.scroll_up(10),
        KeyCode::PageDown => app.scroll_down(10),
        KeyCode::Home => app.scroll_up(usize::MAX / 2),
        KeyCode::End => app.jump_to_tail(),
        _ => {}
    }
}

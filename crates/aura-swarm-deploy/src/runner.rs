//! Spawns a deploy script and streams its output back to the app.
//!
//! Two independent streams are surfaced:
//! - the raw stdout/stderr of the script (and the tools it invokes), and
//! - a structured "curated progress" feed written by `deploy/_lib.sh` to the
//!   channel file we point it at via `DEPLOY_TUI_CHANNEL` (see `_tui_emit`).
//!
//! Records on the structured channel are line-delimited and field-separated by
//! the ASCII Unit Separator (`0x1f`): `<level><US><step-id><US><text>`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::scripts::DeployScript;

const US: char = '\u{1f}';

/// An event produced while a deploy script runs.
#[derive(Debug, Clone)]
pub enum RunEvent {
    /// A structured curated-log record from `_lib.sh` (left progress column).
    Progress {
        level: String,
        step: String,
        text: String,
    },
    /// A raw stdout/stderr line from the script or an underlying command.
    Raw(String),
    /// A `log_watch` hint: a read-only command the infra-state pane should poll.
    Watch(String),
    /// The process finished; payload is the exit code (`None` if killed/unknown).
    Exited(Option<i32>),
}

/// Handle to a running script: a stream of [`RunEvent`]s plus a cancel signal.
pub struct RunHandle {
    pub events: mpsc::Receiver<RunEvent>,
    cancel: Option<oneshot::Sender<()>>,
}

impl RunHandle {
    /// Request cancellation of the running script (best effort; idempotent).
    pub fn cancel(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
    }
}

/// Spawn `bash <script> [args]` in `deploy_dir`, wiring up the structured
/// progress channel and streaming all output through the returned handle.
pub fn spawn(
    script: &DeployScript,
    args: &[String],
    deploy_dir: &Path,
) -> anyhow::Result<RunHandle> {
    let (tx, rx) = mpsc::channel::<RunEvent>(1024);
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();

    let channel_path = make_channel_path();
    std::fs::write(&channel_path, b"").with_context(|| {
        format!(
            "failed to create TUI progress channel at {}",
            channel_path.display()
        )
    })?;

    let mut cmd = make_bash_command(&script.path, args, deploy_dir, &channel_path);
    let mut child = cmd
        .spawn()
        .context("failed to spawn `bash` (is bash on PATH? on Windows you need WSL or Git Bash)")?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    tokio::spawn(read_raw(stdout, tx.clone()));
    tokio::spawn(read_raw(stderr, tx.clone()));

    let (tail_stop_tx, tail_stop_rx) = oneshot::channel::<()>();
    tokio::spawn(tail_channel(channel_path.clone(), tx.clone(), tail_stop_rx));

    tokio::spawn(async move {
        let code = tokio::select! {
            status = child.wait() => status.ok().and_then(|s| s.code()),
            _ = cancel_rx => {
                let _ = child.start_kill();
                child.wait().await.ok().and_then(|s| s.code())
            }
        };
        // Give the channel tailer a beat to flush the final step_ok/step_fail.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = tail_stop_tx.send(());
        let _ = tx.send(RunEvent::Exited(code)).await;
        let _ = std::fs::remove_file(&channel_path);
    });

    Ok(RunHandle {
        events: rx,
        cancel: Some(cancel_tx),
    })
}

/// Read newline-delimited raw output, stripping ANSI escapes for display.
async fn read_raw<R>(reader: R, tx: mpsc::Sender<RunEvent>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if tx.send(RunEvent::Raw(strip_ansi(&line))).await.is_err() {
            break;
        }
    }
}

/// Tail the structured progress channel file until signaled to stop, parsing
/// each appended record into a [`RunEvent`].
async fn tail_channel(path: PathBuf, tx: mpsc::Sender<RunEvent>, mut stop: oneshot::Receiver<()>) {
    let Ok(file) = tokio::fs::File::open(&path).await else {
        return;
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut stopping = false;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await.unwrap_or(0);
        if n == 0 {
            if stopping {
                break;
            }
            match stop.try_recv() {
                Ok(()) | Err(oneshot::error::TryRecvError::Closed) => {
                    stopping = true; // one more drain pass, then break on next EOF
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                }
            }
            continue;
        }
        if let Some(event) = parse_record(line.trim_end_matches(['\n', '\r'])) {
            if tx.send(event).await.is_err() {
                break;
            }
        }
    }
}

/// Parse one `<level><US><step><US><text>` record.
fn parse_record(rec: &str) -> Option<RunEvent> {
    if rec.is_empty() {
        return None;
    }
    let mut parts = rec.splitn(3, US);
    let level = parts.next()?.to_string();
    let step = parts.next().unwrap_or("").to_string();
    let text = parts.next().unwrap_or("").to_string();
    if level == "watch" {
        return Some(RunEvent::Watch(text));
    }
    Some(RunEvent::Progress { level, step, text })
}

/// Build the `bash` command that runs the script, translating paths for WSL
/// when the Windows-native binary is driving `bash.exe` (WSL2).
///
/// On Windows, `bash` is typically WSL's launcher, which cannot resolve Windows
/// paths like `C:\…`. If `wslpath` is available we convert the script, working
/// directory, and channel path to `/mnt/c/…` form and `cd` into the dir before
/// exec'ing the script. Everywhere else (Linux, macOS, Windows Git Bash) we
/// pass native paths directly.
fn make_bash_command(
    script_path: &Path,
    args: &[String],
    deploy_dir: &Path,
    channel_path: &Path,
) -> Command {
    let mut cmd = Command::new("bash");

    #[cfg(windows)]
    if let (Some(wsl_script), Some(wsl_dir), Some(wsl_chan)) = (
        wslpath(script_path),
        wslpath(deploy_dir),
        wslpath(channel_path),
    ) {
        let inner = format!(
            "cd {} && exec bash {} \"$@\"",
            sh_single_quote(&wsl_dir),
            sh_single_quote(&wsl_script),
        );
        cmd.arg("-c")
            .arg(inner)
            .arg("aswarm-deploy") // $0 for the inner shell
            .args(args)
            .env("DEPLOY_TUI", "1")
            .env("DEPLOY_TUI_CHANNEL", wsl_chan)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        return cmd;
    }

    cmd.arg(script_path)
        .args(args)
        .current_dir(deploy_dir)
        .env("DEPLOY_TUI", "1")
        .env("DEPLOY_TUI_CHANNEL", channel_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Convert a Windows path to its WSL (`/mnt/c/…`) form via `wslpath`.
/// Returns `None` if `wslpath` is unavailable (e.g. Git Bash), so the caller
/// falls back to passing the native path.
#[cfg(windows)]
fn wslpath(path: &Path) -> Option<String> {
    let win = path.to_str()?;
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(format!("wslpath -a -u {}", sh_single_quote(win)))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Single-quote a string for safe interpolation into a bash command line.
#[cfg(windows)]
fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A unique temp path for this run's structured progress channel.
fn make_channel_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("aura-deploy-tui-{}-{nanos}.chan", std::process::id()))
}

/// Remove ANSI/VT escape sequences so raw output renders cleanly in the TUI.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if ('@'..='~').contains(&nc) {
                        break;
                    }
                }
            } else {
                chars.next(); // charset/keypad designators (ESC + one byte)
            }
        } else {
            out.push(c);
        }
    }
    out
}

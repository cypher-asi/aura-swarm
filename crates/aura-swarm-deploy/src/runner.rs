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

/// Build the `bash` command that runs the script, translating paths to match
/// the actual `bash` interpreter on Windows.
///
/// On Windows a `bash` on PATH can be either the WSL launcher (which resolves
/// Windows paths as `/mnt/c/…` via `wslpath`) or Git Bash/Cygwin (which use
/// `/c/…` via `cygpath`). Feeding the wrong flavor a path it can't resolve is
/// what caused `cd: /mnt/c/…: No such file or directory` when launched from Git
/// Bash. We therefore resolve a single concrete `bash.exe`, detect *its* flavor,
/// convert the script/working-dir/channel paths with the matching tool, and run
/// that same binary. Everywhere else (Linux, macOS) we pass native paths.
fn make_bash_command(
    script_path: &Path,
    args: &[String],
    deploy_dir: &Path,
    channel_path: &Path,
) -> Command {
    #[cfg(windows)]
    if let Some(bash) = resolve_bash() {
        let tool = match bash_flavor(&bash) {
            BashFlavor::Wsl => Some("wslpath"),
            BashFlavor::Msys => Some("cygpath"),
            BashFlavor::Other => None,
        };
        if let Some(tool) = tool {
            if let (Some(u_script), Some(u_dir), Some(u_chan)) = (
                to_unix_path(&bash, tool, script_path),
                to_unix_path(&bash, tool, deploy_dir),
                to_unix_path(&bash, tool, channel_path),
            ) {
                let inner = format!(
                    "cd {} && exec bash {} \"$@\"",
                    sh_single_quote(&u_dir),
                    sh_single_quote(&u_script),
                );
                let mut cmd = Command::new(&bash);
                cmd.arg("-c")
                    .arg(inner)
                    .arg("aswarm-deploy") // $0 for the inner shell
                    .args(args)
                    .env("DEPLOY_TUI", "1")
                    .env("DEPLOY_TUI_CHANNEL", u_chan)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                return cmd;
            }
        }
    }

    let mut cmd = Command::new(bash_program());
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

/// The `bash` program to spawn: a concrete `bash.exe` resolved from PATH on
/// Windows (so every pane uses the same interpreter), or plain `bash` elsewhere.
pub(crate) fn bash_program() -> std::ffi::OsString {
    #[cfg(windows)]
    if let Some(p) = resolve_bash() {
        return p.into_os_string();
    }
    std::ffi::OsString::from("bash")
}

/// Which `bash` flavor we're driving, which decides how Windows paths are
/// translated for the interpreter.
#[cfg(windows)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum BashFlavor {
    /// WSL: convert via `wslpath` to `/mnt/c/…`.
    Wsl,
    /// Git Bash / MSYS / Cygwin: convert via `cygpath` to `/c/…`.
    Msys,
    /// Unknown: pass native paths and let the shell sort it out.
    Other,
}

/// Resolve a concrete `bash.exe` by scanning the inherited `PATH` in order.
///
/// Returns the first existing match, so launching from Git Bash picks Git Bash
/// and launching from PowerShell/WSL picks the WSL launcher. Using one resolved
/// binary for both the flavor probe and the run avoids the probe and runner
/// disagreeing about which interpreter is in play.
#[cfg(windows)]
fn resolve_bash() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("bash.exe"))
        .find(|candidate| candidate.is_file())
}

/// Detect the flavor of `bash` by inspecting `uname -s`.
#[cfg(windows)]
fn bash_flavor(bash: &Path) -> BashFlavor {
    let Ok(out) = std::process::Command::new(bash)
        .arg("-c")
        .arg("uname -s")
        .output()
    else {
        return BashFlavor::Other;
    };
    if !out.status.success() {
        return BashFlavor::Other;
    }
    let kernel = String::from_utf8_lossy(&out.stdout)
        .trim()
        .to_ascii_uppercase();
    if kernel.starts_with("LINUX") {
        BashFlavor::Wsl
    } else if kernel.starts_with("MINGW")
        || kernel.starts_with("MSYS")
        || kernel.starts_with("CYGWIN")
    {
        BashFlavor::Msys
    } else {
        BashFlavor::Other
    }
}

/// Convert a Windows path to the unix form the interpreter understands, using
/// `tool` (`wslpath` for WSL, `cygpath` for Git Bash/Cygwin). Returns `None`
/// if the conversion fails, so the caller can fall back to native paths.
#[cfg(windows)]
fn to_unix_path(bash: &Path, tool: &str, path: &Path) -> Option<String> {
    let raw = path.to_str()?;
    // `Path::canonicalize` on Windows yields verbatim paths (`\\?\C:\...`) which
    // both converters mistranslate; strip the prefix first.
    let win = raw.strip_prefix(r"\\?\UNC\").map_or_else(
        || raw.strip_prefix(r"\\?\").unwrap_or(raw).to_string(),
        |rest| format!(r"\\{rest}"),
    );
    let out = std::process::Command::new(bash)
        .arg("-c")
        .arg(format!("{tool} -a -u {}", sh_single_quote(&win)))
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

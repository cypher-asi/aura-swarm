//! Periodic, read-only infra-state snapshots for the right-bottom pane.
//!
//! These are deliberately read-only (`get`/`describe`/`show`) so polling never
//! mutates anything. Early rollout steps run before the cluster exists, so a
//! failing command is surfaced as plain text rather than treated as an error.

use std::process::Stdio;

use tokio::process::Command;

/// The default snapshot command. Combined into a single `bash -c` so callers
/// only manage one child. Errors are captured (`2>&1`) and shown verbatim.
pub fn default_watch() -> String {
    "kubectl get nodes -o wide 2>&1 | head -n 15; \
     echo; \
     kubectl get pods -A 2>&1 | head -n 40"
        .to_string()
}

/// Run a read-only snapshot command and return its combined output text.
pub async fn run_snapshot(cmd: String) -> String {
    let result = Command::new("bash")
        .arg("-c")
        .arg(&cmd)
        .stdin(Stdio::null())
        .output()
        .await;

    match result {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if !stdout.trim().is_empty() {
                return stdout.into_owned();
            }
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.trim().is_empty() {
                "(no output)".to_string()
            } else {
                stderr.into_owned()
            }
        }
        Err(e) => format!("snapshot command failed to run: {e}"),
    }
}

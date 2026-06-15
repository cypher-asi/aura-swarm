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

/// Run a read-only snapshot command and return its (condensed) output text.
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
            let body = if stdout.trim().is_empty() {
                String::from_utf8_lossy(&out.stderr).into_owned()
            } else {
                stdout.into_owned()
            };
            condense(&body)
        }
        Err(e) => format!(
            "snapshot command failed to run: {e}\n(is `bash` on PATH? on Windows, run under WSL)"
        ),
    }
}

/// Collapse common "cluster not up yet" noise into a single calm line, so the
/// pane stays readable during early steps that build the cluster/node group.
fn condense(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let cluster_down = [
        "connection refused",
        "was refused",
        "unable to connect to the server",
        "couldn't get current server api group list",
        "no such host",
        "i/o timeout",
        "the server doesn't have a resource type",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    if cluster_down {
        return "Kubernetes API not reachable yet.\n\
                Infra snapshots populate once the cluster and kubeconfig are available\n\
                (expected during early steps like 02 that build the node group).\n\
                Override the polled command with --watch / AURA_DEPLOY_WATCH."
            .to_string();
    }

    if text.trim().is_empty() {
        "(no output)".to_string()
    } else {
        text.to_string()
    }
}

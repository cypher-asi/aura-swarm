//! Discovery and metadata for the staged deploy scripts in `deploy/`.

use std::path::{Path, PathBuf};

/// A discovered, runnable deploy script.
#[derive(Clone, Debug)]
pub struct DeployScript {
    /// Absolute path to the script.
    pub path: PathBuf,
    /// Bare file name, e.g. `07-deploy-r1.sh` (used for sorting and display).
    pub file_name: String,
    /// Short human description pulled from the script's header comment.
    pub title: String,
}

/// Discover runnable `*.sh` scripts directly inside `deploy_dir`.
///
/// Sorted by file name so the numbered rollout steps (00..13) appear in order.
/// Library/partials (names starting with `_`, e.g. `_lib.sh`) are skipped, and
/// the search is intentionally non-recursive (the `legacy/` set is excluded).
pub fn discover(deploy_dir: &Path) -> std::io::Result<Vec<DeployScript>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(deploy_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()).map(ToString::to_string)
        else {
            continue;
        };
        let is_sh = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("sh"));
        if !is_sh || name.starts_with('_') {
            continue;
        }
        let title = parse_title(&path).unwrap_or_else(|| name.clone());
        out.push(DeployScript {
            path,
            file_name: name,
            title,
        });
    }
    out.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(out)
}

/// Pull a short title from the script's leading comment block, e.g.
/// `# 07-deploy-r1.sh - Build, push and deploy ...` yields the text after the
/// dash (first sentence only), falling back to the first comment line.
fn parse_title(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let line = line.trim_start();
        if line.starts_with("#!") {
            continue;
        }
        let Some(rest) = line.strip_prefix('#') else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() {
            continue;
        }
        let desc = rest.split_once(" - ").map_or(rest, |(_, d)| d);
        return Some(first_sentence(desc));
    }
    None
}

/// Keep just the first sentence so picker rows stay on one line.
fn first_sentence(s: &str) -> String {
    let s = s.trim();
    let cut = s.find(". ").map_or(s.len(), |i| i + 1);
    s[..cut].trim().to_string()
}

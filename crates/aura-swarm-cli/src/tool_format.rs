//! Tool result formatting for display in the TUI.
//!
//! Parses structured JSON tool results from the agent runtime and formats
//! them as human-readable text for the chat panel.

use base64::Engine;

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

        let stdout = decode_base64(stdout_b64);
        let stderr = decode_base64(stderr_b64);

        Some(Self {
            tool,
            ok,
            stdout,
            stderr,
        })
    }

    /// Format the result for display.
    fn format(&self) -> String {
        if !self.ok {
            let error_msg = if self.stderr.is_empty() {
                "Unknown error".to_string()
            } else {
                self.stderr.trim().to_string()
            };
            return format!("Error: {error_msg}");
        }

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
            let entries: Vec<&str> = self.stdout.lines().collect();
            if entries.is_empty() {
                "(empty directory)".to_string()
            } else {
                let count = entries.len();
                let preview: String = entries
                    .iter()
                    .take(10)
                    .map(|e| format!("  {e}"))
                    .collect::<Vec<_>>()
                    .join("\n");
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
            let content = self.stdout.trim();
            let lines: Vec<&str> = content.lines().collect();
            if lines.len() > 20 {
                let preview: String = lines
                    .iter()
                    .take(20)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{preview}\n... ({} more lines)", lines.len() - 20)
            } else {
                content.to_string()
            }
        }
    }

    fn format_write(&self) -> String {
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
        .unwrap_or_else(|| input.to_string())
}

/// Format a tool result for display.
pub(crate) fn format_tool_result(result: &str) -> String {
    ToolResult::parse(result)
        .map(|r| r.format())
        .unwrap_or_else(|| {
            if result.len() > 500 {
                format!("{}...", &result[..500])
            } else {
                result.to_string()
            }
        })
}

/// Format tool arguments like a shell command for display.
/// Always returns something meaningful — at minimum the tool name.
pub(crate) fn format_tool_args(tool_name: &str, args: &serde_json::Value) -> String {
    let obj = args.as_object();

    let get_str =
        |field: &str| -> Option<&str> { obj.and_then(|o| o.get(field)).and_then(|v| v.as_str()) };

    let formatted = match tool_name {
        "fs.ls" | "fs_ls" => {
            let path = get_str("path").unwrap_or(".");
            format!("ls {path}")
        }
        "fs.stat" | "fs_stat" => {
            let path = get_str("path").unwrap_or(".");
            format!("stat {path}")
        }
        "fs.read" | "fs_read" => get_str("path")
            .map(|p| format!("read {p}"))
            .unwrap_or_default(),
        "fs.write" | "fs_write" => {
            let path = get_str("path");
            let content = get_str("content").unwrap_or("");
            match path {
                Some(p) => {
                    let lines = content.lines().count();
                    let chars = content.len();
                    format!("write {p} ({lines} lines, {chars} chars)")
                }
                None => String::new(),
            }
        }
        "fs.mkdir" | "fs_mkdir" => get_str("path")
            .map(|p| format!("mkdir {p}"))
            .unwrap_or_default(),
        "fs.rm" | "fs_rm" => get_str("path")
            .map(|p| format!("rm {p}"))
            .unwrap_or_default(),
        "search_code" | "code_search" | "grep" => {
            let query = get_str("query")
                .or_else(|| get_str("pattern"))
                .or_else(|| get_str("search"))
                .unwrap_or("?");
            let path = get_str("path")
                .or_else(|| get_str("directory"))
                .unwrap_or(".");
            format!("search \"{query}\" in {path}")
        }
        "find_files" | "glob" | "find" => {
            let pattern = get_str("pattern")
                .or_else(|| get_str("glob"))
                .unwrap_or("*");
            let path = get_str("path")
                .or_else(|| get_str("directory"))
                .unwrap_or(".");
            format!("find {pattern} in {path}")
        }
        name if name.eq_ignore_ascii_case("cmd.run")
            || name.eq_ignore_ascii_case("cmd_run")
            || name.eq_ignore_ascii_case("bash")
            || name.eq_ignore_ascii_case("shell")
            || name.eq_ignore_ascii_case("exec") =>
        {
            let cmd = get_str("command")
                .or_else(|| get_str("cmd"))
                .or_else(|| get_str("input"))
                .or_else(|| get_str("script"));

            match cmd {
                Some(c) if c.len() > 80 => format!("$ {}...", &c[..80]),
                Some(c) => format!("$ {c}"),
                None => String::new(),
            }
        }
        _ => String::new(),
    };

    if !formatted.is_empty() {
        return formatted;
    }

    if args.is_null() {
        return tool_name.to_string();
    }

    let compact = serde_json::to_string(args).unwrap_or_default();
    if compact == "{}" || compact.is_empty() || compact == "null" {
        tool_name.to_string()
    } else if compact.len() > 60 {
        format!("{tool_name} {}...", &compact[..60])
    } else {
        format!("{tool_name} {compact}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    fn b64(s: &str) -> String {
        STANDARD.encode(s)
    }

    fn make_tool_json(tool: &str, ok: bool, stdout: &str, stderr: &str) -> String {
        serde_json::json!({
            "tool": tool,
            "ok": ok,
            "stdout": b64(stdout),
            "stderr": b64(stderr),
        })
        .to_string()
    }

    // =========================================================================
    // decode_base64
    // =========================================================================

    #[test]
    fn decode_base64_valid() {
        assert_eq!(decode_base64("SGVsbG8="), "Hello");
    }

    #[test]
    fn decode_base64_invalid() {
        assert_eq!(decode_base64("not-base64!!!"), "not-base64!!!");
    }

    #[test]
    fn decode_base64_empty() {
        assert_eq!(decode_base64(""), "");
    }

    // =========================================================================
    // format_tool_result  (via ToolResult::parse + format)
    // =========================================================================

    #[test]
    fn format_tool_result_fs_ls_few_lines() {
        let json = make_tool_json("fs.ls", true, "file1.txt\nfile2.txt\ndir1", "");
        let output = format_tool_result(&json);
        assert!(output.contains("file1.txt"), "output: {output}");
        assert!(output.contains("file2.txt"), "output: {output}");
        assert!(output.contains("dir1"), "output: {output}");
    }

    #[test]
    fn format_tool_result_cmd_run_empty() {
        let json = make_tool_json("cmd.run", true, "", "");
        let output = format_tool_result(&json);
        assert_eq!(output, "(no output)");
    }

    #[test]
    fn format_tool_result_error_with_stderr() {
        let json = make_tool_json("cmd.run", false, "", "permission denied");
        let output = format_tool_result(&json);
        assert!(output.starts_with("Error:"), "output: {output}");
        assert!(output.contains("permission denied"), "output: {output}");
    }

    #[test]
    fn format_tool_result_invalid_json() {
        let raw = "this is not json at all";
        let output = format_tool_result(raw);
        assert_eq!(output, raw);
    }

    #[test]
    fn format_tool_result_invalid_json_long_truncated() {
        let raw = "x".repeat(600);
        let output = format_tool_result(&raw);
        assert!(output.len() < raw.len(), "should be truncated");
        assert!(output.ends_with("..."), "output: {output}");
    }

    // =========================================================================
    // ToolResult::parse
    // =========================================================================

    #[test]
    fn tool_result_parse_valid() {
        let json = make_tool_json("fs.ls", true, "hello", "");
        let tr = ToolResult::parse(&json).expect("should parse");
        assert_eq!(tr.tool, "fs.ls");
        assert!(tr.ok);
        assert_eq!(tr.stdout, "hello");
        assert!(tr.stderr.is_empty());
    }

    #[test]
    fn tool_result_parse_missing_field() {
        let json = r#"{"tool":"fs.ls"}"#;
        assert!(ToolResult::parse(json).is_none());
    }

    // =========================================================================
    // format_tool_args
    // =========================================================================

    #[test]
    fn format_tool_args_fs_ls_with_path() {
        let args = serde_json::json!({"path": "/tmp"});
        let out = format_tool_args("fs.ls", &args);
        assert!(out.contains("/tmp"), "output: {out}");
        assert!(out.starts_with("ls "), "output: {out}");
    }

    #[test]
    fn format_tool_args_cmd_run() {
        let args = serde_json::json!({"command": "echo hello"});
        let out = format_tool_args("cmd.run", &args);
        assert!(out.contains("echo hello"), "output: {out}");
    }

    #[test]
    fn format_tool_args_null_args() {
        let out = format_tool_args("unknown", &serde_json::Value::Null);
        assert_eq!(out, "unknown");
    }

    #[test]
    fn format_tool_args_generic_compact() {
        let args = serde_json::json!({"a": 1});
        let out = format_tool_args("my_tool", &args);
        assert!(out.contains("my_tool"), "output: {out}");
        assert!(out.contains(r#""a":1"#) || out.contains(r#""a": 1"#), "output: {out}");
    }

    #[test]
    fn format_tool_args_empty_object() {
        let args = serde_json::json!({});
        let out = format_tool_args("my_tool", &args);
        assert_eq!(out, "my_tool");
    }

    // =========================================================================
    // ToolResult format variants
    // =========================================================================

    #[test]
    fn format_tool_result_fs_read_empty() {
        let json = make_tool_json("fs.read", true, "", "");
        let output = format_tool_result(&json);
        assert_eq!(output, "(empty file)");
    }

    #[test]
    fn format_tool_result_fs_write_ok() {
        let json = make_tool_json("fs.write", true, "Wrote 42 bytes to foo.txt", "");
        let output = format_tool_result(&json);
        assert!(output.contains("Wrote 42 bytes"), "output: {output}");
    }

    #[test]
    fn format_tool_result_generic_ok_no_stdout() {
        let json = make_tool_json("some.tool", true, "", "");
        let output = format_tool_result(&json);
        assert_eq!(output, "OK");
    }

    #[test]
    fn format_tool_result_error_empty_stderr() {
        let json = make_tool_json("cmd.run", false, "", "");
        let output = format_tool_result(&json);
        assert!(output.contains("Unknown error"), "output: {output}");
    }

    #[test]
    fn format_tool_result_fs_ls_empty_dir() {
        let json = make_tool_json("fs.ls", true, "", "");
        let output = format_tool_result(&json);
        assert_eq!(output, "(empty directory)");
    }
}

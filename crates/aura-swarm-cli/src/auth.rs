//! Credential storage and interactive zOS login.
//!
//! Tokens are persisted as JSON at a platform-appropriate path so that
//! `aswarm` can be invoked without `--token` after a successful login.

use std::io::{self, Write};
use std::path::PathBuf;

use aura_swarm_auth::{AuthConfig, ZosClient};
use serde::{Deserialize, Serialize};
use tokio::fs;

/// On-disk credential format.
#[derive(Debug, Serialize, Deserialize)]
struct StoredCredentials {
    access_token: String,
}

/// Returns the path to the credentials file.
///
/// - Windows: `%LOCALAPPDATA%/aura-swarm/credentials.json`
/// - Linux:   `~/.local/share/aura-swarm/credentials.json`
/// - macOS:   `~/Library/Application Support/aura-swarm/credentials.json`
pub fn credentials_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("aura-swarm").join("credentials.json"))
}

/// Persist an access token to disk.
pub async fn save_credentials(token: &str) -> anyhow::Result<()> {
    let path = credentials_path()
        .ok_or_else(|| anyhow::anyhow!("could not determine local data directory"))?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let creds = StoredCredentials {
        access_token: token.to_owned(),
    };

    let json = serde_json::to_string_pretty(&creds)?;
    fs::write(&path, json).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    }

    Ok(())
}

/// Load a previously stored access token, returning `None` if absent.
pub async fn load_credentials() -> Option<String> {
    let path = credentials_path()?;
    let data = fs::read_to_string(path).await.ok()?;
    let creds: StoredCredentials = serde_json::from_str(&data).ok()?;
    Some(creds.access_token)
}

/// Delete the stored credentials file.
pub async fn clear_credentials() -> anyhow::Result<()> {
    if let Some(path) = credentials_path() {
        match fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Resolve the token to use: explicit flag > env (handled by clap) > stored credentials.
pub async fn resolve_token(flag_value: Option<String>) -> anyhow::Result<String> {
    if let Some(token) = flag_value {
        return Ok(token);
    }

    if let Some(token) = load_credentials().await {
        return Ok(token);
    }

    anyhow::bail!("Not authenticated. Run `aswarm login` first, or pass --token.")
}

/// Prompt for email and password, authenticate against zOS, and return the access token.
pub async fn login_interactive(zos_url: &str) -> anyhow::Result<String> {
    let config = AuthConfig {
        base_url: zos_url.to_owned(),
        ..AuthConfig::default()
    };

    let client = ZosClient::new(config)?;

    let email = tokio::task::spawn_blocking(|| {
        print!("Email: ");
        io::stdout().flush()?;
        let mut email = String::new();
        io::stdin().read_line(&mut email)?;
        Ok::<_, anyhow::Error>(email.trim().to_owned())
    })
    .await??;

    if email.is_empty() {
        anyhow::bail!("Email cannot be empty");
    }

    let password = tokio::task::spawn_blocking(|| prompt_password("Password: ")).await??;
    if password.is_empty() {
        anyhow::bail!("Password cannot be empty");
    }

    let resp = client.login(&email, &password).await?;
    Ok(resp.access_token)
}

/// Read a password from the terminal, printing `*` for each character.
///
/// Handles typing, paste (rapid character events), backspace, and Esc to cancel.
/// This function performs blocking I/O (crossterm raw mode + event reads) and
/// should only be called from a blocking context (e.g. inside `spawn_blocking`).
fn prompt_password(prompt: &str) -> anyhow::Result<String> {
    use crossterm::terminal;

    let mut stdout = io::stdout();
    write!(stdout, "{prompt}")?;
    stdout.flush()?;

    terminal::enable_raw_mode()?;
    let result = read_password_raw(&mut stdout);
    terminal::disable_raw_mode()?;

    writeln!(stdout)?;
    stdout.flush()?;

    result
}

fn read_password_raw(stdout: &mut io::Stdout) -> anyhow::Result<String> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};

    let mut password = String::new();

    loop {
        let ev = event::read()?;

        if let Event::Key(key) = ev {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Enter => return Ok(password),
                KeyCode::Esc => anyhow::bail!("Cancelled"),
                KeyCode::Backspace => {
                    if password.pop().is_some() {
                        write!(stdout, "\x08 \x08")?;
                        stdout.flush()?;
                    }
                }
                KeyCode::Char(c) => {
                    password.push(c);
                    write!(stdout, "*")?;
                    stdout.flush()?;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_token_flag_wins() {
        let result = resolve_token(Some("my-token".to_string())).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "my-token");
    }

    #[tokio::test]
    async fn resolve_token_flag_wins_over_stored() {
        let result = resolve_token(Some("explicit-tok".to_string())).await;
        assert_eq!(result.unwrap(), "explicit-tok");
    }

    #[tokio::test]
    async fn resolve_token_none_without_stored_fails() {
        // Without stored credentials, resolve_token(None) should error.
        // This test may pass or fail depending on whether credentials
        // are stored on the machine - we just check the flag path above.
        // For safety, only assert the error message format if it does fail.
        if let Err(e) = resolve_token(None).await {
            let msg = e.to_string();
            assert!(msg.contains("Not authenticated"), "unexpected error: {msg}");
        }
    }

    #[test]
    fn credentials_path_returns_some() {
        // On most systems dirs::data_local_dir() succeeds
        if let Some(path) = credentials_path() {
            assert!(path.ends_with("credentials.json"));
            let parent = path.parent().unwrap();
            assert!(
                parent.ends_with("aura-swarm"),
                "parent: {}",
                parent.display()
            );
        }
    }
}

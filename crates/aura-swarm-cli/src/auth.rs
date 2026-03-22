//! Credential storage and interactive zOS login.
//!
//! Tokens are persisted as JSON at a platform-appropriate path so that
//! `aswarm` can be invoked without `--token` after a successful login.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use aura_swarm_auth::{AuthConfig, ZosClient};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal;
use serde::{Deserialize, Serialize};

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
pub fn save_credentials(token: &str) -> anyhow::Result<()> {
    let path = credentials_path()
        .ok_or_else(|| anyhow::anyhow!("could not determine local data directory"))?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let creds = StoredCredentials {
        access_token: token.to_owned(),
    };

    let json = serde_json::to_string_pretty(&creds)?;
    fs::write(&path, json)?;

    // Best-effort restrictive permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

/// Load a previously stored access token, returning `None` if absent.
pub fn load_credentials() -> Option<String> {
    let path = credentials_path()?;
    let data = fs::read_to_string(path).ok()?;
    let creds: StoredCredentials = serde_json::from_str(&data).ok()?;
    Some(creds.access_token)
}

/// Delete the stored credentials file.
pub fn clear_credentials() -> anyhow::Result<()> {
    if let Some(path) = credentials_path() {
        if path.exists() {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Resolve the token to use: explicit flag > env (handled by clap) > stored credentials.
pub fn resolve_token(flag_value: Option<String>) -> anyhow::Result<String> {
    if let Some(token) = flag_value {
        return Ok(token);
    }

    if let Some(token) = load_credentials() {
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

    let client = ZosClient::new(config);

    // Prompt email
    print!("Email: ");
    io::stdout().flush()?;
    let mut email = String::new();
    io::stdin().read_line(&mut email)?;
    let email = email.trim().to_owned();
    if email.is_empty() {
        anyhow::bail!("Email cannot be empty");
    }

    let password = prompt_password("Password: ")?;
    if password.is_empty() {
        anyhow::bail!("Password cannot be empty");
    }

    let resp = client.login(&email, &password).await?;
    Ok(resp.access_token)
}

/// Read a password from the terminal, printing `*` for each character.
///
/// Handles typing, paste (rapid character events), backspace, and Esc to cancel.
fn prompt_password(prompt: &str) -> anyhow::Result<String> {
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

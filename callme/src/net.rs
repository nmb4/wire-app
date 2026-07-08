use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use iroh::{Endpoint, NodeAddr, SecretKey};
pub use iroh_roq::ALPN;

use crate::rtc::RtcConnection;

/// Returns the per-user config directory used to persist callme state
/// (secret key and friends list).
///
/// Can be overridden with the `CALLME_CONFIG_DIR` environment variable.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CALLME_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(dir) = std::env::var_os("APPDATA") {
            return Some(PathBuf::from(dir).join("callme"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return Some(PathBuf::from(home).join("Library/Application Support/callme"));
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(dir).join("callme"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Some(PathBuf::from(home).join(".config/callme"));
        }
    }
    None
}

/// Loads the persisted secret key, generating and saving one on first run.
///
/// This keeps the node id stable across restarts so it can be shared with
/// friends. The `IROH_SECRET` environment variable still takes precedence.
fn load_or_create_secret_key() -> Result<SecretKey> {
    if let Ok(secret) = std::env::var("IROH_SECRET") {
        return SecretKey::from_str(&secret).context("failed to parse secret key from IROH_SECRET");
    }

    if let Some(dir) = config_dir() {
        let key_path = dir.join("secret.key");
        if key_path.exists() {
            let contents =
                std::fs::read_to_string(&key_path).context("failed to read stored secret key")?;
            return SecretKey::from_str(contents.trim()).context("failed to parse stored secret key");
        }
    }

    let secret_key = SecretKey::generate(&mut rand::rngs::OsRng);
    if let Some(dir) = config_dir() {
        let key_path = dir.join("secret.key");
        if let Some(parent) = key_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&key_path, secret_key.to_string());
    }
    Ok(secret_key)
}

pub async fn bind_endpoint() -> Result<Endpoint> {
    let secret_key = load_or_create_secret_key()?;
    Endpoint::builder()
        .secret_key(secret_key)
        .discovery_n0()
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
}

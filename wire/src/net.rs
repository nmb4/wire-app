use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Once;

use anyhow::{Context, Result};
use iroh::{Endpoint, NodeAddr, SecretKey};
pub use iroh_roq::ALPN;
use tracing::warn;

use crate::rtc::RtcConnection;

const CONFIG_DIR_NAME: &str = "wire";
const LEGACY_CONFIG_DIR_NAME: &str = "callme";

static CONFIG_MIGRATION: Once = Once::new();

/// Returns the per-user config directory used to persist wire state
/// (secret key, friends list, and application settings).
///
/// Can be overridden with the `WIRE_CONFIG_DIR` environment variable.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("WIRE_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }

    let dir = platform_config_dir(CONFIG_DIR_NAME)?;
    run_config_migration(&dir);
    Some(dir)
}

/// Returns the legacy callme config directory, if it exists on this platform.
pub fn legacy_config_dir() -> Option<PathBuf> {
    if std::env::var_os("WIRE_CONFIG_DIR").is_some() {
        return None;
    }
    platform_config_dir(LEGACY_CONFIG_DIR_NAME).filter(|path| path.exists())
}

/// Run config migration once before any worker or UI code touches app data.
pub fn prepare_config_dir() {
    let _ = config_dir();
}

fn run_config_migration(current: &Path) {
    CONFIG_MIGRATION.call_once(|| {
        let Some(legacy) = platform_config_dir(LEGACY_CONFIG_DIR_NAME) else {
            return;
        };
        if legacy == current || !legacy.exists() {
            return;
        }

        if let Err(err) = migrate_legacy_config_inner(&legacy, current) {
            warn!(
                "failed to migrate legacy config from {}: {err}",
                legacy.display()
            );
        }
    });
}

fn platform_config_dir(name: &str) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        if let Some(dir) = std::env::var_os("APPDATA") {
            return Some(PathBuf::from(dir).join(name));
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return Some(PathBuf::from(home).join(format!("Library/Application Support/{name}")));
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(dir).join(name));
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Some(PathBuf::from(home).join(format!(".config/{name}")));
        }
    }
    None
}

fn migrate_legacy_config_inner(legacy: &Path, current: &Path) -> Result<()> {
    if current.exists() {
        merge_legacy_entries(legacy, current)?;
    } else if let Some(parent) = current.parent() {
        std::fs::create_dir_all(parent).context("failed to create config parent directory")?;
        match std::fs::rename(legacy, current) {
            Ok(()) => return Ok(()),
            Err(_) => {
                std::fs::create_dir_all(current).context("failed to create config directory")?;
                merge_legacy_entries(legacy, current)?;
            }
        }
    } else {
        std::fs::create_dir_all(current).context("failed to create config directory")?;
        merge_legacy_entries(legacy, current)?;
    }

    remove_dir_if_empty(legacy)?;
    Ok(())
}

fn merge_legacy_entries(legacy: &Path, current: &Path) -> Result<()> {
    std::fs::create_dir_all(current).context("failed to create config directory")?;
    for entry in std::fs::read_dir(legacy).context("failed to read legacy config directory")? {
        let entry = entry.context("failed to read legacy config entry")?;
        let source = entry.path();
        let destination = current.join(entry.file_name());
        migrate_entry(&source, &destination)?;
    }
    Ok(())
}

fn migrate_entry(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        if should_replace_with_legacy(source, destination)? {
            copy_and_remove_legacy(source, destination)?;
        }
        return Ok(());
    }

    match std::fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => copy_and_remove_legacy(source, destination),
    }
}

fn should_replace_with_legacy(source: &Path, destination: &Path) -> Result<bool> {
    let Some(name) = destination.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };

    match name {
        "friends.json" => {
            Ok(is_empty_friends_file(destination)? && !is_empty_friends_file(source)?)
        }
        "settings.json" => {
            Ok(std::fs::metadata(destination)?.len() == 0 && std::fs::metadata(source)?.len() > 0)
        }
        _ => Ok(false),
    }
}

fn is_empty_friends_file(path: &Path) -> Result<bool> {
    let contents = std::fs::read_to_string(path).unwrap_or_default();
    let trimmed = contents.trim();
    Ok(trimmed.is_empty() || trimmed == "[]")
}

fn copy_and_remove_legacy(source: &Path, destination: &Path) -> Result<()> {
    std::fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    let _ = std::fs::remove_file(source);
    Ok(())
}

fn remove_dir_if_empty(path: &Path) -> Result<()> {
    let mut entries = std::fs::read_dir(path).context("failed to read directory")?;
    if entries.next().is_some() {
        return Ok(());
    }
    std::fs::remove_dir(path).context("failed to remove empty legacy config directory")?;
    Ok(())
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
            return SecretKey::from_str(contents.trim())
                .context("failed to parse stored secret key");
        }

        if let Some(legacy) = legacy_config_dir() {
            let legacy_key = legacy.join("secret.key");
            if legacy_key.exists() {
                let contents = std::fs::read_to_string(&legacy_key)
                    .context("failed to read legacy stored secret key")?;
                let secret_key = SecretKey::from_str(contents.trim())
                    .context("failed to parse legacy stored secret key")?;
                if let Some(parent) = key_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&key_path, secret_key.to_string());
                let _ = std::fs::remove_file(&legacy_key);
                return Ok(secret_key);
            }
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

/// Generates a node identity that is never persisted.
///
/// Intended for isolated development instances that must not reuse the normal
/// application identity.
pub fn generate_ephemeral_secret_key() -> SecretKey {
    SecretKey::generate(&mut rand::rngs::OsRng)
}

pub async fn bind_endpoint() -> Result<Endpoint> {
    bind_endpoint_with_alpns(std::iter::empty()).await
}

/// Binds the persistent Wire endpoint and advertises application protocols in
/// addition to the RTC protocol. The identity and discovery configuration are
/// deliberately shared, while each protocol keeps an independent lifecycle.
pub async fn bind_endpoint_with_alpns(
    additional_alpns: impl IntoIterator<Item = Vec<u8>>,
) -> Result<Endpoint> {
    let secret_key = load_or_create_secret_key()?;
    let mut alpns = vec![ALPN.to_vec()];
    for alpn in additional_alpns {
        if !alpns.contains(&alpn) {
            alpns.push(alpn);
        }
    }
    Endpoint::builder()
        .secret_key(secret_key)
        .discovery_n0()
        .alpns(alpns)
        .bind()
        .await
}

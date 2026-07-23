#![cfg_attr(not(windows), allow(dead_code))]

use std::{
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const FILES_API: &str = "https://api.stardive.space/v1/files";
const RELEASE_PREFIX: &str = "wire-app-v";
const RELEASE_SUFFIX: &str = ".zip";
const EXECUTABLE_NAME: &str = "wire-app.exe";

#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    pub id: String,
    pub version: String,
    pub sha256: String,
}

#[derive(Deserialize)]
struct FileList {
    files: Vec<FileEntry>,
}

#[derive(Deserialize)]
struct FileEntry {
    id: String,
    original_name: String,
    sha256: String,
}

fn parse_version(value: &str) -> Option<[u64; 3]> {
    let mut parts = value.split('.');
    let version = [
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ];
    parts.next().is_none().then_some(version)
}

pub(crate) fn is_version_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

fn release_from_file(file: FileEntry) -> Option<([u64; 3], ReleaseInfo)> {
    let version = file
        .original_name
        .strip_prefix(RELEASE_PREFIX)?
        .strip_suffix(RELEASE_SUFFIX)?;
    let parsed = parse_version(version)?;
    Some((
        parsed,
        ReleaseInfo {
            id: file.id,
            version: version.to_string(),
            sha256: file.sha256,
        },
    ))
}

fn latest_newer_release(files: Vec<FileEntry>, current: [u64; 3]) -> Option<ReleaseInfo> {
    files
        .into_iter()
        .filter_map(release_from_file)
        .filter(|(version, _)| *version > current)
        .max_by_key(|(version, _)| *version)
        .map(|(_, release)| release)
}

pub fn check_for_update() -> Result<Option<ReleaseInfo>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to create update client")?;
    let files = client
        .get(FILES_API)
        .send()
        .context("failed to contact update server")?
        .error_for_status()
        .context("update server returned an error")?
        .json::<FileList>()
        .context("update server returned invalid metadata")?;

    let current = parse_version(crate::APP_VERSION)
        .context("the current application version is not major.minor.patch")?;
    Ok(latest_newer_release(files.files, current))
}

pub fn download_update(release: &ReleaseInfo) -> Result<PathBuf> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .context("failed to create download client")?;
    let url = format!("{FILES_API}/{}", release.id);
    let archive = client
        .get(url)
        .send()
        .context("failed to download update")?
        .error_for_status()
        .context("update download returned an error")?
        .bytes()
        .context("failed to read update download")?;

    let actual_sha256 = format!("{:x}", Sha256::digest(&archive));
    if !actual_sha256.eq_ignore_ascii_case(&release.sha256) {
        bail!("download checksum did not match the API metadata");
    }

    let mut zip = zip::ZipArchive::new(Cursor::new(archive)).context("invalid update ZIP")?;
    let mut entry = zip
        .by_name(EXECUTABLE_NAME)
        .with_context(|| format!("update ZIP does not contain {EXECUTABLE_NAME}"))?;
    let mut executable = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut executable)
        .context("failed to extract update executable")?;

    let desktop = dirs::desktop_dir().context("could not locate the Desktop folder")?;
    let temporary = desktop.join(format!(".{EXECUTABLE_NAME}.download"));
    fs::write(&temporary, executable).context("failed to write update to Desktop")?;
    Ok(temporary)
}

pub fn relaunch_after_download(temporary: &Path) -> Result<()> {
    let desktop = dirs::desktop_dir().context("could not locate the Desktop folder")?;
    let destination = desktop.join(EXECUTABLE_NAME);
    let source = powershell_quote(temporary);
    let destination = powershell_quote(&destination);
    let script = format!(
        "$source = {source}; $destination = {destination}; for ($attempt = 0; $attempt -lt 120; $attempt++) {{ try {{ Move-Item -LiteralPath $source -Destination $destination -Force -ErrorAction Stop; Start-Process -FilePath $destination; exit 0 }} catch {{ Start-Sleep -Milliseconds 250 }} }}; exit 1"
    );

    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .spawn()
        .context("failed to start the update helper")?;
    Ok(())
}

fn powershell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_filename() {
        let file = FileEntry {
            id: "abc".into(),
            original_name: "wire-app-v1.12.3.zip".into(),
            sha256: "hash".into(),
        };
        let (version, release) = release_from_file(file).unwrap();
        assert_eq!(version, [1, 12, 3]);
        assert_eq!(release.version, "1.12.3");
    }

    #[test]
    fn ignores_non_release_files() {
        let file = FileEntry {
            id: "abc".into(),
            original_name: "wire-app.zip".into(),
            sha256: "hash".into(),
        };
        assert!(release_from_file(file).is_none());
    }

    #[test]
    fn selects_highest_version_newer_than_current() {
        let file = |name: &str| FileEntry {
            id: name.into(),
            original_name: name.into(),
            sha256: "hash".into(),
        };
        let release = latest_newer_release(
            vec![
                file("wire-app-v0.1.2.zip"),
                file("wire-app-v0.2.0.zip"),
                file("unrelated.zip"),
            ],
            [0, 1, 2],
        )
        .unwrap();
        assert_eq!(release.version, "0.2.0");
        assert!(latest_newer_release(vec![file("wire-app-v0.1.2.zip")], [0, 1, 2]).is_none());
    }

    #[test]
    fn compares_client_versions_for_peer_update_hints() {
        assert!(is_version_newer("1.2.0", "1.1.9"));
        assert!(!is_version_newer("1.1.9", "1.2.0"));
        assert!(!is_version_newer("development", "1.2.0"));
    }
}

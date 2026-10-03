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
use tracing::debug;

const FILES_API: &str = "https://api.stardive.space/v1/files";
const RELEASE_PREFIX: &str = "wire-app-v";
const RELEASE_SUFFIX: &str = ".zip";
const EXECUTABLE_NAME: &str = "wire-app.exe";
/// Prefix and suffix of a staged executable. Hidden on purpose: it only exists
/// between "download finished" and "relaunch".
const STAGED_PREFIX: &str = ".wire-app.update.";
const STAGED_SUFFIX: &str = ".staged";
/// A Wire executable is tens of megabytes; anything larger is a protocol bug.
pub(crate) const MAX_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;

/// A staging name unique to one transfer within one process.
///
/// The process id separates concurrent transfers; the counter separates repeated
/// attempts within one process. Both would still collide after a recycled pid,
/// which is why [`sweep_stale_staged_files`] exists.
fn staged_name() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{STAGED_PREFIX}{}-{n}{STAGED_SUFFIX}", std::process::id())
}

/// Delete staged executables left behind by a previous run.
///
/// A crash, a killed helper, or a failed relaunch can leave one next to the
/// install. They are inert on their own, but they accumulate and are large.
pub fn sweep_stale_staged_files() {
    for dir in staging_dirs() {
        sweep_staged_in(&dir);
    }
}

/// Remove every staged executable in `dir`, leaving all other files alone.
fn sweep_staged_in(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(STAGED_PREFIX) || !name.ends_with(STAGED_SUFFIX) {
            continue;
        }
        if let Err(error) = fs::remove_file(entry.path()) {
            debug!(
                path = %entry.path().display(),
                "could not remove a stale staged update: {error}"
            );
        }
    }
}

/// Directories a staged executable can live in.
fn staging_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            dirs.push(parent.to_path_buf());
        }
    }
    if let Some(desktop) = dirs::desktop_dir() {
        if !dirs.contains(&desktop) {
            dirs.push(desktop);
        }
    }
    dirs
}

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

/// Order two semantic versions. Unparseable versions sort as equal so a
/// development build never appears to outrank a real release number.
pub(crate) fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    match (parse_version(left), parse_version(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => std::cmp::Ordering::Equal,
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

/// Where a verified update is written, and what it will replace on relaunch.
///
/// The preferred target is the running executable itself, so a peer-to-peer
/// update leaves a single, self-consistent install. Windows cannot overwrite a
/// running image, so the replacement is handed to a helper that waits for this
/// process to exit. Installations under a protected directory (`Program Files`)
/// cannot be written at all, so those fall back to the Desktop copy the
/// original updater produced.
#[derive(Clone, Debug)]
pub struct StagedUpdate {
    path: PathBuf,
    destination: PathBuf,
}

impl StagedUpdate {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Pick a unique staging location for this transfer.
    ///
    /// Unique per plan on purpose. The hosted download runs on a UI thread while
    /// peer transfers run on the worker, so one shared filename would let two
    /// writers hold the same file open at once — and the loser could then rewrite
    /// the file the winner had already verified.
    pub fn plan() -> Result<Self> {
        let current =
            std::env::current_exe().context("could not locate the running Wire executable")?;
        let desktop = dirs::desktop_dir().context("could not locate the Desktop folder")?;
        Self::plan_for(&current, &desktop, &staged_name())
    }

    fn plan_for(current: &Path, desktop: &Path, staged_name: &str) -> Result<Self> {
        if let Some(dir) = current.parent() {
            if dir_is_writable(dir) {
                return Ok(Self {
                    path: dir.join(staged_name),
                    destination: current.to_path_buf(),
                });
            }
        }
        Ok(Self {
            path: desktop.join(staged_name),
            destination: desktop.join(EXECUTABLE_NAME),
        })
    }

    /// Verify the staged bytes against the digest advertised by the source.
    ///
    /// The digest only proves the transfer was not corrupted, but it is the
    /// same guarantee the hosted-release path relies on.
    pub fn verify(&self, expected_sha256: &str) -> Result<()> {
        let actual = sha256_of_file(&self.path)?;
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            bail!("the received executable did not match the sender's checksum");
        }
        Ok(())
    }

    /// Hand the staged executable to a helper that replaces the current one and
    /// starts it. Returns as soon as the helper is running; the caller is
    /// expected to close the window so the file lock clears.
    pub fn install_and_relaunch(&self) -> Result<()> {
        let source = powershell_quote(&self.path);
        let destination = powershell_quote(&self.destination);
        let script = format!(
            "$source = {source}; $destination = {destination}; for ($attempt = 0; $attempt -lt 480; $attempt++) {{ try {{ Move-Item -LiteralPath $source -Destination $destination -Force -ErrorAction Stop; Start-Process -FilePath $destination; exit 0 }} catch {{ Start-Sleep -Milliseconds 250 }} }}; exit 1"
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
}

pub fn sha256_of_file(path: &Path) -> Result<String> {
    use std::io::Read as _;
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open {} for verification", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .context("failed while reading the downloaded executable")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Probe writability by creating a real file: a read-only directory attribute is
/// not always reported by `metadata().permissions()` on Windows.
fn dir_is_writable(dir: &Path) -> bool {
    // `create_new` so a probe left behind by a process that died mid-check (or
    // one sharing a recycled pid) cannot make a writable directory look full.
    for attempt in 0..8u32 {
        let probe = dir.join(format!(
            ".wire-write-probe-{}-{attempt}",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(_) => {
                let _ = fs::remove_file(&probe);
                return true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return false,
        }
    }
    false
}

pub fn download_update(release: &ReleaseInfo) -> Result<StagedUpdate> {
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
    if executable.is_empty() {
        bail!("the update ZIP contains an empty executable");
    }

    let staged = StagedUpdate::plan()?;
    if let Err(error) = fs::write(&staged.path, executable)
        .with_context(|| format!("failed to write update to {}", staged.path.display()))
    {
        // Never leave a truncated file where a later run might pick it up.
        let _ = fs::remove_file(&staged.path);
        return Err(error);
    }
    Ok(staged)
}

/// Install a verified update and restart into it.
///
/// Shared by the hosted-release and peer-to-peer paths so both end up at the
/// same single, self-consistent install.
pub fn install_and_relaunch(staged: &StagedUpdate) -> Result<()> {
    staged.install_and_relaunch()
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

    #[test]
    fn orders_versions_and_treats_unparseable_ones_as_equal() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("1.2.0", "1.10.0"), Ordering::Less);
        assert_eq!(compare_versions("0.7.10", "0.7.9"), Ordering::Greater);
        assert_eq!(compare_versions("1.2.0", "1.2.0"), Ordering::Equal);
        assert_eq!(compare_versions("development", "1.2.0"), Ordering::Equal);
    }

    #[test]
    fn stages_next_to_the_running_executable_when_the_directory_is_writable() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("wire-app.exe");
        let desktop = dir.path().join("Desktop");
        std::fs::create_dir_all(&desktop).unwrap();
        let plan = StagedUpdate::plan_for(&exe, &desktop, ".wire-app.update.1.staged").unwrap();
        assert_eq!(plan.destination, exe);
        assert_eq!(plan.path.parent(), Some(dir.path()));
        // Hidden: the staged file only exists between download and relaunch.
        assert!(plan
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.')));
    }

    #[test]
    fn every_transfer_gets_its_own_staging_file() {
        // Two concurrent transfers (hosted on a UI thread, peer on the worker)
        // must never target the same path, or the loser's bytes could be
        // installed after the winner verified its own.
        let first = staged_name();
        let second = staged_name();
        assert_ne!(first, second);
        assert!(first.starts_with(STAGED_PREFIX) && first.ends_with(STAGED_SUFFIX));
    }

    #[test]
    fn a_stale_staged_file_from_a_previous_run_is_swept() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir
            .path()
            .join(format!("{STAGED_PREFIX}999-3{STAGED_SUFFIX}"));
        let current = dir.path().join("wire-app.exe");
        std::fs::write(&stale, b"leftover").unwrap();
        std::fs::write(&current, b"running").unwrap();
        sweep_staged_in(dir.path());
        assert!(!stale.exists(), "an orphaned staged file must be removed");
        assert!(
            current.exists(),
            "the sweep must never touch the real executable"
        );
    }

    #[test]
    fn falls_back_to_the_desktop_when_the_install_directory_is_read_only() {
        let desktop = tempfile::tempdir().unwrap();
        let protected = tempfile::tempdir().unwrap();
        // A file (not a directory) in place of the install dir makes any child
        // path unwritable, which is what a protected install looks like here.
        let blocked = protected.path().join("Program Files");
        std::fs::write(&blocked, b"").unwrap();
        let plan = StagedUpdate::plan_for(&blocked.join("wire-app.exe"), desktop.path(), ".stage")
            .unwrap();
        assert_eq!(plan.destination, desktop.path().join(EXECUTABLE_NAME));
        assert_eq!(plan.path.parent(), Some(desktop.path()));
    }

    #[test]
    fn verification_rejects_bytes_that_do_not_match_the_advertised_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire-app.exe");
        std::fs::write(&path, b"not really an executable").unwrap();
        let plan = StagedUpdate {
            path: path.clone(),
            destination: dir.path().join("target.exe"),
        };
        let actual = sha256_of_file(&path).unwrap();
        plan.verify(&actual).unwrap();
        assert!(plan.verify(&"0".repeat(64)).is_err());
    }
}

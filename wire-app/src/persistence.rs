use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use tracing::warn;

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{suffix}"))
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(value) => Ok(Some(value)),
            Err(primary_error) => {
                let backup = sidecar(path, "bak");
                let bytes = fs::read(&backup).with_context(|| {
                    format!(
                        "invalid JSON in {} ({primary_error}) and no readable backup",
                        path.display()
                    )
                })?;
                let value = serde_json::from_slice(&bytes).with_context(|| {
                    format!(
                        "invalid JSON in both {} and {}",
                        path.display(),
                        backup.display()
                    )
                })?;
                warn!(
                    path = %path.display(),
                    backup = %backup.display(),
                    "recovered local state from backup after the primary file became invalid"
                );
                Ok(Some(value))
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let backup = sidecar(path, "bak");
            match fs::read(&backup) {
                Ok(bytes) => {
                    Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                        format!("invalid JSON in {}", backup.display())
                    })?))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error).with_context(|| format!("read {}", backup.display())),
            }
        }
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    let temporary = sidecar(path, "tmp");
    let mut file =
        File::create(&temporary).with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("write {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("flush {}", temporary.display()))?;
    drop(file);

    replace_file(&temporary, path)?;

    // Keep the last fully committed value as a recovery copy. Failure to refresh
    // it must not turn a successful primary commit into a reported failure.
    let backup = sidecar(path, "bak");
    if let Err(error) = fs::copy(path, &backup) {
        warn!(path = %backup.display(), %error, "could not refresh JSON recovery copy");
    } else if let Ok(file) = File::open(&backup) {
        let _ = file.sync_all();
    }
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        },
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .with_context(|| {
        format!(
            "replace {}",
            destination_path(destination.as_slice()).display()
        )
    })
}

#[cfg(windows)]
fn destination_path(value: &[u16]) -> PathBuf {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    let value = value.strip_suffix(&[0]).unwrap_or(value);
    PathBuf::from(OsString::from_wide(value))
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)
        .with_context(|| format!("replace {}", destination.display()))?;
    if let Some(parent) = destination.parent() {
        File::open(parent)
            .and_then(|file| file.sync_all())
            .with_context(|| format!("flush directory {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_json_and_recovers_from_a_torn_primary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let expected = vec!["one".to_owned(), "two".to_owned()];

        write_json(&path, &expected).unwrap();
        assert_eq!(
            read_json::<Vec<String>>(&path).unwrap(),
            Some(expected.clone())
        );

        fs::write(&path, b"{truncated").unwrap();
        assert_eq!(read_json::<Vec<String>>(&path).unwrap(), Some(expected));
    }

    #[test]
    fn missing_file_is_distinct_from_an_empty_value() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("friends.json");
        assert_eq!(read_json::<Vec<String>>(&path).unwrap(), None);

        write_json(&path, &Vec::<String>::new()).unwrap();
        assert_eq!(read_json::<Vec<String>>(&path).unwrap(), Some(Vec::new()));
    }
}

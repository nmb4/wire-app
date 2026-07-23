//! Per-user "start with system" integration.

use std::path::Path;

use anyhow::{Context, Result};

const APP_NAME: &str = "Wire";

pub fn set_enabled(enabled: bool) -> Result<()> {
    let executable = std::env::current_exe().context("could not locate the Wire executable")?;
    set_enabled_for(&executable, enabled)
}

#[cfg(windows)]
fn set_enabled_for(executable: &Path, enabled: bool) -> Result<()> {
    use anyhow::bail;
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let reg_command = || {
        let mut command = Command::new("reg");
        command.creation_flags(CREATE_NO_WINDOW);
        command
    };

    if enabled {
        let command = format!("\"{}\"", executable.display());
        let output = reg_command()
            .args(["add", RUN_KEY, "/v", APP_NAME, "/t", "REG_SZ", "/d"])
            .arg(command)
            .args(["/f"])
            .output()
            .context("could not run the Windows startup configuration tool")?;
        if output.status.success() {
            return Ok(());
        }
        bail!(
            "Windows could not update the startup setting: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let deletion = reg_command()
        .args(["delete", RUN_KEY, "/v", APP_NAME, "/f"])
        .output()
        .context("could not run the Windows startup configuration tool")?;
    if deletion.status.success() {
        return Ok(());
    }

    // `reg delete` reports a localized error when the value is already absent.
    // Query after the failed deletion so idempotent disable stays reliable in
    // every system language while a value that remains is still an error.
    let query = reg_command()
        .args(["query", RUN_KEY, "/v", APP_NAME])
        .output()
        .context("could not verify the Windows startup setting")?;
    if query.status.success() {
        bail!(
            "Windows could not update the startup setting: {}",
            String::from_utf8_lossy(&deletion.stderr).trim()
        )
    } else {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn set_enabled_for(executable: &Path, enabled: bool) -> Result<()> {
    let directory = dirs::home_dir()
        .context("could not locate the home directory")?
        .join("Library/LaunchAgents");
    let path = directory.join("live.stardive.wire.plist");
    if !enabled {
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error).context("could not remove the Wire launch agent");
            }
        }
        return Ok(());
    }

    std::fs::create_dir_all(&directory)?;
    let executable = xml_escape(&executable.to_string_lossy());
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>live.stardive.wire</string>
<key>ProgramArguments</key><array><string>{executable}</string></array>
<key>RunAtLoad</key><true/>
</dict></plist>
"#
    );
    std::fs::write(path, plist).context("could not write the Wire launch agent")
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn set_enabled_for(executable: &Path, enabled: bool) -> Result<()> {
    let directory = dirs::config_dir()
        .context("could not locate the user config directory")?
        .join("autostart");
    let path = directory.join("wire.desktop");
    if !enabled {
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error).context("could not remove the Wire autostart entry");
            }
        }
        return Ok(());
    }

    std::fs::create_dir_all(&directory)?;
    let executable = desktop_exec_escape(&executable.to_string_lossy());
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName={APP_NAME}\nExec=\"{executable}\"\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    );
    std::fs::write(path, entry).context("could not write the Wire autostart entry")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_exec_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

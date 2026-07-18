//! ADB (Android Debug Bridge) transport.
//!
//! This is the simplest transport: it shells out to the `adb` binary, which
//! handles USB connection, authentication, and the actual file copy. We only
//! need to invoke it and parse its output.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use super::{Device, RemoteEntry, Transport};

pub struct AdbTransport {
    /// Path to the `adb` executable.
    adb: PathBuf,
}

impl AdbTransport {
    /// Locate `adb` and build a transport.
    ///
    /// Resolution order:
    ///   1. `$ZAP_ADB`
    ///   2. `$ANDROID_HOME/platform-tools/adb`
    ///   3. `adb` on `$PATH`
    ///
    /// Returns an error only when a path is explicitly configured but missing;
    /// a bare `adb` on `$PATH` is validated lazily on first use.
    pub fn locate() -> Result<Self> {
        if let Some(p) = std::env::var_os("ZAP_ADB") {
            let path = PathBuf::from(p);
            if !path.exists() {
                bail!("ZAP_ADB points to {}, which does not exist", path.display());
            }
            return Ok(Self { adb: path });
        }

        if let Some(home) = std::env::var_os("ANDROID_HOME") {
            let path = PathBuf::from(home).join("platform-tools").join("adb");
            if path.exists() {
                return Ok(Self { adb: path });
            }
        }

        Ok(Self {
            adb: PathBuf::from("adb"),
        })
    }

    /// Run adb with the given args, capturing stdout. Fails if adb is missing
    /// or exits non-zero.
    fn capture(&self, args: &[&str]) -> Result<String> {
        let output = Command::new(&self.adb)
            .args(args)
            .output()
            .map_err(|e| self.spawn_error(e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("adb {} failed: {}", args.join(" "), stderr.trim());
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Run adb inheriting stdio, so the user sees adb's own progress output.
    fn run_inherited(&self, args: &[&str]) -> Result<()> {
        let status = Command::new(&self.adb)
            .args(args)
            .status()
            .map_err(|e| self.spawn_error(e))?;

        if !status.success() {
            bail!("adb {} failed with {}", args.join(" "), status);
        }
        Ok(())
    }

    fn spawn_error(&self, e: std::io::Error) -> anyhow::Error {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!(
                "could not run adb (looked for `{}`).\n\
                 Install it with: brew install android-platform-tools\n\
                 Or set $ZAP_ADB / $ANDROID_HOME to point at it.",
                self.adb.display()
            )
        } else {
            anyhow::Error::new(e).context(format!("failed to run `{}`", self.adb.display()))
        }
    }
}

impl Transport for AdbTransport {
    fn name(&self) -> &'static str {
        "adb"
    }

    fn list_devices(&self) -> Result<Vec<Device>> {
        // `adb devices` output looks like:
        //   List of devices attached
        //   emulator-5554	device
        //   1234abcd	unauthorized
        let out = self.capture(&["devices"])?;
        let mut devices = Vec::new();
        for line in out.lines().skip(1) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(id), Some(state)) = (parts.next(), parts.next()) else {
                continue;
            };
            devices.push(Device {
                id: id.to_string(),
                label: None,
                state: state.to_string(),
            });
        }
        Ok(devices)
    }

    fn list_dir(&self, device: &str, remote_path: &str) -> Result<Vec<RemoteEntry>> {
        // `-1` one entry per line, `-p` appends `/` to directory names.
        let out = self
            .capture(&["-s", device, "shell", "ls", "-1", "-p", remote_path])
            .with_context(|| format!("listing {remote_path} on {device}"))?;

        let mut entries = Vec::new();
        for line in out.lines() {
            let raw = line.trim_end_matches('\r');
            if raw.is_empty() {
                continue;
            }
            let is_dir = raw.ends_with('/');
            let name = raw.trim_end_matches('/').to_string();
            if name.is_empty() {
                continue;
            }
            entries.push(RemoteEntry {
                name,
                is_dir,
                size: None,
            });
        }
        Ok(entries)
    }

    fn pull(&self, device: &str, remote_path: &str, local_path: &str) -> Result<()> {
        self.run_inherited(&["-s", device, "pull", "-a", remote_path, local_path])
    }

    fn push(&self, device: &str, local_path: &str, remote_path: &str) -> Result<()> {
        self.run_inherited(&["-s", device, "push", local_path, remote_path])
    }
}

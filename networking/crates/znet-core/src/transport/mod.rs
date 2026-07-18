//! Transport abstraction.
//!
//! A `Transport` is one way of moving files between a host (the Mac) and an
//! Android device: over USB via ADB, over Wi-Fi, over MTP, over Bluetooth, etc.
//! The CLI is written entirely against this trait, so adding a new transport
//! means writing a new `impl Transport` and nothing else.

pub mod adb;

use anyhow::Result;

/// An Android device reachable through some transport.
#[derive(Debug, Clone)]
pub struct Device {
    /// Stable identifier for the device within its transport (e.g. an ADB serial).
    pub id: String,
    /// Human-readable label, if the transport can provide one.
    pub label: Option<String>,
    /// Connection state as reported by the transport (e.g. "device", "unauthorized").
    pub state: String,
}

/// A single entry in a remote directory listing.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    /// Size in bytes, when the transport reports it.
    pub size: Option<u64>,
}

/// One way of moving files to and from Android devices.
pub trait Transport {
    /// Short name of this transport, e.g. "adb".
    fn name(&self) -> &'static str;

    /// List the devices currently reachable through this transport.
    fn list_devices(&self) -> Result<Vec<Device>>;

    /// List the contents of a directory on the device.
    fn list_dir(&self, device: &str, remote_path: &str) -> Result<Vec<RemoteEntry>>;

    /// Copy a file or directory from the device to the local filesystem.
    fn pull(&self, device: &str, remote_path: &str, local_path: &str) -> Result<()>;

    /// Copy a file or directory from the local filesystem to the device.
    fn push(&self, device: &str, local_path: &str, remote_path: &str) -> Result<()>;
}

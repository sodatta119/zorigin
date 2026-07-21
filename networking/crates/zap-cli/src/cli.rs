//! Command-line interface definition.

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "zap", version, about = "Fast file transfer between Android and macOS")]
pub struct Cli {
    /// Which transport to use.
    #[arg(long, value_enum, default_value_t = TransportKind::Adb, global = true)]
    pub transport: TransportKind,

    /// Target device id (e.g. an ADB serial). If omitted and exactly one
    /// device is connected, that device is used automatically.
    #[arg(long, short = 'd', global = true)]
    pub device: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Copy, Clone, ValueEnum)]
pub enum TransportKind {
    /// USB via the Android Debug Bridge.
    Adb,
}

#[derive(Subcommand)]
pub enum Command {
    /// List connected devices.
    Devices,

    /// List a directory on the device.
    Ls {
        /// Remote path on the device.
        #[arg(default_value = "/sdcard")]
        path: String,
    },

    /// Copy a file or directory from the device to the Mac.
    Pull {
        /// Path on the device.
        remote: String,
        /// Destination path on the Mac (defaults to the current directory).
        #[arg(default_value = ".")]
        local: String,
    },

    /// Copy a file or directory from the Mac to the device.
    Push {
        /// Path on the Mac.
        local: String,
        /// Destination path on the device.
        remote: String,
    },

    /// Download a file from another Zap over the LAN, using the native fast lane
    /// when available and falling back to HTTP. Give it a Zap download link, e.g.
    /// `zap get "http://192.168.1.5:8080/download?path=movie.mp4&k=<token>" ~/Downloads`.
    Get {
        /// Zap download URL. Must include `?path=<file>`; add `&k=<token>` if the
        /// server is secured. Plain http:// only for now.
        url: String,
        /// Destination file or directory (defaults to the current directory).
        #[arg(default_value = ".")]
        dest: String,
        /// Max parallel fast-lane connections. Adaptive mode ramps up to this;
        /// with --fixed it is the exact count.
        #[arg(long, default_value_t = 8)]
        streams: usize,
        /// Fast-lane chunk size in MiB. Adaptive mode starts here and tunes it;
        /// with --fixed it stays constant.
        #[arg(long = "chunk-mb", default_value_t = 4)]
        chunk_mb: u64,
        /// Disable adaptation: use exactly --streams connections and a constant
        /// --chunk-mb (useful for A/B throughput experiments).
        #[arg(long)]
        fixed: bool,
    },

    /// Upload a file to another Zap over the LAN, using the native fast lane when
    /// available and falling back to HTTP. Give it a Zap server link, e.g.
    /// `zap put ~/clip.mp4 "http://192.168.1.5:8080/?k=<token>"`.
    Put {
        /// Local file to upload.
        local: String,
        /// Zap server URL (include `?k=<token>` if the server is secured).
        url: String,
        /// Save on the server under this name (defaults to the local filename).
        #[arg(long)]
        name: Option<String>,
    },

    /// Start a web server so a phone can transfer files over Wi-Fi (no app).
    Serve {
        /// Directory to share and to receive uploads into.
        #[arg(long, default_value = ".")]
        dir: String,
        /// Port to listen on.
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Address to bind. Defaults to all interfaces so the phone can connect.
        #[arg(long, default_value = "0.0.0.0")]
        bind: std::net::IpAddr,
        /// Require a login (HTTP Basic auth). Defaults to zap/zap unless
        /// --user/--password are given.
        #[arg(long)]
        secure: bool,
        /// Username for the login (implies --secure).
        #[arg(long)]
        user: Option<String>,
        /// Password for the login (implies --secure).
        #[arg(long)]
        password: Option<String>,
    },
}

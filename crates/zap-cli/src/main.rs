mod cli;

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::Parser;

use cli::{Cli, Command, TransportKind};
use zap_core::transport::{adb::AdbTransport, Transport};
use zap_core::web::{self, Credentials, ServeConfig, ServerInfo};

fn main() {
    if let Err(e) = run() {
        eprintln!("zap: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let device_flag = cli.device.clone();

    match cli.command {
        // The web transport is server-mode and doesn't use a host-driven Transport.
        Command::Serve {
            dir,
            port,
            bind,
            secure,
            user,
            password,
        } => {
            let auth = if secure || user.is_some() || password.is_some() {
                Some(Credentials {
                    user: user.unwrap_or_else(|| "zap".to_string()),
                    pass: password.unwrap_or_else(|| "zap".to_string()),
                })
            } else {
                None
            };
            let login = auth.as_ref().map(|c| format!("{} / {}", c.user, c.pass));
            web::serve(
                ServeConfig {
                    dir: PathBuf::from(dir),
                    port,
                    bind,
                    auth,
                    history: None, // one-shot CLI: no persistent history
                },
                |info| {
                    print_banner(info);
                    if let Some(login) = &login {
                        println!("Login required — {login}");
                    }
                },
            )
        }

        Command::Devices => {
            let transport = build_transport(cli.transport)?;
            cmd_devices(transport.as_ref())
        }
        Command::Ls { path } => {
            let transport = build_transport(cli.transport)?;
            let device = resolve_device(transport.as_ref(), device_flag.as_deref())?;
            cmd_ls(transport.as_ref(), &device, &path)
        }
        Command::Pull { remote, local } => {
            let transport = build_transport(cli.transport)?;
            let device = resolve_device(transport.as_ref(), device_flag.as_deref())?;
            transport.pull(&device, &remote, &local)
        }
        Command::Push { local, remote } => {
            let transport = build_transport(cli.transport)?;
            let device = resolve_device(transport.as_ref(), device_flag.as_deref())?;
            transport.push(&device, &local, &remote)
        }
    }
}

fn build_transport(kind: TransportKind) -> Result<Box<dyn Transport>> {
    Ok(match kind {
        TransportKind::Adb => Box::new(AdbTransport::locate()?),
    })
}

fn cmd_devices(transport: &dyn Transport) -> Result<()> {
    let devices = transport.list_devices()?;
    if devices.is_empty() {
        println!("No devices found via {}.", transport.name());
        return Ok(());
    }
    for d in devices {
        match d.label {
            Some(label) => println!("{}\t{}\t{}", d.id, d.state, label),
            None => println!("{}\t{}", d.id, d.state),
        }
    }
    Ok(())
}

fn cmd_ls(transport: &dyn Transport, device: &str, path: &str) -> Result<()> {
    let entries = transport.list_dir(device, path)?;
    for e in entries {
        let suffix = if e.is_dir { "/" } else { "" };
        println!("{}{}", e.name, suffix);
    }
    Ok(())
}

/// Pick the device to operate on: the explicit `--device` if given, otherwise
/// the sole connected/ready device.
fn resolve_device(transport: &dyn Transport, requested: Option<&str>) -> Result<String> {
    if let Some(id) = requested {
        return Ok(id.to_string());
    }

    let ready: Vec<_> = transport
        .list_devices()?
        .into_iter()
        .filter(|d| d.state == "device")
        .collect();

    match ready.as_slice() {
        [] => bail!(
            "no ready devices found via {}. Connect a device (and authorize USB debugging), \
             or pass --device.",
            transport.name()
        ),
        [only] => Ok(only.id.clone()),
        many => {
            let serials: Vec<_> = many.iter().map(|d| d.id.as_str()).collect();
            bail!(
                "multiple devices connected; pass --device <id>. Available: {}",
                serials.join(", ")
            )
        }
    }
}

/// Present connection details for a freshly-started web server: the share path,
/// the URL, and a scannable QR code. This is CLI presentation — the core server
/// stays free of terminal concerns.
fn print_banner(info: &ServerInfo) {
    println!("⚡ Zap — sharing {}", info.dir.display());
    println!();
    match info.lan_ip {
        Some(ip) => {
            let url = info.url();
            println!("✓ Reachable at  {url}");
            println!("  (this device: {ip}, port {})", info.port);
            println!();
            println!("Open that URL on the other device (same Wi-Fi):");
            // The QR carries the pairing key when secured, so scanning it skips
            // the login prompt.
            if let Some(qr) = render_qr(&info.url_with_key()) {
                println!("\n{qr}");
            }
            if info.auth_token.is_some() {
                println!("(Scanning the QR signs in automatically — no password typing.)");
            }
        }
        None => {
            println!("⚠ No Wi-Fi/LAN IP detected — connect to Wi-Fi (not just a cable/hotspot).");
            println!(
                "Serving on port {}; find this machine's Wi-Fi IP and open http://<ip>:{}/",
                info.port, info.port
            );
        }
    }
    println!();
    println!("Both devices must be on the same Wi-Fi. If it won't connect,");
    println!("turn off your router's \"AP / client isolation\" setting.");
    println!("Press Ctrl-C to stop.");
}

/// Render a URL as a QR code using unicode half-blocks for the terminal.
fn render_qr(url: &str) -> Option<String> {
    use qrcode::render::unicode;
    use qrcode::QrCode;

    let code = QrCode::new(url.as_bytes()).ok()?;
    Some(code.render::<unicode::Dense1x2>().quiet_zone(true).build())
}

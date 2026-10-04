use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use qrcode::{QrCode, render::unicode};
use std::{
    io::{self, Write},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
use weld_control::{Request, Response, call, socket_path};
use weld_hoist_iroh::pairing::DevicePermissions;

#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long)]
    session: Option<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Pair,
    Devices {
        #[command(subcommand)]
        command: Devices,
    },
}
#[derive(Subcommand)]
enum Devices {
    List,
    Revoke { identity: String },
}

fn main() -> Result<()> {
    let args = Arguments::parse();
    let path = match args.socket {
        Some(path) => path,
        None => socket_path(
            &args
                .session
                .or_else(|| std::env::var("WAYLAND_DISPLAY").ok())
                .unwrap_or_else(|| "weld-0".into()),
        )?,
    };
    match args.command {
        Command::Pair => {
            let Response::Invitation(link) = call(&path, &Request::Pair)? else {
                anyhow::bail!("unexpected pairing response");
            };
            let code = QrCode::new(link.as_bytes())?;
            println!(
                "Scan with the phone camera/QR reader and open the Weld link.\nInvitation expires in two minutes.\n{}\n{link}",
                code.render::<unicode::Dense1x2>().build()
            );
            let deadline = Instant::now() + Duration::from_secs(120);
            while Instant::now() < deadline {
                if let Response::Pending(Some(candidate)) = call(&path, &Request::Pending)? {
                    println!(
                        "\n{:?} wants to pair\nIdentity: {}",
                        candidate.name, candidate.identity
                    );
                    print!(
                        "Allow browsing and hoisting running applications? Type the code shown on the phone (empty cancels): "
                    );
                    io::stdout().flush()?;
                    let mut answer = String::new();
                    io::stdin().read_line(&mut answer)?;
                    if answer.trim() != candidate.verification {
                        call(&path, &Request::Cancel)?;
                        anyhow::bail!("pairing cancelled: verification did not match");
                    }
                    call(
                        &path,
                        &Request::Approve {
                            identity: candidate.identity,
                            verification: answer.trim().into(),
                            permissions: DevicePermissions {
                                browse: true,
                                hoist: true,
                            },
                        },
                    )?;
                    println!(
                        "Paired. The phone can browse and request running applications; remote launch is not granted."
                    );
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(250));
            }
            anyhow::bail!("pairing invitation expired");
        }
        Command::Devices {
            command: Devices::List,
        } => {
            let Response::Devices(devices) = call(&path, &Request::Devices)? else {
                anyhow::bail!("unexpected devices response");
            };
            for device in devices {
                println!(
                    "{}  {:?}  browse={} hoist={}",
                    device.identity,
                    device.name,
                    device.permissions.browse,
                    device.permissions.hoist
                );
            }
        }
        Command::Devices {
            command: Devices::Revoke { identity },
        } => {
            call(&path, &Request::Revoke(identity)).context("revocation failed")?;
            println!("Device revoked and active connections closed.");
        }
    }
    Ok(())
}

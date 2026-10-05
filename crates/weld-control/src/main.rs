use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use qrcode::{QrCode, render::unicode};
use std::{
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
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
    Diagnostics {
        #[command(subcommand)]
        command: Diagnostics,
    },
    Pair,
    Devices {
        #[command(subcommand)]
        command: Devices,
    },
}
#[derive(Subcommand)]
enum Devices {
    List,
    Revoke {
        identity: String,
    },
    Diagnostics {
        identity: String,
        #[arg(long, action = clap::ArgAction::Set)]
        enabled: bool,
    },
}

#[derive(Subcommand)]
enum Diagnostics {
    List,
    Explain {
        id: weld_diagnostics::SessionId,
        #[arg(long)]
        verbose: bool,
    },
    Export {
        id: weld_diagnostics::SessionId,
        #[arg(long)]
        output: PathBuf,
    },
    ExplainFile {
        path: PathBuf,
        #[arg(long)]
        verbose: bool,
    },
}

fn main() -> Result<()> {
    let args = Arguments::parse();
    if let Command::Diagnostics {
        command: Diagnostics::ExplainFile { path, verbose },
    } = &args.command
    {
        let bundle = weld_diagnostics::ReportBundle::read(std::fs::File::open(path)?)?;
        print!("{}", weld_diagnostics::explain(&bundle).render(*verbose));
        return Ok(());
    }
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
        Command::Diagnostics { command } => match command {
            Diagnostics::List => {
                let Response::DiagnosticSessions(sessions) =
                    call(&path, &Request::DiagnosticSessions)?
                else {
                    anyhow::bail!("unexpected diagnostics response");
                };
                for (id, endpoint, ended, peer) in sessions {
                    println!(
                        "{id}  {endpoint:?}  {}  peer-report={peer}",
                        if ended { "ended" } else { "active" }
                    );
                }
            }
            Diagnostics::Explain { id, verbose } => {
                let Response::DiagnosticReport(bundle) =
                    call(&path, &Request::DiagnosticReport(id))?
                else {
                    anyhow::bail!("unexpected diagnostics response");
                };
                print!("{}", weld_diagnostics::explain(&bundle).render(verbose));
            }
            Diagnostics::Export { id, output } => {
                let Response::DiagnosticReport(bundle) =
                    call(&path, &Request::DiagnosticReport(id))?
                else {
                    anyhow::bail!("unexpected diagnostics response");
                };
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&output)?;
                bundle.write(file)?;
                println!("Saved {}", output.display());
            }
            Diagnostics::ExplainFile { .. } => anyhow::bail!("offline report already handled"),
        },
        Command::Devices {
            command: Devices::Diagnostics { identity, enabled },
        } => {
            call(&path, &Request::DiagnosticsPermission { identity, enabled })?;
            println!("Session-scoped diagnostic exchange enabled={enabled}");
        }
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
                                diagnostics: false,
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
                    "{}  {:?}  browse={} hoist={} diagnostics={}",
                    device.identity,
                    device.name,
                    device.permissions.browse,
                    device.permissions.hoist,
                    device.permissions.diagnostics
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

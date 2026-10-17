use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use qrcode::{QrCode, render::unicode};
use std::{
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use weld_control::access::AccessRequest;
use weld_control::{Request, Response, call, socket_path};
use weld_hoist_protocol::device::DevicePermissions;

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
    /// Serve window inspection tools to an MCP client over stdin/stdout.
    Mcp,
    /// Query the running host's managed windows.
    #[command(disable_help_flag = true)]
    Window {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<String>,
    },
    /// Inspect and revoke external application-control sessions.
    Access {
        #[command(subcommand)]
        command: Access,
    },
    /// Launch an application in the selected Weld session.
    Launch {
        /// Working directory; defaults to the calling shell's current directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Executable and exact arguments, following --.
        #[arg(required = true, num_args = 1.., last = true)]
        command: Vec<String>,
    },
    #[command(visible_aliases = ["diagnostic", "diag"])]
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
enum Access {
    Status {
        #[arg(long)]
        json: bool,
    },
    Revoke {
        #[arg(long, conflicts_with = "session", required_unless_present = "session")]
        all: bool,
        #[arg(long)]
        session: Option<u64>,
    },
    /// Permit fresh connections under existing local and paired-device approvals.
    Allow,
}

#[derive(Subcommand)]
enum Diagnostics {
    List,
    Explain {
        /// Session to explain; defaults to the newest recorded session.
        id: Option<weld_diagnostics::SessionId>,
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
        Command::Mcp => weld_control::mcp::run(path)?,
        Command::Window { arguments } => {
            weld_control::windows::WindowClient::new(path)
                .cli_run_with(std::iter::once("window".to_owned()).chain(arguments))
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        }
        Command::Access { command } => {
            let request = match command {
                Access::Status { .. } => AccessRequest::Status,
                Access::Revoke { all: true, .. } => AccessRequest::RevokeAll,
                Access::Revoke {
                    session: Some(id), ..
                } => AccessRequest::Revoke(weld_access::AccessSessionId(id)),
                Access::Revoke { .. } => anyhow::bail!("select --all or --session ID"),
                Access::Allow => AccessRequest::Allow,
            };
            match call(&path, &Request::Access(request))? {
                Response::AccessStatus(status) => {
                    if matches!(command, Access::Status { json: true }) {
                        println!("{}", serde_json::to_string_pretty(&status)?);
                    } else {
                        println!(
                            "New access sessions: {}",
                            if status.accepting {
                                "enabled"
                            } else {
                                "blocked"
                            }
                        );
                        for session in status.sessions {
                            println!(
                                "{}  {}  {}",
                                session.id.0,
                                if session.authorized {
                                    "authorized"
                                } else {
                                    "ended"
                                },
                                session.participant
                            );
                        }
                        for participant in status.blocked_participants {
                            println!("Reconnect blocked: {participant}");
                        }
                    }
                }
                Response::Ok => println!("Access policy updated."),
                _ => anyhow::bail!("unexpected access response"),
            }
        }
        Command::Launch { cwd, command } => {
            let (program, arguments) = command
                .split_first()
                .context("launch needs an executable")?;
            let current =
                std::env::current_dir().context("could not determine launch directory")?;
            let directory = cwd.map(|path| current.join(path)).unwrap_or(current);
            let request = weld_control::launch::LaunchRequest {
                program: program.clone(),
                arguments: arguments.to_vec(),
                directory,
            };
            let Response::Launched(receipt) = call(&path, &Request::Launch(request))? else {
                anyhow::bail!("unexpected launch response");
            };
            println!("Started process {} in Weld.", receipt.process_id);
        }
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
                let id = match id {
                    Some(id) => id,
                    None => {
                        let Response::DiagnosticSessions(sessions) =
                            call(&path, &Request::DiagnosticSessions)?
                        else {
                            anyhow::bail!("unexpected diagnostics response");
                        };
                        sessions
                            .last()
                            .context("no diagnostic sessions recorded yet")?
                            .0
                    }
                };
                let bundle = collect_report(&path, id)?;
                println!("Session {id}");
                print!("{}", weld_diagnostics::explain(&bundle).render(verbose));
            }
            Diagnostics::Export { id, output } => {
                let bundle = collect_report(&path, id)?;
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
                        "Allow browsing, thumbnail previews and hoisting running applications? Type the code shown on the phone (empty cancels): "
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
                                thumbnails: true,
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
                    "{}  {:?}  browse={} hoist={} diagnostics={} thumbnails={}",
                    device.identity,
                    device.name,
                    device.permissions.browse,
                    device.permissions.hoist,
                    device.permissions.diagnostics,
                    device.permissions.thumbnails
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

fn collect_report(
    path: &Path,
    id: weld_diagnostics::SessionId,
) -> Result<weld_diagnostics::ReportBundle> {
    let Response::DiagnosticCollection(collection) = call(path, &Request::CollectDiagnostics(id))?
    else {
        anyhow::bail!("unexpected diagnostics response");
    };
    eprintln!("Peer: {}", collection.status);
    if collection.status != weld_diagnostics::collection::CollectionStatus::Collected
        && collection.bundle.peer.is_some()
    {
        eprintln!("Using previously collected peer evidence.");
    }
    Ok(collection.bundle)
}

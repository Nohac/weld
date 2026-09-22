//! Disposable ADB transport experiment. No production Weld dependencies or app data.
mod raw;
mod tunnel;
mod wire;
mod workload;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, SecretKey, TransportAddr,
    dns::{DnsProtocol, DnsResolver},
    endpoint::{Connection, PortmapperConfig, presets},
};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, split},
    net::{TcpListener, TcpStream},
    time::timeout,
};

pub struct Flow {
    read: Box<dyn AsyncRead + Unpin + Send>,
    write: Box<dyn AsyncWrite + Unpin + Send>,
}
impl Flow {
    fn duplex(stream: DuplexStream) -> Self {
        let (read, write) = split(stream);
        Self {
            read: Box::new(read),
            write: Box::new(write),
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    Raw,
    Iroh,
}

#[derive(Parser)]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Identity {
        path: PathBuf,
    },
    Server {
        #[arg(long, value_enum)]
        mode: Mode,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        peer: EndpointId,
        #[arg(long, default_value_t=5, value_parser=clap::value_parser!(u64).range(1..=10))]
        seconds: u64,
    },
    Client {
        #[arg(long, value_enum)]
        mode: Mode,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        peer: EndpointId,
        #[arg(long)]
        connect: SocketAddr,
    },
}

const ALPN: &[u8] = b"weld/adb-probe/1";

async fn endpoint(
    stream: TcpStream,
    key: SecretKey,
    peer: EndpointId,
) -> Result<(Endpoint, tunnel::Driver, Arc<tunnel::Counters>)> {
    let (transport, driver, counters) = tunnel::open(stream, key.public(), peer)?;
    // No Android JVM/system resolver, and no names need resolving in this test.
    let resolver = DnsResolver::builder()
        .with_nameserver(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9),
            DnsProtocol::Udp,
        )
        .build();
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(key)
        .alpns(vec![ALPN.to_vec()])
        .clear_ip_transports()
        .clear_relay_transports()
        .clear_address_lookup()
        .portmapper_config(PortmapperConfig::Disabled)
        .dns_resolver(resolver)
        .add_custom_transport(transport)
        .bind()
        .await?;
    ensure!(
        endpoint.bound_sockets().is_empty(),
        "IP sockets must be disabled"
    );
    Ok((endpoint, driver, counters))
}

fn verify(connection: &Connection, expected: EndpointId) -> Result<()> {
    ensure!(
        connection.remote_id() == expected,
        "unexpected authenticated peer"
    );
    let paths = connection.paths();
    let selected = paths
        .iter()
        .find(|path| path.is_selected())
        .context("no selected path")?;
    ensure!(
        matches!(selected.remote_addr(), TransportAddr::Custom(address) if address.id() == tunnel::TRANSPORT_ID),
        "non-custom transport selected"
    );
    println!(
        "{}",
        serde_json::json!({"event":"authenticated", "transport":"custom-adb", "peer":expected.to_string()})
    );
    Ok(())
}

fn read_key(path: &PathBuf) -> Result<SecretKey> {
    let bytes: [u8; 32] = fs::read(path)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid identity size"))?;
    Ok(SecretKey::from_bytes(&bytes))
}

async fn run(command: Command) -> Result<()> {
    let (stream, key, peer, mode, server, seconds) = match command {
        Command::Identity { path } => {
            let key = SecretKey::generate();
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(&key.to_bytes())?;
            println!("{}", key.public());
            return Ok(());
        }
        Command::Server {
            mode,
            identity,
            peer,
            seconds,
        } => {
            let key = read_key(&identity)?;
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
            println!(
                "{}",
                serde_json::json!({"event":"ready", "port":listener.local_addr()?.port()})
            );
            std::io::stdout().flush()?;
            let (stream, _) = timeout(Duration::from_secs(15), listener.accept()).await??;
            (stream, key, peer, mode, true, seconds)
        }
        Command::Client {
            mode,
            identity,
            peer,
            connect,
        } => {
            ensure!(
                connect.ip().is_loopback(),
                "probe connects only to loopback"
            );
            let key = read_key(&identity)?;
            let stream = timeout(Duration::from_secs(10), TcpStream::connect(connect)).await??;
            (stream, key, peer, mode, false, 0)
        }
    };
    println!(
        "{}",
        serde_json::json!({"event":"mode", "mode":format!("{mode:?}"),
        "frame_hz":90, "iroh_send_capacity":tunnel::SEND_CAPACITY,
        "receive_and_raw_capacity":wire::CAPACITY,
        "packet_counter_note":"sent_packets counts enqueue, not socket completion; queued packets can cross phase boundaries",
        "caveat":"Synthetic traffic only. ADB is an ordered byte stream; this is not native UDP or video presentation latency."})
    );
    match mode {
        Mode::Raw => {
            let (control, media, _driver) = raw::open(stream)?;
            if server {
                workload::serve(control, media, seconds, None).await?;
            } else {
                workload::receive(control, media, None).await?;
            }
        }
        Mode::Iroh => {
            let (endpoint, _driver, counters) = endpoint(stream, key, peer).await?;
            let connection = timeout(Duration::from_secs(10), async {
                if server {
                    Ok::<_, anyhow::Error>(
                        endpoint.accept().await.context("endpoint closed")?.await?,
                    )
                } else {
                    let address = EndpointAddr::from_parts(
                        peer,
                        [TransportAddr::Custom(tunnel::address(peer))],
                    );
                    Ok(endpoint.connect(address, ALPN).await?)
                }
            })
            .await??;
            verify(&connection, peer)?;
            if server {
                let (write, read) = connection.accept_bi().await?;
                let media = connection.open_uni().await?;
                workload::serve(
                    Flow {
                        read: Box::new(read),
                        write: Box::new(write),
                    },
                    Flow {
                        read: Box::new(tokio::io::empty()),
                        write: Box::new(media),
                    },
                    seconds,
                    Some(counters),
                )
                .await?;
            } else {
                let (mut write, read) = connection.open_bi().await?;
                // Opening a QUIC stream alone does not transmit it. Prime it so
                // the source can accept it before creating the media stream.
                write.write_all(&0_u64.to_be_bytes()).await?;
                let mut read = read;
                let mut initial = [0; 8];
                // The source starts echo concurrently with its media producer.
                read.read_exact(&mut initial).await?;
                ensure!(initial == [0; 8], "invalid initial echo");
                let media = connection.accept_uni().await?;
                workload::receive(
                    Flow {
                        read: Box::new(read),
                        write: Box::new(write),
                    },
                    Flow {
                        read: Box::new(media),
                        write: Box::new(tokio::io::sink()),
                    },
                    Some(counters),
                )
                .await?;
            }
            let stats = connection.stats();
            println!(
                "{}",
                serde_json::json!({"event":"quic_totals",
                "lost_packets":stats.lost_packets, "lost_bytes":stats.lost_bytes,
                "note":"QUIC loss detection includes adapter drops and spurious retransmissions, not just physical link loss"})
            );
            connection.close(0_u32.into(), b"probe complete");
            // Iroh documents a roughly three-second conservative QUIC drain.
            // Do not race that internal timeout with an equal outer deadline.
            timeout(Duration::from_secs(8), endpoint.close())
                .await
                .context("endpoint close timed out")?;
        }
    }
    Ok(())
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    timeout(Duration::from_secs(60), run(Arguments::parse().command))
        .await
        .context("probe runtime limit")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn custom_only_connection_verifies_identity_and_rejects_wrong_peer() -> Result<()> {
        timeout(Duration::from_secs(10), async {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
            let client_stream = TcpStream::connect(listener.local_addr()?).await?;
            let (server_stream, _) = listener.accept().await?;
            let source_key = SecretKey::generate();
            let destination_key = SecretKey::generate();
            let source_id = source_key.public();
            let destination_id = destination_key.public();
            let (source, _source_driver, _) =
                endpoint(server_stream, source_key, destination_id).await?;
            let (destination, _destination_driver, _) =
                endpoint(client_stream, destination_key, source_id).await?;
            let address = EndpointAddr::from_parts(
                source_id,
                [TransportAddr::Custom(tunnel::address(source_id))],
            );
            let (server, client) = tokio::try_join!(
                async { Ok::<_, anyhow::Error>(source.accept().await.context("closed")?.await?) },
                async { Ok::<_, anyhow::Error>(destination.connect(address, ALPN).await?) }
            )?;
            verify(&server, destination_id)?;
            verify(&client, source_id)?;
            assert!(verify(&server, SecretKey::generate().public()).is_err());
            server.close(0_u32.into(), b"test complete");
            tokio::join!(source.close(), destination.close());
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }
}

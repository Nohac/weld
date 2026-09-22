//! Synthetic raw baseline: two logical flows multiplexed over the same ADB TCP socket.
//! No encryption or peer-authentication claim. Never carries real application data.
use crate::{Flow, tunnel::Driver, wire};
use std::io;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf, duplex, split},
    net::TcpStream,
    sync::mpsc,
};

async fn outgoing(
    mut reader: ReadHalf<DuplexStream>,
    id: u8,
    sender: mpsc::Sender<Vec<u8>>,
) -> io::Result<()> {
    let mut buffer = vec![0; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer).await?;
        let mut packet = Vec::with_capacity(count + 1);
        packet.push(id);
        packet.extend_from_slice(&buffer[..count]);
        sender
            .send(packet)
            .await
            .map_err(|_| io::Error::other("raw writer closed"))?;
        if count == 0 {
            return Ok(());
        }
    }
}

async fn incoming(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    mut control: WriteHalf<DuplexStream>,
    mut media: WriteHalf<DuplexStream>,
) -> io::Result<()> {
    while let Some(packet) = wire::read(&mut reader).await? {
        let (id, payload) = packet
            .split_first()
            .ok_or_else(|| io::Error::other("empty raw record"))?;
        let writer = match id {
            0 => &mut control,
            1 => &mut media,
            _ => return Err(io::Error::other("unknown raw flow")),
        };
        if payload.is_empty() {
            writer.shutdown().await?;
        } else {
            writer.write_all(payload).await?;
        }
    }
    Ok(())
}

pub fn open(stream: TcpStream) -> io::Result<(Flow, Flow, Driver)> {
    stream.set_nodelay(true)?;
    let (reader, mut writer) = stream.into_split();
    let (control, control_wire) = duplex(64 * 1024);
    let (media, media_wire) = duplex(64 * 1024);
    let (control_read, control_write) = split(control_wire);
    let (media_read, media_write) = split(media_wire);
    let (control_send, mut controls) = mpsc::channel::<Vec<u8>>(wire::CAPACITY);
    let (media_send, mut frames) = mpsc::channel::<Vec<u8>>(wire::CAPACITY);
    let driver = Driver(tokio::spawn(async move {
        let write = async {
            let mut control_open = true;
            let mut media_open = true;
            while control_open || media_open {
                let packet = tokio::select! {
                    biased;
                    packet = controls.recv(), if control_open => {
                        if packet.is_none() { control_open = false; }
                        packet
                    }
                    packet = frames.recv(), if media_open => {
                        if packet.is_none() { media_open = false; }
                        packet
                    }
                };
                if let Some(packet) = packet {
                    wire::write(&mut writer, &packet).await?;
                }
            }
            writer.shutdown().await
        };
        tokio::try_join!(
            incoming(reader, control_write, media_write),
            write,
            outgoing(control_read, 0, control_send),
            outgoing(media_read, 1, media_send),
        )
        .map(|_| ())
    }));
    Ok((Flow::duplex(control), Flow::duplex(media), driver))
}

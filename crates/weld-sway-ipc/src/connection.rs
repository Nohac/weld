//! Typed mode queries and subscriptions for one admitted IPC client.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::{
    net::{UnixStream, unix::OwnedWriteHalf},
    sync::watch,
};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::{
    ModeSnapshot,
    codec::IpcCodec,
    protocol::{BindingState, EventKind, ModeEvent, Request, Response},
};

type ReplyWriter = FramedWrite<OwnedWriteHalf, IpcCodec>;

pub(crate) async fn run(
    stream: UnixStream,
    mut snapshot: watch::Receiver<ModeSnapshot>,
) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let mut requests = FramedRead::new(reader, IpcCodec);
    let mut replies = FramedWrite::new(writer, IpcCodec);
    let mut subscribed = false;
    let mut last_mode = None;
    loop {
        tokio::select! {
            request = requests.next() => {
                let Some(request) = request else {
                    return Ok(());
                };
                let current = snapshot.borrow().clone();
                let response = match request? {
                    Request::Subscribe(events) => {
                        let success = events.iter().all(|event| *event == EventKind::Mode);
                        send(&mut replies, Response::Subscription { success }).await?;
                        if success {
                            subscribed |= events.contains(&EventKind::Mode);
                            if subscribed {
                                let mode = mode_event(&current);
                                send(&mut replies, Response::Mode(&mode)).await?;
                                last_mode = Some(mode);
                            }
                        }
                        continue;
                    }
                    Request::GetBindingModes => Response::BindingModes(&current.names),
                    Request::GetBindingState => Response::BindingState(BindingState {
                        name: &current.name,
                    }),
                    Request::RunCommand => Response::CommandRejected,
                    Request::Unsupported(kind) => Response::Unsupported(kind),
                };
                send(&mut replies, response).await?;
            }
            changed = snapshot.changed(), if subscribed => {
                changed?;
                let mode = mode_event(&snapshot.borrow_and_update());
                if last_mode.as_ref() != Some(&mode) {
                    send(&mut replies, Response::Mode(&mode)).await?;
                    last_mode = Some(mode);
                }
            }
        }
    }
}

fn mode_event(snapshot: &ModeSnapshot) -> ModeEvent {
    ModeEvent {
        change: snapshot.name.clone(),
        pango_markup: snapshot.pango_markup,
    }
}

async fn send(writer: &mut ReplyWriter, response: Response<'_>) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), writer.send(response))
        .await
        .context("IPC client stopped reading")?
}

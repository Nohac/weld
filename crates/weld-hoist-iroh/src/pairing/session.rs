//! Receiver stream setup over an authenticated Iroh endpoint.
use super::*;
use crate::{
    IrohDestinationPeer,
    admission::PendingConnection,
    channel::{LaneTask, PacketLane, PacketReader, PacketWriter},
    framing::{read_record, write_record},
    host::HostLifetime,
    peer::spawn_destination_peer,
};
use iroh::Endpoint;
use std::sync::Weak;
use weld_hoist_core::session::{device::DeviceClient, negotiation, services::ServiceClient};
use weld_hoist_encoded::EncodedDestinationTransport;
use weld_hoist_protocol::{
    device::{DeviceHello as Hello, DeviceWelcome as Welcome},
    service::LANE_MARKER,
};

/// Receiver media peer plus an independent, bounded application-control channel.
/// Dropping the session closes its peer and cancels any pending control exchange.
pub struct DeviceSession {
    pub services: ServiceClient,
    _service_task: LaneTask,
    pub control: DeviceClient,
    pub peer: IrohDestinationPeer,
}
impl Drop for DeviceSession {
    fn drop(&mut self) {
        self.peer.disconnect();
    }
}
pub struct PendingDeviceSession {
    pub(crate) result: oneshot::Receiver<Result<DeviceSession, String>>,
    pub(crate) cancel: Option<oneshot::Sender<()>>,
}
impl PendingDeviceSession {
    pub fn poll(&mut self) -> Result<Option<DeviceSession>> {
        match self.result.try_recv() {
            Ok(result) => {
                self.cancel = None;
                result.map(Some).map_err(anyhow::Error::msg)
            }
            Err(oneshot::error::TryRecvError::Empty) => Ok(None),
            Err(_) => anyhow::bail!("device connection stopped"),
        }
    }
}
impl Drop for PendingDeviceSession {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

pub(crate) async fn connect_session(
    host: Weak<HostLifetime>,
    endpoint: Endpoint,
    profile: IrohConnectionProfile,
    preferences: weld_hoist_protocol::presentation::ReceiverPreferences,
    notifier: IrohNotifier,
    result: oneshot::Sender<Result<DeviceSession, String>>,
) -> Result<()> {
    let setup = async {
        let guard = PendingConnection::new(
            endpoint
                .connect(profile.endpoint_addr()?, SESSION_ALPN)
                .await?,
        );
        let (mut send, mut recv) = guard.connection.open_bi().await?;
        write_record(
            &mut send,
            &Hello {
                preferences: preferences.clone(),
            },
        )
        .await?;
        let welcome: Welcome = read_record(&mut recv).await?;
        negotiation::presentation(&preferences, welcome.codec)?;
        let (control_send, mut control_recv) = guard.connection.accept_bi().await?;
        let mut marker = [0; 8];
        control_recv.read_exact(&mut marker).await?;
        ensure!(&marker == b"weldctl1", "invalid device media channel");
        let media = guard.connection.accept_uni().await?;
        let (service_send, mut service_recv) = guard.connection.accept_bi().await?;
        service_recv.read_exact(&mut marker).await?;
        ensure!(&marker == LANE_MARKER, "invalid service channel");
        let service_wake = notifier.clone();
        let (services, driver) = ServiceClient::new(move || {
            let _ = service_wake.notify();
        });
        let service_connection = guard.connection.clone();
        let service_task = LaneTask(tokio::spawn(async move {
            tokio::select! {
                result = driver.run(PacketLane::new(service_send, service_recv)) => {
                    if let Err(error) = result { tracing::debug!(%error, "service lane ended"); }
                },
                _ = service_connection.closed() => {},
            }
        }));
        let peer = spawn_destination_peer(
            host.upgrade().context("Iroh host closed")?,
            guard.connection.clone(),
            control_send,
            control_recv,
            media,
            notifier.clone(),
            welcome.codec,
        );
        Ok::<_, anyhow::Error>((guard, send, recv, peer, services, service_task))
    };
    let (mut guard, send, recv, peer, services, service_task) =
        match tokio::time::timeout(Duration::from_secs(15), setup)
            .await
            .context("device connection timed out")
            .and_then(|result| result)
        {
            Ok(value) => value,
            Err(error) => {
                let _ = result.send(Err(format!("{error:#}")));
                notifier.notify()?;
                return Ok(());
            }
        };
    guard.admitted_session();
    let recorder = peer.diagnostics();
    let wake = notifier.clone();
    let (control, driver) = DeviceClient::new(move || {
        let _ = wake.notify();
    });
    if result
        .send(Ok(DeviceSession {
            services,
            _service_task: service_task,
            control,
            peer,
        }))
        .is_err()
    {
        return Ok(());
    }
    notifier.notify()?;
    tokio::select! {
        result = driver.run(PacketWriter(send), PacketReader(recv), recorder.as_ref()) => result,
        _ = guard.connection.closed() => Ok(()),
    }
}

#[cfg(test)]
mod tests;

//! Outgoing enrollment and application-session setup with supervised lifetimes.
use super::{
    channel::{receive_record, send_record},
    device::DeviceClient,
    negotiation,
    pairing::enrollment::{self, PairingProgress},
    server::{DEVICE_SERVICE, ENROLLMENT_SERVICE},
    services::ServiceClient,
};
use anyhow::{Context, Result};
use std::{future::Future, time::Duration};
use tokio::sync::watch;
use weld_diagnostics::Recorder;
use weld_hoist_protocol::{
    device::{DeviceHello, DeviceWelcome},
    presentation::ReceiverPreferences,
};
use weld_media::VideoCodec;
use weld_session::{
    ConnectionControl, Connector, Operation, SessionRuntime, Wake, connection::Duplex,
};

pub use weld_session::connection::PacketConnection as ControlConnection;
/// Encoded-hoist binding capabilities used after presentation negotiation.
pub trait DestinationConnection: ControlConnection + Sync {
    type Peer: ConnectionControl + Send + 'static;
    fn destination(
        &self,
        codec: VideoCodec,
        wake: Wake,
    ) -> impl Future<Output = Result<DestinationChannels<Self::Peer, Self::Writer, Self::Reader>>> + Send;
}
pub struct DestinationChannels<P, W, R> {
    pub peer: P,
    pub services: (W, R),
    pub recorder: Option<Recorder>,
}

/// Application control and independent service requests for one live receiver.
pub struct DeviceSession<P: ConnectionControl> {
    pub services: ServiceClient,
    pub control: DeviceClient,
    pub peer: P,
}
impl<P: ConnectionControl> Drop for DeviceSession<P> {
    fn drop(&mut self) {
        self.peer.close();
    }
}

pub fn connect<C>(
    runtime: &SessionRuntime,
    connector: C,
    preferences: ReceiverPreferences,
    wake: Wake,
) -> Result<Operation<DeviceSession<<C::Connection as DestinationConnection>::Peer>>>
where
    C: Connector,
    C::Connection: DestinationConnection,
{
    let notify = wake.clone();
    runtime.start_operation("weld/device-client/1", 8, wake, |session| session.peer.close(), move |completion| async move {
        let setup = async {
            let connection = connector.connect(DEVICE_SERVICE).await?;
            let (mut send, mut receive) = connection.open_control().await?;
            send_record(&mut send, &DeviceHello { preferences: preferences.clone() }).await?;
            let welcome: DeviceWelcome = receive_record(&mut receive).await?;
            negotiation::presentation(&preferences, welcome.codec)?;
            let channels = connection.destination(welcome.codec, notify.clone()).await?;
            Ok::<_, anyhow::Error>((connection, send, receive, channels))
        };
        let (connection, send, receive, channels) = tokio::time::timeout(Duration::from_secs(15), setup)
            .await.context("device connection timed out")??;
        let service_wake = notify.clone();
        let (services, service_driver) = ServiceClient::new(move || service_wake());
        let (control, driver) = DeviceClient::new(move || notify());
        completion.send(DeviceSession { services, control, peer: channels.peer })?;
        let control = driver.run(send, receive, channels.recorder.as_ref());
        let service = service_driver.run(Duplex(channels.services.0, channels.services.1));
        tokio::pin!(control);
        let drivers = async {
            tokio::select! {
                result = &mut control => result,
                result = service => {
                    if let Err(error) = result { tracing::debug!(%error, "service lane ended"); }
                    control.await
                }
            }
        };
        tokio::select! { result = drivers => result, result = connection.closed() => result }
    })
}

/// Enrollment status remains observable until the caller retires the operation.
pub struct PendingPairing {
    progress: watch::Receiver<PairingProgress>,
    _operation: Operation<()>,
}
impl PendingPairing {
    pub fn progress(&self) -> PairingProgress {
        let progress = self.progress.borrow().clone();
        if matches!(
            progress,
            PairingProgress::Connecting | PairingProgress::Verify { .. }
        ) && self.progress.has_changed().is_err()
        {
            PairingProgress::Failed("pairing operation stopped".into())
        } else {
            progress
        }
    }
}
pub struct Enrollment {
    pub token: [u8; 32],
    pub device_name: String,
    pub host_name: String,
}
pub fn pair<C>(
    runtime: &SessionRuntime,
    connector: C,
    enrollment: Enrollment,
    wake: Wake,
) -> Result<PendingPairing>
where
    C: Connector,
    C::Connection: ControlConnection,
{
    let (progress, receiver) = watch::channel(PairingProgress::Connecting);
    let notify = wake.clone();
    let operation = runtime.start_operation(
        "weld/enrollment-client/1",
        8,
        wake,
        |_| {},
        move |completion| async move {
            let exchange = async {
                let connection = connector.connect(ENROLLMENT_SERVICE).await?;
                let (send, receive) = connection.open_control().await?;
                enrollment::enroll(
                    &mut Duplex(send, receive),
                    enrollment.token,
                    enrollment.device_name,
                    enrollment.host_name,
                    |state| {
                        progress.send_replace(state);
                        notify();
                        Ok(())
                    },
                )
                .await
            };
            let result = tokio::time::timeout(Duration::from_secs(125), exchange)
                .await
                .context("pairing timed out")
                .and_then(|result| result);
            match result {
                Ok(()) => completion.send(()),
                Err(error) => {
                    progress.send_replace(PairingProgress::Failed(format!("{error:#}")));
                    notify();
                    Err(error)
                }
            }
        },
    )?;
    Ok(PendingPairing {
        progress: receiver,
        _operation: operation,
    })
}

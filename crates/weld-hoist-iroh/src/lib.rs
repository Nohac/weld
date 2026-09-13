//! Encoded hoist transport over authenticated Iroh connections.

mod adapter;
mod admission;
mod device;
mod diagnostics;
mod framing;
mod host;
mod inbox;
mod input_outbox;
mod media_queue;
#[cfg(feature = "vaapi")]
mod native;
mod notifier;
mod peer;
mod pending_source;
mod private_file;
mod rendezvous;

pub use adapter::{
    IrohDestinationEndpoint, destination_registration_with_backend,
    source_registration_with_backend,
};
pub use device::{IrohConnectionProfile, IrohDeviceIdentity, IrohTrustedPeers};
pub use host::{
    IrohDnsPolicy, IrohHost, IrohNetwork, PendingDestinationConnection, PendingSourceAdmission,
};
#[cfg(feature = "vaapi")]
pub use native::{
    IrohSourceRegistrationOptions, destination_registration, pending_source_registration,
    source_registration,
};
pub use notifier::IrohNotifier;
pub use peer::{IrohDestinationPeer, IrohSourcePeer};
pub use pending_source::pending_source_registration_with_backend;

/// Authenticated Iroh endpoint identity, kept opaque to Weld policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IrohPeerIdentity(String);

impl IrohPeerIdentity {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    mod portable_receiver;
    mod portable_source;
    use std::{path::Path, thread, time::Duration};

    use weld_client::{ClientId, ClientSourceId, ClientSurfaceId};
    use weld_hoist_encoded::{
        EncodedDestinationTransport, EncodedSourceTransport, ReceiveBudget, SourceTransportPacket,
    };
    use weld_hoist_protocol::{
        DestinationEnvelope, DestinationMessage, HoistSessionId, MediaEnvelope, SourceEnvelope,
        SourceMessage,
    };
    use weld_media::{
        EncodedAccessUnit, EncodedFrameKind, MediaFrameId, MediaStreamId, StreamGeneration,
        VideoCodec,
    };

    use super::*;

    fn send_source(
        source: &impl EncodedSourceTransport,
        packet: SourceTransportPacket,
    ) -> weld_hoist_core::HoistPortResult<()> {
        assert!(matches!(
            source.try_send(packet)?,
            weld_hoist_encoded::SendStatus::Sent
        ));
        Ok(())
    }

    #[test]
    fn direct_hosts_exchange_independent_control_and_media() {
        let directory = rendezvous::tests::ExchangeDirectory::new();
        let ticket = directory.0.join("source.ticket");
        let expected = directory.0.join("destination.identity");
        let source_host = IrohHost::bind(IrohNetwork::Direct).expect("source host");
        let destination_host = IrohHost::bind(IrohNetwork::Direct).expect("destination host");
        destination_host
            .publish_identity(&expected)
            .expect("approved destination identity");
        let source_notifier = IrohNotifier::new(|| Ok(()));
        let destination_notifier = IrohNotifier::new(|| Ok(()));
        let source_ticket = ticket.clone();
        let source = thread::spawn(move || {
            source_host
                .accept_source(
                    source_ticket,
                    expected,
                    VideoCodec::H264,
                    source_notifier,
                    Duration::from_secs(10),
                )
                .expect("accepted source peer")
        });
        wait_for_ticket(&ticket);
        let destination = destination_host
            .connect_destination(
                &ticket,
                vec![VideoCodec::H264],
                destination_notifier,
                Duration::from_secs(10),
            )
            .expect("connected destination peer");
        let source = source.join().expect("source thread");
        assert_eq!(source.codec(), VideoCodec::H264);
        assert_eq!(destination.codec(), VideoCodec::H264);
        assert!(!source.identity().as_str().is_empty());
        assert!(!destination.identity().as_str().is_empty());

        let session = HoistSessionId::new(1);
        let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 2), 3);
        send_source(
            &source,
            SourceTransportPacket::Control(SourceEnvelope {
                session,
                message: SourceMessage::Mapped { surface },
            }),
        )
        .expect("source control");
        let received = wait_for(|| {
            destination
                .drain(ReceiveBudget::ALL)
                .ok()
                .filter(|items| !items.is_empty())
        });
        assert!(matches!(
            &received[0],
            SourceTransportPacket::Control(SourceEnvelope {
                session: observed_session,
                message: SourceMessage::Mapped { surface: observed_surface },
            }) if *observed_session == session && *observed_surface == surface
        ));

        destination
            .send(DestinationEnvelope {
                session,
                message: DestinationMessage::Reclaim,
            })
            .expect("destination control");
        let returned = wait_for(|| source.drain().ok().filter(|items| !items.is_empty()));
        assert!(matches!(returned[0].message, DestinationMessage::Reclaim));

        let cursor = weld_client::ClientCursor::Image(
            weld_client::ClientCursorImage::new(2, 1, (1, 0), vec![255; 8]).expect("cursor"),
        );
        send_source(
            &source,
            SourceTransportPacket::Control(SourceEnvelope {
                session,
                message: SourceMessage::Cursor {
                    update: weld_client::ClientCursorUpdate {
                        surface,
                        cursor: cursor.clone(),
                    },
                    sequence: 1,
                },
            }),
        )
        .expect("cursor control");
        let received = wait_for(|| {
            destination
                .drain(ReceiveBudget::ALL)
                .ok()
                .filter(|items| !items.is_empty())
        });
        assert!(
            matches!(&received[0], SourceTransportPacket::Control(SourceEnvelope {
            message: SourceMessage::Cursor { update, sequence: 1 }, .. }) if update.cursor == cursor)
        );
        destination
            .send(DestinationEnvelope {
                session,
                message: DestinationMessage::CursorReceived {
                    surface,
                    sequence: 1,
                },
            })
            .expect("cursor ack");
        let returned = wait_for(|| source.drain().ok().filter(|items| !items.is_empty()));
        assert!(matches!(
            returned[0].message,
            DestinationMessage::CursorReceived { sequence: 1, .. }
        ));

        let access_unit = EncodedAccessUnit {
            frame: MediaFrameId::new(MediaStreamId::new(4), StreamGeneration::new(5), 6),
            codec: VideoCodec::H264,
            kind: EncodedFrameKind::Keyframe,
            timestamp_micros: 7,
            payload: vec![8, 9, 10],
        };
        send_source(
            &source,
            SourceTransportPacket::Media(MediaEnvelope {
                session,
                access_unit: access_unit.clone(),
            }),
        )
        .expect("source media");
        let received = wait_for(|| {
            destination
                .drain(ReceiveBudget::ALL)
                .ok()
                .filter(|items| !items.is_empty())
        });
        assert!(matches!(
            &received[0],
            SourceTransportPacket::Media(MediaEnvelope {
                session: observed_session,
                access_unit: observed,
            }) if *observed_session == session && *observed == access_unit
        ));

        // No reverse message is sent for any of these media records. Admission
        // is local, and the receiver's two-record inbox is drained in FIFO order.
        for sequence in 7..27 {
            let mut unit = access_unit.clone();
            unit.frame.sequence = sequence;
            send_source(
                &source,
                SourceTransportPacket::Media(MediaEnvelope {
                    session,
                    access_unit: unit,
                }),
            )
            .expect("next frame without ACK");
            let received = wait_for(|| {
                destination
                    .drain(ReceiveBudget::ALL)
                    .ok()
                    .filter(|items| !items.is_empty())
            });
            assert!(
                matches!(&received[0], SourceTransportPacket::Media(packet) if packet.access_unit.frame.sequence == sequence)
            );
        }
        portable_receiver::check_registration(&source, destination.clone());
        source.disconnect();
        wait_for(|| (!destination.is_available()).then_some(()));
        let _ = std::fs::remove_file(ticket);
    }

    fn wait_for_ticket(path: &Path) {
        wait_for(|| {
            path.metadata()
                .ok()
                .filter(|metadata| metadata.len() > 0)
                .map(|_| ())
        });
    }

    #[test]
    fn invalid_approved_identity_fails_without_anonymous_fallback() {
        let directory = rendezvous::tests::ExchangeDirectory::new();
        let expected = directory.0.join("invalid.identity");
        rendezvous::publish(&expected, "not-an-endpoint-id").expect("invalid identity fixture");
        let host = IrohHost::bind(IrohNetwork::Direct).expect("host");
        let notifier = IrohNotifier::new(|| Ok(()));
        let error = host
            .accept_source(
                directory.0.join("source.ticket"),
                expected,
                VideoCodec::H264,
                notifier,
                Duration::from_secs(1),
            )
            .err()
            .expect("invalid identity rejected");
        assert!(error.to_string().contains("identity is invalid"));
    }

    fn wait_for<T>(mut condition: impl FnMut() -> Option<T>) -> T {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = condition() {
                return value;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for Iroh test state"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

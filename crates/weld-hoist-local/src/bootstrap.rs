use std::os::fd::OwnedFd;

use anyhow::Result;
use weld_hoist_protocol::ProtocolRevision;

use crate::{
    LocalBootstrapAcknowledgement, LocalBootstrapOffer, LocalPacketConnection, LocalPeerRole,
    LocalSurfaceMode, TransportError,
};

pub struct LocalTransportConnections {
    pub mode: LocalSurfaceMode,
    pub control: LocalPacketConnection,
    pub media: Option<LocalPacketConnection>,
}

pub fn bootstrap_source(
    control: LocalPacketConnection,
    mode: LocalSurfaceMode,
) -> Result<LocalTransportConnections, TransportError> {
    let (media, descriptors) = match mode {
        LocalSurfaceMode::Native => (None, Vec::new()),
        LocalSurfaceMode::EncodedOpaque(_) => {
            let (source, destination) = LocalPacketConnection::source_handoff_pair()?;
            (Some(source), vec![destination])
        }
    };
    control.queue(
        &LocalBootstrapOffer {
            revision: ProtocolRevision::CURRENT,
            mode,
            media_descriptor: media.as_ref().map(|_| 0),
        },
        descriptors,
    )?;
    control.flush_blocking()?;
    let acknowledgement = control.receive_blocking::<LocalBootstrapAcknowledgement>()?;
    if !acknowledgement.file_descriptors.is_empty() {
        return Err(TransportError::Protocol(
            "bootstrap acknowledgement attached unexpected descriptors".to_owned(),
        ));
    }
    ProtocolRevision::CURRENT
        .ensure_compatible(acknowledgement.message.revision)
        .map_err(|error| TransportError::Protocol(error.to_string()))?;
    if let Some(rejection) = acknowledgement.message.rejection {
        return Err(TransportError::Protocol(format!(
            "destination rejected {mode:?}: {rejection}"
        )));
    }
    Ok(LocalTransportConnections {
        mode,
        control,
        media,
    })
}

pub fn bootstrap_destination(
    control: LocalPacketConnection,
    validate: impl FnOnce(LocalSurfaceMode) -> Result<()>,
) -> Result<LocalTransportConnections, TransportError> {
    let offer = control.receive_blocking::<LocalBootstrapOffer>()?;
    let mode = offer.message.mode;
    let result = match ProtocolRevision::CURRENT.ensure_compatible(offer.message.revision) {
        Err(error) => Err(error.to_string()),
        Ok(()) => match validate(mode) {
            Ok(()) => receive_media_connection(
                mode,
                offer.message.media_descriptor,
                offer.file_descriptors,
            )
            .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        },
    };
    control.queue(
        &LocalBootstrapAcknowledgement {
            revision: ProtocolRevision::CURRENT,
            rejection: result.as_ref().err().cloned(),
        },
        Vec::new(),
    )?;
    control.flush_blocking()?;
    let media = result.map_err(TransportError::Protocol)?;
    Ok(LocalTransportConnections {
        mode,
        control,
        media,
    })
}

fn receive_media_connection(
    mode: LocalSurfaceMode,
    descriptor_index: Option<u16>,
    mut descriptors: Vec<OwnedFd>,
) -> Result<Option<LocalPacketConnection>, TransportError> {
    match mode {
        LocalSurfaceMode::Native => {
            if descriptor_index.is_some() || !descriptors.is_empty() {
                return Err(TransportError::Protocol(
                    "native bootstrap attached a media channel".to_owned(),
                ));
            }
            Ok(None)
        }
        LocalSurfaceMode::EncodedOpaque(_) => {
            let index = descriptor_index.ok_or_else(|| {
                TransportError::Protocol("encoded bootstrap omitted its media channel".to_owned())
            })?;
            if descriptors.len() != 1 || index != 0 {
                return Err(TransportError::Protocol(
                    "encoded bootstrap must attach exactly one media channel".to_owned(),
                ));
            }
            let descriptor = descriptors.remove(0);
            LocalPacketConnection::from_received_fd(descriptor, LocalPeerRole::Destination)
                .map(Some)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_bootstrap_hands_off_an_independent_media_connection() {
        let (source_control, destination_control) =
            LocalPacketConnection::pair().expect("control pair");
        let source = std::thread::spawn(move || {
            bootstrap_source(
                source_control,
                LocalSurfaceMode::EncodedOpaque(weld_media::VideoCodec::H264),
            )
            .expect("source bootstrap")
        });
        let destination =
            bootstrap_destination(destination_control, |_| Ok(())).expect("destination bootstrap");
        let source = source.join().expect("source thread");
        let source_media = source.media.expect("source media channel");
        let destination_media = destination.media.expect("destination media channel");

        source_media
            .queue(&17_u64, Vec::new())
            .expect("media record");
        source_media.pump().expect("send media record");
        let packets = destination_media
            .drain::<u64>()
            .expect("receive media record");

        assert_eq!(packets[0].message, 17);
    }

    #[test]
    fn destination_rejection_reaches_the_source_with_its_reason() {
        let (source_control, destination_control) =
            LocalPacketConnection::pair().expect("control pair");
        let source = std::thread::spawn(move || {
            bootstrap_source(
                source_control,
                LocalSurfaceMode::EncodedOpaque(weld_media::VideoCodec::Av1),
            )
            .err()
            .expect("source must observe rejection")
            .to_string()
        });
        let destination = bootstrap_destination(destination_control, |_| {
            anyhow::bail!("no hardware decoder")
        })
        .err()
        .expect("destination must reject");
        let source = source.join().expect("source thread");

        assert!(source.contains("no hardware decoder"));
        assert!(destination.to_string().contains("no hardware decoder"));
    }

    #[test]
    fn destination_rejects_a_different_exact_protocol_revision() {
        let (source, destination) = LocalPacketConnection::pair().expect("control pair");
        source
            .queue(
                &LocalBootstrapOffer {
                    revision: ProtocolRevision::new(2),
                    mode: LocalSurfaceMode::Native,
                    media_descriptor: None,
                },
                Vec::new(),
            )
            .expect("bootstrap offer");
        source.flush_blocking().expect("bootstrap offer send");

        let rejection = match bootstrap_destination(destination, |_| Ok(())) {
            Ok(_) => panic!("destination accepted a mismatched revision"),
            Err(error) => error,
        };
        let acknowledgement = source
            .receive_blocking::<LocalBootstrapAcknowledgement>()
            .expect("bootstrap acknowledgement");

        assert!(rejection.to_string().contains("revision 1"));
        assert!(rejection.to_string().contains("revision 2"));
        assert!(
            acknowledgement
                .message
                .rejection
                .is_some_and(|reason| reason.contains("revision 2"))
        );
    }
}

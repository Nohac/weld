//! Admission checks shared by paired sessions and explicit source connections.

use anyhow::{Result, ensure};
use weld_hoist_protocol::{
    ProtocolRevision, SurfaceMode,
    bootstrap::{BootstrapAnswer, BootstrapOffer, PeerRole},
    presentation::{ReceiverPreferences, SourcePresentation},
};
use weld_media::VideoCodec;

/// Resolve the source's chosen codec against receiver capabilities before attaching media.
pub fn presentation(
    preferences: &ReceiverPreferences,
    codec: VideoCodec,
) -> Result<SourcePresentation> {
    ensure!(
        preferences.codecs.contains(&codec),
        "source selected an unsupported codec"
    );
    Ok(SourcePresentation {
        codec,
        stream_mode: preferences.stream_mode,
    })
}

pub fn encoded_offer(
    offer: &BootstrapOffer,
    preferences: &ReceiverPreferences,
) -> Result<SourcePresentation> {
    ProtocolRevision::CURRENT.ensure_compatible(offer.revision)?;
    ensure!(
        offer.role == PeerRole::Source,
        "bootstrap peer has the wrong role"
    );
    let SurfaceMode::EncodedOpaque(codec) = offer.mode else {
        anyhow::bail!("source selected an unsupported surface mode");
    };
    presentation(preferences, codec)
}

pub fn answer(answer: &BootstrapAnswer) -> Result<()> {
    ensure!(
        answer.role == PeerRole::Destination,
        "bootstrap peer returned the wrong role"
    );
    acknowledgement(answer.revision, answer.rejection.as_deref())
}

/// Verify the exact revision and propagate the receiver's admission failure.
pub fn acknowledgement(revision: ProtocolRevision, rejection: Option<&str>) -> Result<()> {
    ProtocolRevision::CURRENT.ensure_compatible(revision)?;
    if let Some(reason) = rejection {
        anyhow::bail!("destination rejected the session: {reason}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_client::SurfaceStreamMode;

    #[test]
    fn negotiated_presentation_preserves_composition_and_rejects_unsupported_sources() {
        let preferences = ReceiverPreferences {
            codecs: vec![VideoCodec::H264],
            stream_mode: SurfaceStreamMode::Composited,
        };
        let mut offer = BootstrapOffer {
            revision: ProtocolRevision::CURRENT,
            role: PeerRole::Source,
            mode: SurfaceMode::EncodedOpaque(VideoCodec::H264),
        };
        let selected = encoded_offer(&offer, &preferences).expect("compatible offer");
        assert_eq!(selected.codec, VideoCodec::H264);
        assert_eq!(selected.stream_mode, SurfaceStreamMode::Composited);
        offer.mode = SurfaceMode::EncodedOpaque(VideoCodec::Av1);
        assert!(encoded_offer(&offer, &preferences).is_err());
        offer.mode = SurfaceMode::Native;
        assert!(encoded_offer(&offer, &preferences).is_err());
        offer.mode = SurfaceMode::EncodedOpaque(VideoCodec::H264);
        offer.role = PeerRole::Destination;
        assert!(encoded_offer(&offer, &preferences).is_err());
        offer.role = PeerRole::Source;
        offer.revision = ProtocolRevision::new(ProtocolRevision::CURRENT.raw() + 1);
        assert!(encoded_offer(&offer, &preferences).is_err());
    }
}

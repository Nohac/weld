use std::{
    fs::File,
    io::{Read, Seek},
    os::fd::{AsFd, OwnedFd},
};

use anyhow::{Context, Result, ensure};
use smithay::utils::SealedFile;
use weld_media::EncodedAccessUnit;

use crate::{LocalEncodedAccessUnit, ensure_descriptors_consumed};

const MAX_ENCODED_ACCESS_UNIT_BYTES: usize = 32 * 1024 * 1024;

pub(crate) fn export_access_unit(
    access_unit: EncodedAccessUnit,
) -> Result<(LocalEncodedAccessUnit, Vec<OwnedFd>)> {
    ensure!(
        !access_unit.payload.is_empty()
            && access_unit.payload.len() <= MAX_ENCODED_ACCESS_UNIT_BYTES,
        "encoded access unit has an invalid length"
    );
    let payload_bytes = u32::try_from(access_unit.payload.len())
        .context("encoded access unit length exceeds wire range")?;
    let sealed = SealedFile::with_data(c"weld-encoded-frame", &access_unit.payload)
        .context("failed to seal encoded access unit")?;
    let descriptor = sealed
        .as_fd()
        .try_clone_to_owned()
        .context("failed to duplicate encoded access unit descriptor")?;
    Ok((
        LocalEncodedAccessUnit {
            frame: access_unit.frame,
            codec: access_unit.codec,
            kind: access_unit.kind,
            timestamp_micros: access_unit.timestamp_micros,
            payload_descriptor: 0,
            payload_bytes,
        },
        vec![descriptor],
    ))
}

pub(crate) fn import_access_unit(
    access_unit: LocalEncodedAccessUnit,
    descriptors: Vec<OwnedFd>,
) -> Result<EncodedAccessUnit> {
    let mut descriptors = descriptors.into_iter().map(Some).collect::<Vec<_>>();
    let descriptor = descriptors
        .get_mut(usize::from(access_unit.payload_descriptor))
        .context("encoded payload descriptor index is out of bounds")?
        .take()
        .context("encoded payload descriptor index was reused")?;
    ensure_descriptors_consumed(&descriptors)?;
    let payload_bytes = usize::try_from(access_unit.payload_bytes)
        .context("encoded access unit length exceeds address space")?;
    ensure!(
        payload_bytes > 0 && payload_bytes <= MAX_ENCODED_ACCESS_UNIT_BYTES,
        "encoded access unit has an invalid length"
    );
    let mut file = File::from(descriptor);
    let required_seals = smithay::reexports::rustix::fs::SealFlags::SHRINK
        | smithay::reexports::rustix::fs::SealFlags::GROW
        | smithay::reexports::rustix::fs::SealFlags::WRITE;
    let seals = smithay::reexports::rustix::fs::fcntl_get_seals(&file)
        .context("failed to inspect encoded access unit seals")?;
    ensure!(
        seals.contains(required_seals),
        "encoded access unit descriptor is not sealed"
    );
    let actual = usize::try_from(file.metadata()?.len())
        .context("encoded access unit length exceeds address space")?;
    ensure!(
        actual == payload_bytes,
        "encoded access unit descriptor length differs from its record"
    );
    file.rewind()?;
    let mut payload = vec![0; payload_bytes];
    file.read_exact(&mut payload)?;
    Ok(EncodedAccessUnit {
        frame: access_unit.frame,
        codec: access_unit.codec,
        kind: access_unit.kind,
        timestamp_micros: access_unit.timestamp_micros,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use weld_media::{EncodedFrameKind, MediaFrameId, MediaStreamId, StreamGeneration, VideoCodec};

    use super::*;

    fn access_unit() -> EncodedAccessUnit {
        EncodedAccessUnit {
            frame: MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(2), 3),
            codec: VideoCodec::H264,
            kind: EncodedFrameKind::Keyframe,
            timestamp_micros: 4,
            payload: vec![5, 6, 7],
        }
    }

    #[test]
    fn sealed_access_unit_roundtrips_with_exact_length() {
        let expected = access_unit();
        let (record, descriptors) = export_access_unit(expected.clone()).expect("export");

        let imported = import_access_unit(record, descriptors).expect("import");

        assert_eq!(imported, expected);
    }

    #[test]
    fn access_unit_rejects_unsealed_and_mismatched_descriptors() {
        let (mut record, descriptors) = export_access_unit(access_unit()).expect("export");
        record.payload_bytes += 1;
        assert!(import_access_unit(record, descriptors).is_err());

        let record = LocalEncodedAccessUnit {
            frame: access_unit().frame,
            codec: VideoCodec::H264,
            kind: EncodedFrameKind::Keyframe,
            timestamp_micros: 4,
            payload_descriptor: 0,
            payload_bytes: 3,
        };
        let descriptor: OwnedFd = File::open("/dev/null").expect("descriptor").into();
        assert!(import_access_unit(record, vec![descriptor]).is_err());
    }
}

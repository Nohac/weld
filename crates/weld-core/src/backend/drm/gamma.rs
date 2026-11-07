//! Hardware color-only updates, serialized by the host with output page flips.
//! Smithay owns mode/plane commits and leaves these gamma properties untouched.
use crate::gamma::{GammaRamp, MAX_GAMMA_ENTRIES};
use anyhow::{Context, Result, ensure};
use smithay::{
    backend::drm::DrmDeviceFd,
    reexports::drm::control::{AtomicCommitFlags, Device, atomic::AtomicModeReq, crtc, property},
};
use std::os::fd::AsFd;

pub(super) struct GammaDevice {
    fd: DrmDeviceFd,
    crtc: crtc::Handle,
    size: u32,
    baseline: Baseline,
}
enum Baseline {
    Atomic {
        property: property::Handle,
        bytes: Option<Vec<u8>>,
    },
    Legacy(GammaRamp),
}
impl GammaDevice {
    /// Snapshot the original table so controller exit and compositor shutdown
    /// restore the pre-control appearance, including an existing non-linear LUT.
    pub fn new(fd: DrmDeviceFd, crtc: crtc::Handle, legacy: bool) -> Result<Option<Self>> {
        if !legacy {
            let mut gamma = None;
            let mut size = 0;
            for (handle, value) in fd.get_properties(crtc)? {
                let info = fd.get_property(handle)?;
                match info.name().to_str() {
                    Ok("GAMMA_LUT") => gamma = Some((handle, value)),
                    Ok("GAMMA_LUT_SIZE") => size = u32::try_from(value).unwrap_or(0),
                    _ => {}
                }
            }
            if let Some((property, value)) = gamma {
                if !(1..=MAX_GAMMA_ENTRIES).contains(&size) {
                    return Ok(None);
                }
                let bytes = if value == 0 {
                    None
                } else {
                    Some(fd.get_property_blob(value)?)
                };
                return Ok(Some(Self {
                    fd,
                    crtc,
                    size,
                    baseline: Baseline::Atomic { property, bytes },
                }));
            }
        }
        // Legacy gamma ioctls are also exposed by some atomic drivers without
        // GAMMA_LUT properties. Preserve the exact original RGB arrays here too.
        let size = fd.get_crtc(crtc)?.gamma_length();
        if !(1..=MAX_GAMMA_ENTRIES).contains(&size) {
            return Ok(None);
        }
        let mut ramp = GammaRamp {
            red: vec![0; size as usize],
            green: vec![0; size as usize],
            blue: vec![0; size as usize],
        };
        fd.get_gamma(crtc, &mut ramp.red, &mut ramp.green, &mut ramp.blue)?;
        Ok(Some(Self {
            fd,
            crtc,
            size,
            baseline: Baseline::Legacy(ramp),
        }))
    }
    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn apply(&self, ramp: Option<&GammaRamp>) -> Result<()> {
        if let Some(ramp) = ramp {
            ensure!(
                ramp.len() == self.size as usize
                    && ramp.green.len() == ramp.len()
                    && ramp.blue.len() == ramp.len(),
                "gamma size changed after admission"
            );
        }
        match &self.baseline {
            Baseline::Atomic { property, bytes } => {
                let encoded = ramp.map(GammaRamp::drm_bytes);
                let data = encoded.as_deref().or_else(|| {
                    if ramp.is_none() {
                        bytes.as_deref()
                    } else {
                        None
                    }
                });
                let blob = data.map(|data| Blob::new(&self.fd, data)).transpose()?;
                let mut request = AtomicModeReq::new();
                request.add_property(
                    self.crtc,
                    *property,
                    property::Value::Blob(blob.as_ref().map_or(0, |blob| blob.id)),
                );
                self.fd
                    .atomic_commit(
                        AtomicCommitFlags::TEST_ONLY | AtomicCommitFlags::ALLOW_MODESET,
                        request.clone(),
                    )
                    .context("testing output gamma LUT")?;
                // Blocking, no page-flip event: completion cannot consume a frame's
                // callback or race the next Smithay commit on this host thread.
                self.fd
                    .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, request)
                    .context("applying output gamma LUT")?;
            }
            Baseline::Legacy(original) => {
                let ramp = ramp.unwrap_or(original);
                self.fd
                    .set_gamma(self.crtc, &ramp.red, &ramp.green, &ramp.blue)
                    .context("applying legacy output gamma")?;
            }
        }
        Ok(())
    }
}

/// The kernel retains its own reference after a successful property commit.
/// Our blob reference is released on both success and test/commit failure.
struct Blob<'a> {
    fd: &'a DrmDeviceFd,
    id: u64,
}
impl<'a> Blob<'a> {
    fn new(fd: &'a DrmDeviceFd, bytes: &[u8]) -> Result<Self> {
        let mut bytes = bytes.to_vec();
        let id = u64::from(drm_ffi::mode::create_property_blob(fd.as_fd(), &mut bytes)?.blob_id);
        Ok(Self { fd, id })
    }
}
impl Drop for Blob<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.fd.destroy_property_blob(self.id) {
            tracing::warn!(%error, "could not release gamma property blob");
        }
    }
}

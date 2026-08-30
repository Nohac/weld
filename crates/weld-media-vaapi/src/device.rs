use std::{num::NonZeroU16, path::Path, rc::Rc};

use anyhow::{Context, Result};
use cros_codecs::libva::Display;

use crate::{H264Decoder, H264Encoder, VppConverter};

/// One VA-API display shared by codec and video-processing sessions.
pub struct VaapiDevice {
    display: Rc<Display>,
}

impl VaapiDevice {
    pub fn open(render_node: impl AsRef<Path>) -> Result<Self> {
        let display = Display::open_drm_display(render_node.as_ref()).with_context(|| {
            format!(
                "could not open VA-API display {}",
                render_node.as_ref().display()
            )
        })?;
        Ok(Self { display })
    }

    pub fn vpp_converter(&self) -> Result<VppConverter> {
        VppConverter::new(self.display.clone())
    }

    pub fn h264_encoder(
        &self,
        width: u32,
        height: u32,
        bitrate: u64,
        frames_per_second: u32,
        intra_period: NonZeroU16,
    ) -> Result<H264Encoder> {
        H264Encoder::new(
            self.display.clone(),
            width,
            height,
            bitrate,
            frames_per_second,
            intra_period,
        )
    }

    pub fn h264_decoder(&self) -> Result<H264Decoder> {
        H264Decoder::new(self.display.clone())
    }

    #[cfg(feature = "diagnostic")]
    pub fn create_xrgb_probe_frame(
        &self,
        width: u32,
        height: u32,
        modifiers: Vec<u64>,
        seed: u8,
    ) -> Result<crate::VaapiDmabuf> {
        crate::vpp::create_xrgb_probe_frame(self.display.clone(), width, height, modifiers, seed)
    }
}

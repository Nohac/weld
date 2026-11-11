//! Narrow VA video processing and diagnostic image access; no codec implementation.
use crate::{
    VaapiDmabuf,
    device::{Config, Display, check, cleanup},
    ffi,
    resources::{Image, Surface, SurfaceRole},
};
use anyhow::{Result, ensure};
use std::{
    ptr,
    rc::Rc,
    time::{Duration, Instant},
};

pub struct Conversion {
    pub buffer: VaapiDmabuf,
    pub timing: ConversionTiming,
}
pub struct ConversionTiming {
    pub setup: Duration,
    pub sync: Duration,
}
/// Thread-confined processing configuration. Surfaces and contexts are scoped to
/// each operation; only exported owned DMA-BUF descriptors cross the boundary.
pub struct VppConverter {
    config: Config,
}
impl VppConverter {
    pub(crate) fn new(display: Rc<Display>) -> Result<Self> {
        Ok(Self {
            config: Config::new(
                display,
                ffi::VAProfile::VAProfileNone,
                ffi::VAEntrypoint::VAEntrypointVideoProc,
            )?,
        })
    }
    /// Convert the visible top-left region into XRGB with one of the supplied modifiers.
    pub fn convert(
        &self,
        input: &VaapiDmabuf,
        width: u32,
        height: u32,
        modifiers: &[u64],
    ) -> Result<Conversion> {
        ensure!(
            width > 0 && height > 0 && width <= input.width && height <= input.height,
            "VPP region is outside its input"
        );
        let region = ffi::VARectangle {
            x: 0,
            y: 0,
            width: u16::try_from(width)?,
            height: u16::try_from(height)?,
        };
        let started = Instant::now();
        let input = Surface::import(self.config.display.clone(), input)?;
        let output = Surface::xrgb(
            self.config.display.clone(),
            width,
            height,
            modifiers,
            SurfaceRole::Output,
        )?;
        let context = Context::new(&self.config, &output)?;
        let mut parameters = ffi::VAProcPipelineParameterBuffer {
            surface: input.id,
            surface_region: &region,
            surface_color_standard: ffi::_VAProcColorStandardType_VAProcColorStandardBT709,
            output_region: &region,
            output_background_color: 0xff00_0000,
            output_color_standard: ffi::_VAProcColorStandardType_VAProcColorStandardSRGB,
            ..Default::default()
        };
        let mut buffer = Parameters::new(&context, &mut parameters)?;
        // SAFETY: live context, its declared output surface, and a matching parameter buffer.
        check(unsafe { ffi::vaBeginPicture(self.config.display.handle, context.id, output.id) })?;
        // SAFETY: the unique buffer belongs to this context and its backing input/regions are live.
        check(unsafe {
            ffi::vaRenderPicture(self.config.display.handle, context.id, &mut buffer.id, 1)
        })?;
        // SAFETY: matching successful begin/render on this live context.
        check(unsafe { ffi::vaEndPicture(self.config.display.handle, context.id) })?;
        let sync_started = Instant::now();
        let buffer = output.export()?;
        ensure!(
            buffer
                .objects
                .iter()
                .all(|object| modifiers.contains(&object.modifier)),
            "VA-API ignored the requested output modifiers"
        );
        Ok(Conversion {
            buffer,
            timing: ConversionTiming {
                setup: sync_started.duration_since(started),
                sync: sync_started.elapsed(),
            },
        })
    }
    /// Reference upload for diagnostics; production CPU uploads belong to FFmpeg.
    pub fn upload_bgra(&self, width: u32, height: u32, pixels: &[u8]) -> Result<VaapiDmabuf> {
        let expected = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|n| n.checked_mul(4));
        ensure!(
            width > 0 && height > 0 && expected == Some(u64::try_from(pixels.len())?),
            "invalid BGRA extent or length"
        );
        let surface = Surface::xrgb(
            self.config.display.clone(),
            width,
            height,
            &[0],
            SurfaceRole::Input,
        )?;
        Image::new(&surface)?.write(pixels)?;
        surface.export()
    }
    /// Read a visible region as tightly packed BGRA for snapshots/validation.
    pub fn read_bgra(&self, input: &VaapiDmabuf, width: u32, height: u32) -> Result<Vec<u8>> {
        let converted = self.convert(input, width, height, &[0])?;
        let surface = Surface::import(self.config.display.clone(), &converted.buffer)?;
        surface.sync()?;
        Image::new(&surface)?.pixels()
    }
}
struct Context<'a> {
    config: &'a Config,
    _output: &'a Surface,
    id: ffi::VAContextID,
}
impl<'a> Context<'a> {
    fn new(config: &'a Config, output: &'a Surface) -> Result<Self> {
        let mut id = 0;
        let mut target = output.id;
        // SAFETY: config and output share a retained display and outlive this context.
        check(unsafe {
            ffi::vaCreateContext(
                config.display.handle,
                config.id,
                i32::try_from(output.width)?,
                i32::try_from(output.height)?,
                ffi::VA_PROGRESSIVE as i32,
                &mut target,
                1,
                &mut id,
            )
        })?;
        Ok(Self {
            config,
            _output: output,
            id,
        })
    }
}
impl Drop for Context<'_> {
    fn drop(&mut self) {
        // SAFETY: this guard uniquely owns the context, and its config/output remain live.
        cleanup(
            unsafe { ffi::vaDestroyContext(self.config.display.handle, self.id) },
            "context",
        );
    }
}
struct Parameters<'a, 'b> {
    context: &'a Context<'b>,
    id: ffi::VABufferID,
}
impl<'a, 'b> Parameters<'a, 'b> {
    fn new(
        context: &'a Context<'b>,
        parameters: &mut ffi::VAProcPipelineParameterBuffer,
    ) -> Result<Self> {
        let mut id = 0;
        // SAFETY: parameters is the header-generated ABI type; its pointees remain
        // alive through submission/completion in convert(). libva copies the record.
        check(unsafe {
            ffi::vaCreateBuffer(
                context.config.display.handle,
                context.id,
                ffi::VABufferType::VAProcPipelineParameterBufferType,
                u32::try_from(std::mem::size_of_val(parameters))?,
                1,
                ptr::from_mut(parameters).cast(),
                &mut id,
            )
        })?;
        Ok(Self { context, id })
    }
}
impl Drop for Parameters<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: uniquely owned buffer, before its borrowed context is destroyed.
        cleanup(
            unsafe { ffi::vaDestroyBuffer(self.context.config.display.handle, self.id) },
            "parameters",
        );
    }
}

#[cfg(test)]
mod hardware_tests {
    use super::*;
    use crate::VaapiDevice;
    use anyhow::Context as _;
    #[test]
    #[ignore = "requires WELD_TEST_RENDER_NODE; offscreen VA-API image access"]
    fn uploaded_pixels_survive_surface_and_converter_retirement() -> Result<()> {
        let path = std::env::var_os("WELD_TEST_RENDER_NODE").context("select render node")?;
        let device = VaapiDevice::open(path)?;
        let converter = device.vpp_converter()?;
        let (width, height) = (98, 74);
        let pixels: Vec<u8> = (0..width * height)
            .flat_map(|index| [(index % 251) as u8, 64, 192, 255])
            .collect();
        let exported = converter.upload_bgra(width, height, &pixels)?;
        drop(converter);
        let imported = Surface::import(device.0.clone(), &exported)?;
        imported.sync()?;
        let actual = Image::new(&imported)?.pixels()?;
        assert_eq!(actual.len(), pixels.len());
        for (a, b) in actual
            .as_chunks::<4>()
            .0
            .iter()
            .zip(pixels.as_chunks::<4>().0)
        {
            assert_eq!(a[..3], b[..3]);
        }
        Ok(())
    }
}

use std::{
    any::Any,
    ffi::{CStr, c_void},
    ptr,
    rc::Rc,
};

use anyhow::{Context, Result, ensure};
use cros_codecs::libva::{
    _VAProcColorStandardType_VAProcColorStandardBT709 as VA_COLOR_BT709,
    _VAProcColorStandardType_VAProcColorStandardSRGB as VA_COLOR_SRGB, Config, Display, Surface,
    SurfaceMemoryDescriptor, UsageHint, VA_FOURCC_BGRX, VA_FOURCC_NV12, VA_RT_FORMAT_RGB32,
    VA_RT_FORMAT_YUV420, VA_STATUS_SUCCESS, VA_SURFACE_ATTRIB_SETTABLE, VABufferID, VABufferType,
    VADRMFormatModifierList, VAEntrypoint, VAGenericValue, VAProcPipelineParameterBuffer,
    VAProfile, VARectangle, VAStatus, VASurfaceAttrib, VASurfaceAttribType, vaBeginPicture,
    vaCreateBuffer, vaDestroyBuffer, vaEndPicture, vaErrorStr, vaRenderPicture,
};

use crate::dmabuf::VaapiDmabuf;

const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");

pub enum VppOutput {
    Nv12,
    Xrgb8888 { modifiers: Vec<u64> },
}

/// One VA-API video-processing context bound to Weld's selected render node.
pub struct VppConverter {
    display: Rc<Display>,
    config: Config,
}

impl VppConverter {
    pub(crate) fn new(display: Rc<Display>) -> Result<Self> {
        let config = display
            .create_config(
                Vec::new(),
                VAProfile::VAProfileNone,
                VAEntrypoint::VAEntrypointVideoProc,
            )
            .context("could not create VA-API video-processing config")?;
        Ok(Self { display, config })
    }

    pub fn convert(&self, input: &VaapiDmabuf, output: VppOutput) -> Result<VaapiDmabuf> {
        let (input_rt_format, input_va_format) = surface_format(input.fourcc)
            .context("VPP input format is not supported by the hardware tracer")?;
        let input_surface = self
            .display
            .create_surfaces(
                input_rt_format,
                Some(input_va_format),
                input.width,
                input.height,
                Some(UsageHint::USAGE_HINT_VPP_READ),
                vec![input.import_descriptor()?],
            )
            .context("could not import VPP input DMA-BUF")?
            .pop()
            .context("VA-API did not create a VPP input surface")?;

        match output {
            VppOutput::Nv12 => {
                let output_surface = self
                    .display
                    .create_surfaces(
                        VA_RT_FORMAT_YUV420,
                        Some(VA_FOURCC_NV12),
                        input.width,
                        input.height,
                        Some(UsageHint::USAGE_HINT_VPP_WRITE | UsageHint::USAGE_HINT_EXPORT),
                        vec![ModifierAllocation { modifiers: vec![0] }],
                    )
                    .context("could not allocate VPP NV12 output")?;
                self.process(input_surface, output_surface, VA_COLOR_SRGB, VA_COLOR_BT709)
            }
            VppOutput::Xrgb8888 { modifiers } => {
                ensure!(
                    !modifiers.is_empty(),
                    "no XRGB output modifiers were supplied"
                );
                let output_surface = self
                    .display
                    .create_surfaces(
                        VA_RT_FORMAT_RGB32,
                        Some(VA_FOURCC_BGRX),
                        input.width,
                        input.height,
                        Some(UsageHint::USAGE_HINT_VPP_WRITE | UsageHint::USAGE_HINT_EXPORT),
                        vec![ModifierAllocation { modifiers }],
                    )
                    .context("could not allocate modifier-constrained VPP XRGB output")?;
                self.process(input_surface, output_surface, VA_COLOR_BT709, VA_COLOR_SRGB)
            }
        }
    }

    fn process<I, O>(
        &self,
        input: Surface<I>,
        output: Vec<Surface<O>>,
        input_color_standard: u32,
        output_color_standard: u32,
    ) -> Result<VaapiDmabuf>
    where
        I: SurfaceMemoryDescriptor,
        O: SurfaceMemoryDescriptor,
    {
        let output_surface = output
            .first()
            .context("VA-API did not create a VPP output surface")?;
        let context = self
            .display
            .create_context(
                &self.config,
                output_surface.size().0,
                output_surface.size().1,
                Some(&output),
                true,
            )
            .context("could not create VA-API VPP context")?;
        let region = VARectangle {
            x: 0,
            y: 0,
            width: u16::try_from(output_surface.size().0)
                .context("VPP width exceeds VA rectangle range")?,
            height: u16::try_from(output_surface.size().1)
                .context("VPP height exceeds VA rectangle range")?,
        };
        let mut parameters = VAProcPipelineParameterBuffer {
            surface: input.id(),
            surface_region: &region,
            surface_color_standard: input_color_standard,
            output_region: &region,
            output_background_color: 0xff00_0000,
            output_color_standard,
            ..Default::default()
        };
        let mut buffer_id: VABufferID = 0;
        // SAFETY: the display and context are live, `parameters` is the exact
        // libva-generated ABI type, and the output ID remains valid until the
        // buffer guard is dropped before the context.
        check_status(unsafe {
            vaCreateBuffer(
                self.display.as_raw(),
                context.as_raw(),
                VABufferType::VAProcPipelineParameterBufferType,
                u32::try_from(std::mem::size_of::<VAProcPipelineParameterBuffer>())
                    .context("VPP parameter size overflow")?,
                1,
                &mut parameters as *mut _ as *mut c_void,
                &mut buffer_id,
            )
        })
        .context("could not create VPP pipeline buffer")?;
        let mut buffer = VppBuffer {
            display: self.display.as_raw(),
            id: buffer_id,
        };
        // SAFETY: all handles belong to this live display/context, the output
        // surface is a render target of the context, and `buffer.id` names the
        // initialized pipeline parameter buffer above.
        check_status(unsafe {
            vaBeginPicture(self.display.as_raw(), context.as_raw(), output_surface.id())
        })
        .context("could not begin VPP picture")?;
        // SAFETY: `buffer.id` remains live and is submitted to its creating
        // context exactly once.
        check_status(unsafe {
            vaRenderPicture(self.display.as_raw(), context.as_raw(), &mut buffer.id, 1)
        })
        .context("could not render VPP picture")?;
        // SAFETY: the matching picture was begun above and all resources remain live.
        check_status(unsafe { vaEndPicture(self.display.as_raw(), context.as_raw()) })
            .context("could not end VPP picture")?;
        output_surface
            .sync()
            .context("could not synchronize VPP output")?;
        VaapiDmabuf::from_prime(
            output_surface
                .export_prime()
                .context("could not export VPP output DMA-BUF")?,
        )
    }
}

struct VppBuffer {
    display: cros_codecs::libva::VADisplay,
    id: VABufferID,
}

impl Drop for VppBuffer {
    fn drop(&mut self) {
        // SAFETY: the buffer ID was returned by `vaCreateBuffer` on this display
        // and the guard destroys it at most once.
        let _ = unsafe { vaDestroyBuffer(self.display, self.id) };
    }
}

struct ModifierAllocation {
    modifiers: Vec<u64>,
}

struct ModifierListBacking {
    list: VADRMFormatModifierList,
    _modifiers: Vec<u64>,
}

impl SurfaceMemoryDescriptor for ModifierAllocation {
    fn add_attrs(&mut self, attributes: &mut Vec<VASurfaceAttrib>) -> Option<Box<dyn Any>> {
        let modifiers = std::mem::take(&mut self.modifiers);
        let num_modifiers = u32::try_from(modifiers.len()).ok()?;
        if num_modifiers == 0 {
            return None;
        }
        let mut backing = Box::new(ModifierListBacking {
            list: VADRMFormatModifierList {
                num_modifiers,
                modifiers: ptr::null_mut(),
            },
            _modifiers: modifiers,
        });
        backing.list.modifiers = backing._modifiers.as_mut_ptr();
        attributes.push(VASurfaceAttrib {
            type_: VASurfaceAttribType::VASurfaceAttribDRMFormatModifiers,
            flags: VA_SURFACE_ATTRIB_SETTABLE,
            value: VAGenericValue::from(&mut backing.list as *mut _ as *mut c_void),
        });
        Some(backing)
    }
}

fn surface_format(fourcc: u32) -> Option<(u32, u32)> {
    match fourcc {
        DRM_FORMAT_XRGB8888 => Some((VA_RT_FORMAT_RGB32, VA_FOURCC_BGRX)),
        DRM_FORMAT_NV12 => Some((VA_RT_FORMAT_YUV420, VA_FOURCC_NV12)),
        _ => None,
    }
}

fn check_status(status: VAStatus) -> Result<()> {
    if status == VA_STATUS_SUCCESS as i32 {
        return Ok(());
    }
    // SAFETY: libva returns a static null-terminated description for every status.
    let description = unsafe { CStr::from_ptr(vaErrorStr(status)) }.to_string_lossy();
    anyhow::bail!("VA status {status}: {description}")
}

#[cfg(feature = "diagnostic")]
pub(crate) fn create_xrgb_probe_frame(
    display: Rc<Display>,
    width: u32,
    height: u32,
    modifiers: Vec<u64>,
    seed: u8,
) -> Result<VaapiDmabuf> {
    let mut surfaces = display
        .create_surfaces(
            VA_RT_FORMAT_RGB32,
            Some(VA_FOURCC_BGRX),
            width,
            height,
            Some(UsageHint::USAGE_HINT_VPP_READ | UsageHint::USAGE_HINT_EXPORT),
            vec![ModifierAllocation { modifiers }],
        )
        .context("could not allocate the diagnostic XRGB surface")?;
    let surface = surfaces
        .pop()
        .context("VA-API did not create the diagnostic XRGB surface")?;
    let image_format = display
        .query_image_formats()
        .context("could not query VA image formats")?
        .into_iter()
        .find(|format| format.fourcc == VA_FOURCC_BGRX)
        .context("VA-API does not expose a BGRX image format")?;
    let mut image = cros_codecs::libva::Image::create_from(
        &surface,
        image_format,
        (width, height),
        (width, height),
    )
    .context("could not map the diagnostic XRGB surface")?;
    let image_description = *image.image();
    let bytes = image.as_mut();
    let offset = usize::try_from(image_description.offsets[0])?;
    let pitch = usize::try_from(image_description.pitches[0])?;
    let width = usize::try_from(width)?;
    let height = usize::try_from(height)?;
    for y in 0..height {
        for x in 0..width {
            let pixel = offset + y * pitch + x * 4;
            let horizontal = (x * 63) / width.saturating_sub(1).max(1);
            let vertical = (y * 63) / height.saturating_sub(1).max(1);
            let value = u8::try_from(horizontal + vertical)?
                .checked_add(seed)
                .context("diagnostic gradient exceeds one byte")?;
            bytes[pixel..pixel + 4].copy_from_slice(&[value, value, value, 255]);
        }
    }
    drop(image);
    surface
        .sync()
        .context("could not synchronize diagnostic XRGB surface")?;
    VaapiDmabuf::from_prime(
        surface
            .export_prime()
            .context("could not export diagnostic XRGB DMA-BUF")?,
    )
}

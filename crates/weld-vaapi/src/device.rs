// Copyright 2022 The ChromiumOS Authors
// Adapted for Weld. See LICENSE and UPSTREAM.md.
use crate::{VppConverter, ffi};
use anyhow::{Context as _, Result, bail, ensure};
use std::{
    ffi::CStr,
    fs::{File, OpenOptions},
    os::fd::AsRawFd,
    path::Path,
    ptr,
    rc::Rc,
};

/// Profiles currently used by Weld's FFmpeg backend.
#[derive(Clone, Copy, Debug)]
pub enum Profile {
    H264ConstrainedBaseline,
    Av1Main,
}
impl Profile {
    pub(crate) fn native(self) -> ffi::VAProfile::Type {
        match self {
            Self::H264ConstrainedBaseline => ffi::VAProfile::VAProfileH264ConstrainedBaseline,
            Self::Av1Main => ffi::VAProfile::VAProfileAV1Profile0,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeEntrypoint {
    Slice,
    LowPowerSlice,
}
impl EncodeEntrypoint {
    pub(crate) fn native(self) -> ffi::VAEntrypoint::Type {
        match self {
            Self::Slice => ffi::VAEntrypoint::VAEntrypointEncSlice,
            Self::LowPowerSlice => ffi::VAEntrypoint::VAEntrypointEncSliceLP,
        }
    }
    pub const fn is_low_power(self) -> bool {
        matches!(self, Self::LowPowerSlice)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct ProfileCapabilities {
    pub decode: bool,
    pub encode: Option<EncodeEntrypoint>,
}
/// Driver-reported values, without codec-specific padding or fallback policy.
#[derive(Clone, Copy, Debug, Default)]
pub struct SurfaceLimits {
    pub minimum_width: Option<u32>,
    pub minimum_height: Option<u32>,
    pub maximum_width: Option<u32>,
    pub maximum_height: Option<u32>,
}

/// A thread-confined connection to one explicitly selected DRM render node.
#[derive(Clone)]
pub struct VaapiDevice(pub(crate) Rc<Display>);
pub(crate) struct Display {
    pub(crate) handle: ffi::VADisplay,
    _file: File,
}
impl VaapiDevice {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.as_ref())
            .with_context(|| format!("opening VA-API render node {}", path.as_ref().display()))?;
        // SAFETY: the owned file remains open until after vaTerminate.
        let handle = unsafe { ffi::vaGetDisplayDRM(file.as_raw_fd()) };
        ensure!(!handle.is_null(), "could not obtain a VA-API display");
        let (mut major, mut minor) = (0, 0);
        // SAFETY: handle comes from vaGetDisplayDRM and both outputs are writable.
        check(unsafe { ffi::vaInitialize(handle, &mut major, &mut minor) })?;
        Ok(Self(Rc::new(Display {
            handle,
            _file: file,
        })))
    }
    pub fn vendor(&self) -> Result<String> {
        // SAFETY: the initialized display is live for this call and string copy.
        let text = unsafe { ffi::vaQueryVendorString(self.0.handle) };
        ensure!(!text.is_null(), "VA-API vendor string is absent");
        // SAFETY: libva returns a display-owned null-terminated string.
        Ok(unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned())
    }
    pub fn profile(&self, profile: Profile) -> Result<ProfileCapabilities> {
        let entries = self.entrypoints(profile.native())?;
        Ok(ProfileCapabilities {
            decode: entries.contains(&ffi::VAEntrypoint::VAEntrypointVLD),
            encode: if entries.contains(&ffi::VAEntrypoint::VAEntrypointEncSlice) {
                Some(EncodeEntrypoint::Slice)
            } else if entries.contains(&ffi::VAEntrypoint::VAEntrypointEncSliceLP) {
                Some(EncodeEntrypoint::LowPowerSlice)
            } else {
                None
            },
        })
    }
    pub fn supports_processing(&self) -> Result<bool> {
        Ok(self
            .entrypoints(ffi::VAProfile::VAProfileNone)?
            .contains(&ffi::VAEntrypoint::VAEntrypointVideoProc))
    }
    pub fn encode_limits(
        &self,
        profile: Profile,
        entrypoint: EncodeEntrypoint,
    ) -> Result<SurfaceLimits> {
        let config = Config::new(self.0.clone(), profile.native(), entrypoint.native())?;
        let mut count = 0;
        // SAFETY: live configuration; a null array requests the required count.
        check(unsafe {
            ffi::vaQuerySurfaceAttributes(self.0.handle, config.id, ptr::null_mut(), &mut count)
        })?;
        let mut attributes = initialized(usize::try_from(count)?)?;
        // SAFETY: storage has count initialized writable elements. count is an in/out capacity.
        check(unsafe {
            ffi::vaQuerySurfaceAttributes(
                self.0.handle,
                config.id,
                attributes.as_mut_ptr(),
                &mut count,
            )
        })?;
        ensure!(
            usize::try_from(count)? <= attributes.len(),
            "VA-API returned too many surface attributes"
        );
        surface_limits(&attributes[..usize::try_from(count)?])
    }
    pub fn vpp_converter(&self) -> Result<VppConverter> {
        VppConverter::new(self.0.clone())
    }

    fn entrypoints(&self, profile: ffi::VAProfile::Type) -> Result<Vec<ffi::VAEntrypoint::Type>> {
        // SAFETY: live initialized display; this returns the array capacity required below.
        let mut count = unsafe { ffi::vaMaxNumEntrypoints(self.0.handle) };
        ensure!(count > 0, "VA-API entrypoint capacity is invalid");
        let mut entries = initialized(usize::try_from(count)?)?;
        // SAFETY: entries has the capacity required by libva and count is writable.
        let status = unsafe {
            ffi::vaQueryConfigEntrypoints(self.0.handle, profile, entries.as_mut_ptr(), &mut count)
        };
        if status as u32 == ffi::VA_STATUS_ERROR_UNSUPPORTED_PROFILE {
            return Ok(Vec::new());
        }
        check(status)?;
        let count = usize::try_from(count)?;
        ensure!(
            count <= entries.len(),
            "VA-API returned too many entrypoints"
        );
        entries.truncate(count);
        Ok(entries)
    }
}
impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: the last Rc owns this initialized display; all dependent objects are gone.
        cleanup(unsafe { ffi::vaTerminate(self.handle) }, "display");
    }
}
pub(crate) struct Config {
    pub(crate) display: Rc<Display>,
    pub(crate) id: ffi::VAConfigID,
}
impl Config {
    pub(crate) fn new(
        display: Rc<Display>,
        profile: ffi::VAProfile::Type,
        entrypoint: ffi::VAEntrypoint::Type,
    ) -> Result<Self> {
        let mut id = 0;
        // SAFETY: initialized display, no optional attributes, and writable output ID.
        check(unsafe {
            ffi::vaCreateConfig(
                display.handle,
                profile,
                entrypoint,
                ptr::null_mut(),
                0,
                &mut id,
            )
        })?;
        Ok(Self { display, id })
    }
}
impl Drop for Config {
    fn drop(&mut self) {
        // SAFETY: this guard uniquely owns the configuration on its retained display.
        cleanup(
            unsafe { ffi::vaDestroyConfig(self.display.handle, self.id) },
            "configuration",
        );
    }
}
pub(crate) fn initialized<T: Default + Clone>(count: usize) -> Result<Vec<T>> {
    let mut values = Vec::new();
    values.try_reserve_exact(count)?;
    values.resize(count, T::default());
    Ok(values)
}
pub(crate) fn check(status: ffi::VAStatus) -> Result<()> {
    if status as u32 == ffi::VA_STATUS_SUCCESS {
        return Ok(());
    }
    // SAFETY: vaErrorStr accepts every VAStatus and returns a static C string.
    let text = unsafe { ffi::vaErrorStr(status) };
    if text.is_null() {
        bail!("VA-API error {status}");
    }
    // SAFETY: the checked non-null pointer refers to a static null-terminated string.
    bail!(
        "VA-API error {status}: {}",
        unsafe { CStr::from_ptr(text) }.to_string_lossy()
    )
}
pub(crate) fn cleanup(status: ffi::VAStatus, resource: &str) {
    if let Err(error) = check(status) {
        tracing::warn!(%error, resource, "could not release VA-API resource");
    }
}

fn surface_limits(attributes: &[ffi::VASurfaceAttrib]) -> Result<SurfaceLimits> {
    let mut seen = 0u8;
    let mut limits = SurfaceLimits::default();
    for attribute in attributes {
        let (flag, target) = match attribute.type_ {
            ffi::VASurfaceAttribType::VASurfaceAttribMinWidth => (1, &mut limits.minimum_width),
            ffi::VASurfaceAttribType::VASurfaceAttribMinHeight => (2, &mut limits.minimum_height),
            ffi::VASurfaceAttribType::VASurfaceAttribMaxWidth => (4, &mut limits.maximum_width),
            ffi::VASurfaceAttribType::VASurfaceAttribMaxHeight => (8, &mut limits.maximum_height),
            _ => continue,
        };
        ensure!(
            attribute.value.type_ == ffi::VAGenericValueType::VAGenericValueTypeInteger,
            "VA-API dimension is not an integer"
        );
        // SAFETY: the discriminant above identifies the integer union member.
        let value = unsafe { attribute.value.value.i };
        ensure!(seen & flag == 0, "duplicate VA-API dimension attribute");
        seen |= flag;
        *target = u32::try_from(value).ok().filter(|value| *value > 0);
    }
    Ok(limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::integer;
    #[test]
    fn dimensions_require_integer_tags_and_unique_attributes_even_when_unknown() {
        let mut width = integer(ffi::VASurfaceAttribType::VASurfaceAttribMinWidth, 128);
        assert_eq!(
            surface_limits(&[width]).expect("width").minimum_width,
            Some(128)
        );
        width.value.value.i = 0;
        assert_eq!(
            surface_limits(&[width]).expect("unknown").minimum_width,
            None
        );
        assert!(surface_limits(&[width, width]).is_err());
        width.value.type_ = ffi::VAGenericValueType::VAGenericValueTypePointer;
        assert!(surface_limits(&[width]).is_err());
    }
    #[test]
    fn invalid_render_node_is_a_fallible_open() {
        assert!(VaapiDevice::open("/nonexistent/weld-vaapi-render-node").is_err());
    }
}

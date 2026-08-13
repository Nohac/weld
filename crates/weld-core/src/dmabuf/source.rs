//! Cached Vulkan imports for live Wayland DMA-BUF objects.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

use anyhow::{Context, Result, bail};
use ash::vk;
use smithay::backend::allocator::{Buffer, Fourcc, dmabuf::Dmabuf};

use super::ImportId;

pub(crate) struct ImportedDmabufSource {
    pub(crate) id: ImportId,
    pub(crate) alive: Cell<bool>,
    pub(crate) texture: wgpu::Texture,
    pub(crate) view: wgpu::TextureView,
    pub(crate) image: vk::Image,
    pub(crate) format: wgpu::TextureFormat,
}

#[derive(Clone)]
pub(crate) struct DmabufSourceCache {
    device: wgpu::Device,
    max_dimension: u32,
    imported: Rc<RefCell<HashMap<Dmabuf, Rc<ImportedDmabufSource>>>>,
    next_import_id: Rc<Cell<Option<u64>>>,
}

impl DmabufSourceCache {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        Self {
            device: device.clone(),
            max_dimension: device.limits().max_texture_dimension_2d,
            imported: Rc::new(RefCell::new(HashMap::new())),
            next_import_id: Rc::new(Cell::new(Some(1))),
        }
    }

    /// Validate and import a protocol buffer before acknowledging its creation.
    pub(crate) fn import(&self, dmabuf: &Dmabuf) -> Result<Rc<ImportedDmabufSource>> {
        if let Some(imported) = self.imported.borrow().get(dmabuf).cloned() {
            return Ok(imported);
        }
        if dmabuf.num_planes() != 1 {
            bail!("only single-plane DMA-BUFs are supported");
        }
        let size = dmabuf.size();
        let width = u32::try_from(size.w).context("negative DMA-BUF width")?;
        let height = u32::try_from(size.h).context("negative DMA-BUF height")?;
        if width == 0 || height == 0 {
            bail!("zero-sized DMA-BUF");
        }
        if width > self.max_dimension || height > self.max_dimension {
            bail!(
                "DMA-BUF {width}x{height} exceeds device limit {}",
                self.max_dimension
            );
        }
        let format = texture_format(dmabuf.format().code)
            .context("DMA-BUF format has no supported wgpu representation")?;
        let fd = dmabuf
            .handles()
            .next()
            .context("DMA-BUF has no plane")?
            .try_clone_to_owned()
            .context("failed to duplicate DMA-BUF plane")?;
        let stride = u64::from(dmabuf.strides().next().context("DMA-BUF has no stride")?);
        let offset = u64::from(dmabuf.offsets().next().context("DMA-BUF has no offset")?);
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let hal_descriptor = wgpu::hal::TextureDescriptor {
            label: Some("weld imported client DMA-BUF"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUses::RESOURCE,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: Vec::new(),
        };
        // SAFETY: linux-dmabuf protocol validation restricts this to the exact
        // single-plane format/modifier pairs advertised from this device. The
        // duplicated fd and supplied layout describe that plane; Vulkan
        // consumes only the duplicate, never Smithay's original fd.
        let hal_texture = unsafe {
            let raw = self
                .device
                .as_hal::<wgpu::hal::api::Vulkan>()
                .context("DMA-BUF device is not backed by Vulkan")?;
            raw.texture_from_dmabuf_fd(
                fd,
                &hal_descriptor,
                dmabuf.format().modifier.into(),
                stride,
                offset,
            )?
        };
        let descriptor = wgpu::TextureDescriptor {
            label: Some("weld imported client DMA-BUF"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        // SAFETY: `hal_texture` was created by this exact device from the
        // matching descriptor. The first foreign acquire establishes the
        // RESOURCE layout before any tracked wgpu access.
        let texture = unsafe {
            self.device
                .create_texture_from_hal::<wgpu::hal::api::Vulkan>(
                    hal_texture,
                    &descriptor,
                    wgpu::TextureUses::RESOURCE,
                )
        };
        // SAFETY: the HAL guard remains live while its opaque image handle is
        // copied. Cache ownership keeps the texture alive until wl_buffer
        // destruction, and tracked submissions retain in-flight uses.
        let image = unsafe {
            texture
                .as_hal::<wgpu::hal::api::Vulkan>()
                .context("imported texture is not backed by Vulkan")?
                .raw_handle()
        };
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let id = ImportId::new(
            self.next_import_id
                .get()
                .context("DMA-BUF import identity space is exhausted")?,
        );
        self.next_import_id.set(id.next());
        let imported = Rc::new(ImportedDmabufSource {
            id,
            alive: Cell::new(true),
            texture,
            view,
            image,
            format,
        });
        self.imported
            .borrow_mut()
            .insert(dmabuf.clone(), imported.clone());
        Ok(imported)
    }

    pub(crate) fn get(&self, dmabuf: &Dmabuf) -> Option<Rc<ImportedDmabufSource>> {
        self.imported.borrow().get(dmabuf).cloned()
    }

    pub(crate) fn remove(&self, dmabuf: &Dmabuf) {
        if let Some(imported) = self.imported.borrow_mut().remove(dmabuf) {
            imported.alive.set(false);
        }
    }
}

pub(crate) const fn texture_format(format: Fourcc) -> Option<wgpu::TextureFormat> {
    match format {
        Fourcc::Argb8888 | Fourcc::Xrgb8888 => Some(wgpu::TextureFormat::Bgra8Unorm),
        Fourcc::Abgr8888 | Fourcc::Xbgr8888 => Some(wgpu::TextureFormat::Rgba8Unorm),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_only_the_advertised_eight_bit_rgb_formats() {
        assert_eq!(
            texture_format(Fourcc::Argb8888),
            Some(wgpu::TextureFormat::Bgra8Unorm)
        );
        assert_eq!(
            texture_format(Fourcc::Xrgb8888),
            Some(wgpu::TextureFormat::Bgra8Unorm)
        );
        assert_eq!(
            texture_format(Fourcc::Abgr8888),
            Some(wgpu::TextureFormat::Rgba8Unorm)
        );
        assert_eq!(
            texture_format(Fourcc::Xbgr8888),
            Some(wgpu::TextureFormat::Rgba8Unorm)
        );
        assert_eq!(texture_format(Fourcc::Nv12), None);
    }
}

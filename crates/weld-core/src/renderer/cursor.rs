//! Immutable cursor uploads and physical cursor geometry for final presentation.

use std::{collections::HashMap, fmt, sync::Arc, time::Instant};

use tracing::warn;
use wgpu::util::DeviceExt;

use crate::{
    cursor::{
        ClientCursorImage, CursorConfiguration, CursorGeometry, CursorImage, ThemeCursorFrame,
        XcursorResolver, client_cursor_geometry, named_cursor_geometry,
    },
    input::InputPosition,
    surface::Extent,
};

pub(super) const CURSOR_UNIFORM_SIZE: usize = 48;

struct CursorTexture {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// Cursor state retained by a queued physical presentation.
#[derive(Clone)]
pub(crate) struct CursorOverlay {
    texture: Option<Arc<CursorTexture>>,
    uniform: [f32; 12],
}

impl CursorOverlay {
    pub(crate) const fn hidden() -> Self {
        Self {
            texture: None,
            uniform: [0.0; 12],
        }
    }

    pub(crate) fn texture_view(&self) -> Option<&wgpu::TextureView> {
        self.texture.as_ref().map(|texture| &texture.view)
    }

    pub(super) fn uniform_bytes(&self) -> [u8; CURSOR_UNIFORM_SIZE] {
        let mut bytes = [0; CURSOR_UNIFORM_SIZE];
        for (index, value) in self.uniform.into_iter().enumerate() {
            let offset = index * size_of::<f32>();
            bytes[offset..offset + size_of::<f32>()].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn scissor(&self, target_width: u32, target_height: u32) -> Option<CursorScissor> {
        self.texture.as_ref()?;
        let origin_x = self.uniform[0];
        let origin_y = self.uniform[1];
        let width = self.uniform[2];
        let height = self.uniform[3];
        if width <= 0.0 || height <= 0.0 {
            return None;
        }
        let left = origin_x.floor().max(0.0).min(target_width as f32) as u32;
        let top = origin_y.floor().max(0.0).min(target_height as f32) as u32;
        let right = (origin_x + width).ceil().max(0.0).min(target_width as f32) as u32;
        let bottom = (origin_y + height)
            .ceil()
            .max(0.0)
            .min(target_height as f32) as u32;
        (right > left && bottom > top).then_some(CursorScissor {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }

    fn visible(
        texture: Arc<CursorTexture>,
        texture_width: u32,
        texture_height: u32,
        geometry: CursorGeometry,
    ) -> Self {
        Self {
            texture: Some(texture),
            uniform: [
                geometry.origin_x,
                geometry.origin_y,
                geometry.width,
                geometry.height,
                geometry.source_x,
                geometry.source_y,
                geometry.source_width,
                geometry.source_height,
                texture_width as f32,
                texture_height as f32,
                1.0,
                0.0,
            ],
        }
    }
}

struct CursorScissor {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// Draws only the compositor cursor over a completed direct-scanout frame.
pub(crate) struct CursorOverlayRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    bind_group_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
}

impl CursorOverlayRenderer {
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("weld cursor overlay bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("weld cursor overlay pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("weld cursor overlay shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("cursor_overlay.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("weld cursor overlay pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("weld cursor overlay uniform"),
            contents: &CursorOverlay::hidden().uniform_bytes(),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
        });
        Self {
            device: device.clone(),
            queue: queue.clone(),
            bind_group_layout,
            pipeline,
            uniform,
        }
    }

    pub(crate) fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        target_extent: Extent,
        cursor: &CursorOverlay,
    ) {
        let Some(texture) = cursor.texture_view() else {
            return;
        };
        let Some(scissor) = cursor.scissor(target_extent.width, target_extent.height) else {
            return;
        };
        self.queue
            .write_buffer(&self.uniform, 0, &cursor.uniform_bytes());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("weld cursor overlay bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(texture),
                },
            ],
        });
        // Bevy's output pass clears and stores the complete target first. This
        // Load is therefore initialized even for a newly imported GBM image.
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("weld cursor overlay pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.set_scissor_rect(scissor.x, scissor.y, scissor.width, scissor.height);
        pass.draw(0..3, 0..1);
    }
}

impl Default for CursorOverlay {
    fn default() -> Self {
        Self::hidden()
    }
}

impl fmt::Debug for CursorOverlay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CursorOverlay")
            .field("visible", &self.texture.is_some())
            .field("uniform", &self.uniform)
            .finish()
    }
}

impl PartialEq for CursorOverlay {
    fn eq(&self, other: &Self) -> bool {
        let same_texture = match (&self.texture, &other.texture) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        };
        same_texture && self.uniform == other.uniform
    }
}

pub(crate) struct CursorEvaluation {
    pub(crate) overlay: CursorOverlay,
    pub(crate) plane: CursorPlaneSnapshot,
    pub(crate) next_animation: Option<Instant>,
}

/// Backend-neutral cursor pixels and physical geometry for a KMS cursor plane.
#[derive(Clone, Debug)]
pub(crate) struct CursorPlaneSnapshot {
    image: Option<CursorPlaneImage>,
    origin_x: i32,
    origin_y: i32,
}

impl CursorPlaneSnapshot {
    pub(crate) const fn hidden() -> Self {
        Self {
            image: None,
            origin_x: 0,
            origin_y: 0,
        }
    }

    pub(crate) const fn image(&self) -> Option<&CursorPlaneImage> {
        self.image.as_ref()
    }

    pub(crate) const fn origin(&self) -> (i32, i32) {
        (self.origin_x, self.origin_y)
    }

    fn visible(
        pixels: Arc<[u8]>,
        texture_width: u32,
        texture_height: u32,
        geometry: CursorGeometry,
    ) -> Option<Self> {
        let width = rounded_extent(geometry.width)?;
        let height = rounded_extent(geometry.height)?;
        Some(Self {
            image: Some(CursorPlaneImage::new(
                pixels,
                texture_width,
                texture_height,
                geometry,
                width,
                height,
            )),
            origin_x: geometry.origin_x.round() as i32,
            origin_y: geometry.origin_y.round() as i32,
        })
    }
}

impl Default for CursorPlaneSnapshot {
    fn default() -> Self {
        Self::hidden()
    }
}

impl PartialEq for CursorPlaneSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.origin_x == other.origin_x
            && self.origin_y == other.origin_y
            && match (&self.image, &other.image) {
                (Some(left), Some(right)) => left == right,
                (None, None) => true,
                _ => false,
            }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CursorPlaneImage {
    pixels: Arc<[u8]>,
    texture_width: u32,
    texture_height: u32,
    source_x: f32,
    source_y: f32,
    source_width: f32,
    source_height: f32,
    width: u32,
    height: u32,
}

impl CursorPlaneImage {
    pub(crate) fn new(
        pixels: Arc<[u8]>,
        texture_width: u32,
        texture_height: u32,
        geometry: CursorGeometry,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            pixels,
            texture_width,
            texture_height,
            source_x: geometry.source_x,
            source_y: geometry.source_y,
            source_width: geometry.source_width,
            source_height: geometry.source_height,
            width,
            height,
        }
    }

    pub(crate) fn pixels(&self) -> &Arc<[u8]> {
        &self.pixels
    }

    pub(crate) const fn texture_extent(&self) -> (u32, u32) {
        (self.texture_width, self.texture_height)
    }

    pub(crate) const fn source(&self) -> (f32, f32, f32, f32) {
        (
            self.source_x,
            self.source_y,
            self.source_width,
            self.source_height,
        )
    }

    pub(crate) const fn extent(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl PartialEq for CursorPlaneImage {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.pixels, &other.pixels)
            && self.texture_width == other.texture_width
            && self.texture_height == other.texture_height
            && self.source_x == other.source_x
            && self.source_y == other.source_y
            && self.source_width == other.source_width
            && self.source_height == other.source_height
            && self.width == other.width
            && self.height == other.height
    }
}

pub(crate) struct GpuCursor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    max_dimension: u32,
    configuration: CursorConfiguration,
    resolver: XcursorResolver,
    image: CursorImage,
    position: Option<InputPosition>,
    output_scale: f64,
    animation_started: Instant,
    theme_textures: HashMap<usize, Arc<CursorTexture>>,
    client_texture: Option<(usize, Arc<CursorTexture>)>,
}

impl GpuCursor {
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        configuration: CursorConfiguration,
        output_scale: f64,
        now: Instant,
    ) -> Self {
        Self {
            device: device.clone(),
            queue: queue.clone(),
            max_dimension: device.limits().max_texture_dimension_2d,
            resolver: XcursorResolver::new(&configuration),
            configuration,
            image: CursorImage::Named(crate::cursor::CursorIcon::Default),
            position: None,
            output_scale,
            animation_started: now,
            theme_textures: HashMap::new(),
            client_texture: None,
        }
    }

    pub(crate) fn set_configuration(&mut self, configuration: CursorConfiguration, now: Instant) {
        if self.configuration == configuration {
            return;
        }
        self.resolver = XcursorResolver::new(&configuration);
        self.configuration = configuration;
        self.animation_started = now;
        self.theme_textures.clear();
    }

    pub(crate) fn set_image(&mut self, image: CursorImage, now: Instant) {
        if self.image.same_image(&image) {
            return;
        }
        self.image = image;
        self.animation_started = now;
        self.client_texture = None;
    }

    pub(crate) fn set_position(&mut self, position: Option<InputPosition>) {
        self.position = position;
    }

    pub(crate) fn set_output_scale(&mut self, output_scale: f64) {
        self.output_scale = output_scale;
    }

    pub(crate) fn evaluate(&mut self, now: Instant) -> CursorEvaluation {
        let Some(position) = self.position else {
            return hidden_evaluation();
        };
        match self.image.clone() {
            CursorImage::Hidden => hidden_evaluation(),
            CursorImage::Named(icon) => self.evaluate_named(icon, position, now),
            CursorImage::Surface(image) => self.evaluate_client(&image, position),
        }
    }

    fn evaluate_named(
        &mut self,
        icon: crate::cursor::CursorIcon,
        position: InputPosition,
        now: Instant,
    ) -> CursorEvaluation {
        let physical_nominal_size = (f64::from(self.configuration.size()) * self.output_scale)
            .round()
            .clamp(1.0, f64::from(u32::MAX)) as u32;
        let selected = self.resolver.frame(
            icon,
            physical_nominal_size,
            now.saturating_duration_since(self.animation_started),
        );
        let key = Arc::as_ptr(&selected.frame) as usize;
        let texture = if let Some(texture) = self.theme_textures.get(&key) {
            Arc::clone(texture)
        } else {
            let Some(texture) = self.upload_theme_frame(&selected.frame) else {
                return hidden_evaluation();
            };
            self.theme_textures.insert(key, Arc::clone(&texture));
            texture
        };
        let geometry = named_cursor_geometry(
            position.x,
            position.y,
            self.output_scale,
            self.configuration.size(),
            &selected.frame,
        );
        CursorEvaluation {
            overlay: CursorOverlay::visible(
                texture,
                selected.frame.width,
                selected.frame.height,
                geometry,
            ),
            plane: CursorPlaneSnapshot::visible(
                Arc::clone(&selected.frame.pixels),
                selected.frame.width,
                selected.frame.height,
                geometry,
            )
            .unwrap_or_else(CursorPlaneSnapshot::hidden),
            next_animation: selected
                .next_frame_after
                .and_then(|delay| now.checked_add(delay)),
        }
    }

    fn evaluate_client(
        &mut self,
        image: &ClientCursorImage,
        position: InputPosition,
    ) -> CursorEvaluation {
        let key = Arc::as_ptr(&image.pixels) as *const u8 as usize;
        let texture = match &self.client_texture {
            Some((current, texture)) if *current == key => Arc::clone(texture),
            _ => {
                let Some(texture) = self.upload_client_image(image) else {
                    return hidden_evaluation();
                };
                self.client_texture = Some((key, Arc::clone(&texture)));
                texture
            }
        };
        let geometry = client_cursor_geometry(
            position.x,
            position.y,
            self.output_scale,
            self.configuration.size(),
            image,
        );
        CursorEvaluation {
            overlay: CursorOverlay::visible(texture, image.width, image.height, geometry),
            plane: CursorPlaneSnapshot::visible(
                Arc::clone(&image.pixels),
                image.width,
                image.height,
                geometry,
            )
            .unwrap_or_else(CursorPlaneSnapshot::hidden),
            next_animation: None,
        }
    }

    fn upload_theme_frame(&self, frame: &ThemeCursorFrame) -> Option<Arc<CursorTexture>> {
        self.upload(
            &frame.pixels,
            frame.width,
            frame.height,
            wgpu::TextureFormat::Bgra8UnormSrgb,
        )
    }

    fn upload_client_image(&self, image: &ClientCursorImage) -> Option<Arc<CursorTexture>> {
        self.upload(
            &image.pixels,
            image.width,
            image.height,
            wgpu::TextureFormat::Bgra8UnormSrgb,
        )
    }

    fn upload(
        &self,
        pixels: &[u8],
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> Option<Arc<CursorTexture>> {
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| usize::try_from(height).ok().map(|height| (width, height)))
            .and_then(|(width, height)| width.checked_mul(height))
            .and_then(|pixels| pixels.checked_mul(4));
        if width == 0
            || height == 0
            || width > self.max_dimension
            || height > self.max_dimension
            || expected != Some(pixels.len())
        {
            warn!(
                width,
                height,
                bytes = pixels.len(),
                "ignored invalid cursor image upload"
            );
            return None;
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("weld immutable cursor image"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        self.queue.write_texture(
            texture.as_image_copy(),
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            texture.size(),
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Some(Arc::new(CursorTexture {
            _texture: texture,
            view,
        }))
    }
}

fn hidden_evaluation() -> CursorEvaluation {
    CursorEvaluation {
        overlay: CursorOverlay::hidden(),
        plane: CursorPlaneSnapshot::hidden(),
        next_animation: None,
    }
}

fn rounded_extent(value: f32) -> Option<u32> {
    value
        .is_finite()
        .then(|| value.round())
        .filter(|value| *value >= 1.0 && *value <= u32::MAX as f32)
        .map(|value| value as u32)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{CursorGeometry, CursorPlaneSnapshot};
    use wgpu::naga::{
        front::wgsl::parse_str,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    #[test]
    fn cursor_overlay_shader_validates() {
        let module = parse_str(include_str!("cursor_overlay.wgsl")).expect("valid WGSL syntax");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("valid WGSL module");
    }

    #[test]
    fn hardware_snapshot_rounds_kms_origin_and_natural_extent() {
        let snapshot = CursorPlaneSnapshot::visible(
            Arc::from([0, 0, 0, 255]),
            1,
            1,
            CursorGeometry {
                origin_x: 23.4,
                origin_y: 11.6,
                width: 30.4,
                height: 29.6,
                source_x: 0.0,
                source_y: 0.0,
                source_width: 1.0,
                source_height: 1.0,
            },
        )
        .expect("valid hardware cursor snapshot");

        assert_eq!(snapshot.origin(), (23, 12));
        assert_eq!(snapshot.image().map(|image| image.extent()), Some((30, 30)));
    }
}

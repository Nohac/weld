//! Immutable cursor uploads and physical cursor geometry for final presentation.

use std::{collections::HashMap, fmt, sync::Arc, time::Instant};

use tracing::warn;

use crate::{
    cursor::{
        ClientCursorImage, CursorConfiguration, CursorGeometry, CursorImage, ThemeCursorFrame,
        XcursorResolver, client_cursor_geometry, named_cursor_geometry,
    },
    input::InputPosition,
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
    pub(crate) next_animation: Option<Instant>,
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
        next_animation: None,
    }
}

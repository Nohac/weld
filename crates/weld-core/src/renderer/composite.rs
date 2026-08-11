//! Shared GPU composition blit used by nested and direct presentation.

use wgpu::util::DeviceExt;

const CURSOR_UNIFORM_SIZE: usize = 16;

/// Cursor state applied by the physical presenter after Bevy composition.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct CursorOverlay {
    center_x: f32,
    center_y: f32,
    radius: f32,
    visible: f32,
}

impl CursorOverlay {
    pub(crate) fn from_logical(position: Option<(f64, f64)>, scale_factor: f64) -> Self {
        let Some((x, y)) = position else {
            return Self::default();
        };
        Self {
            center_x: (x * scale_factor) as f32,
            center_y: (y * scale_factor) as f32,
            radius: (7.0 * scale_factor) as f32,
            visible: 1.0,
        }
    }

    fn uniform_bytes(self) -> [u8; CURSOR_UNIFORM_SIZE] {
        let mut bytes = [0; CURSOR_UNIFORM_SIZE];
        for (index, value) in [self.center_x, self.center_y, self.radius, self.visible]
            .into_iter()
            .enumerate()
        {
            let offset = index * size_of::<f32>();
            bytes[offset..offset + size_of::<f32>()].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }
}

pub(crate) struct CompositionBlitter {
    bind_group_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    cursor_uniform: wgpu::Buffer,
}

impl CompositionBlitter {
    pub(crate) fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("weld composition bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("weld composition pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("weld composition shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("composite.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("weld composition pipeline"),
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
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("weld composition sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let cursor_uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("weld presenter cursor uniform"),
            contents: &CursorOverlay::default().uniform_bytes(),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
        });
        Self {
            bind_group_layout,
            pipeline,
            sampler,
            cursor_uniform,
        }
    }

    pub(crate) fn set_cursor(&self, queue: &wgpu::Queue, cursor: CursorOverlay) {
        queue.write_buffer(&self.cursor_uniform, 0, &cursor.uniform_bytes());
    }

    pub(crate) fn create_bind_group(
        &self,
        device: &wgpu::Device,
        label: &'static str,
        composition: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(composition),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.cursor_uniform.as_entire_binding(),
                },
            ],
        })
    }

    pub(crate) fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        label: &'static str,
        target: &wgpu::TextureView,
        composition: &wgpu::BindGroup,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.025,
                        g: 0.032,
                        b: 0.045,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, composition, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::CursorOverlay;

    #[test]
    fn cursor_overlay_scales_logical_position_and_radius_to_physical_pixels() {
        assert_eq!(
            CursorOverlay::from_logical(Some((80.0, 40.0)), 1.25),
            CursorOverlay {
                center_x: 100.0,
                center_y: 50.0,
                radius: 8.75,
                visible: 1.0,
            }
        );
        assert_eq!(
            CursorOverlay::from_logical(None, 1.25),
            CursorOverlay::default()
        );
    }
}

//! Native opaque lock-screen composition into backend-owned output targets.

use std::{collections::HashMap, rc::Rc};

use anyhow::{Result, bail};

use super::CompositionBlitter;
use crate::{
    OutputId,
    host::{
        CompositionDestination, CompositionFrame, CompositionOutputFrame, CompositionOutputRequest,
    },
    server::session_lock::{LockFrame, LockImage, LockLayer},
    surface::{Extent, SurfaceLayerId},
};

struct UploadedImage {
    source: Rc<LockImage>,
    view: wgpu::TextureView,
}

pub(crate) struct LockComposition {
    device: wgpu::Device,
    queue: wgpu::Queue,
    images: HashMap<(OutputId, SurfaceLayerId), UploadedImage>,
    scenes: HashMap<OutputId, Rc<LockFrame>>,
    blitters: HashMap<wgpu::TextureFormat, CompositionBlitter>,
}

impl LockComposition {
    pub(crate) fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        Self {
            device,
            queue,
            images: HashMap::new(),
            scenes: HashMap::new(),
            blitters: HashMap::new(),
        }
    }

    pub(crate) fn update(&mut self, scenes: HashMap<OutputId, Rc<LockFrame>>) {
        self.images.retain(|(output, layer), _| {
            scenes.get(output).is_some_and(|frame| {
                frame
                    .layers
                    .iter()
                    .any(|item| item.placement.layer == *layer)
            })
        });
        self.scenes = scenes;
        for (output, layer) in self
            .scenes
            .iter()
            .flat_map(|(output, frame)| frame.layers.iter().map(move |layer| (*output, layer)))
        {
            let image = layer.image.clone();
            let key = (output, layer.placement.layer);
            if self
                .images
                .get(&key)
                .is_some_and(|held| Rc::ptr_eq(&held.source, &image))
            {
                continue;
            }
            let maximum = self.device.limits().max_texture_dimension_2d;
            if image.width > maximum || image.height > maximum {
                self.images.remove(&key);
                tracing::warn!(
                    ?output,
                    "lock buffer exceeds device texture limits; retaining opaque fallback"
                );
                continue;
            }
            let size = wgpu::Extent3d {
                width: image.width,
                height: image.height,
                depth_or_array_layers: 1,
            };
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("session lock SHM image"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8UnormSrgb,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            self.queue.write_texture(
                texture.as_image_copy(),
                &image.pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(image.width * 4),
                    rows_per_image: Some(image.height),
                },
                size,
            );
            self.images.insert(
                key,
                UploadedImage {
                    source: image,
                    view: texture.create_view(&Default::default()),
                },
            );
        }
    }
    pub(crate) fn render_outputs(
        &mut self,
        requests: &[CompositionOutputRequest],
        frames: &mut Vec<CompositionOutputFrame>,
    ) -> Result<()> {
        frames.clear();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("protected session composition"),
            });
        for request in requests {
            let CompositionDestination::External(target) = &request.destination else {
                bail!("session lock renderer requires a native output target");
            };
            // Always replace the entire target, including when the locker has
            // no buffer, has disconnected, or supplied transparent pixels.
            {
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("opaque session lock fallback"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target.view(),
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
            }
            if let Some(scene) = self.scenes.get(&request.output) {
                let blitter = self
                    .blitters
                    .entry(target.format())
                    .or_insert_with(|| CompositionBlitter::new(&self.device, target.format()));
                for layer in &scene.layers {
                    let Some(image) = self.images.get(&(request.output, layer.placement.layer))
                    else {
                        continue;
                    };
                    let Some((viewport, scissor)) = layer_rect(
                        layer,
                        scene,
                        target.extent(),
                        self.device.limits().max_texture_dimension_2d,
                    ) else {
                        continue;
                    };
                    let binding =
                        blitter.create_bind_group(&self.device, "session lock image", &image.view);
                    blitter.encode_overlay(
                        &mut encoder,
                        "session lock image",
                        target.view(),
                        &binding,
                        viewport,
                        [scissor],
                    );
                }
            }
            frames.push(CompositionOutputFrame {
                surfaces: Vec::new(),
                output: request.output,
                frame: CompositionFrame::external(target.clone()),
            });
        }
        self.queue.submit([encoder.finish()]);
        Ok(())
    }
}

type Viewport = (f32, f32, f32, f32);
type Scissor = (u32, u32, u32, u32);

fn layer_rect(
    layer: &LockLayer,
    frame: &LockFrame,
    extent: Extent,
    maximum: u32,
) -> Option<(Viewport, Scissor)> {
    let scale_x = extent.width as f32 / frame.logical_width;
    let scale_y = extent.height as f32 / frame.logical_height;
    let view = layer.placement.view;
    let x = layer.placement.position.x * scale_x;
    let y = layer.placement.position.y * scale_y;
    let width = view.logical_width * scale_x;
    let height = view.logical_height * scale_y;
    let left = x.floor().clamp(0.0, extent.width as f32) as u32;
    let top = y.floor().clamp(0.0, extent.height as f32) as u32;
    let right = (x + width).ceil().clamp(0.0, extent.width as f32) as u32;
    let bottom = (y + height).ceil().clamp(0.0, extent.height as f32) as u32;
    if right <= left || bottom <= top {
        return None;
    }
    // Expand the viewport to the full texture; scissoring selects the client's
    // source rectangle while preserving scaling for partly off-output children.
    let viewport = (
        x - view.source_x / view.source_width * width,
        y - view.source_y / view.source_height * height,
        layer.image.width as f32 / view.source_width * width,
        layer.image.height as f32 / view.source_height * height,
    );
    let (vx, vy, vw, vh) = viewport;
    let max = maximum as f32;
    if ![vx, vy, vw, vh].into_iter().all(f32::is_finite)
        || vw <= 0.0
        || vh <= 0.0
        || vw > max
        || vh > max
        || vx < -max * 2.0
        || vy < -max * 2.0
        || vx + vw > max * 2.0 - 1.0
        || vy + vh > max * 2.0 - 1.0
    {
        return None;
    }
    Some((viewport, (left, top, right - left, bottom - top)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        host::CompositionTargetView,
        surface::{Extent, LogicalPoint, SurfaceContentView, SurfaceLayerPlacement},
    };

    #[test]
    #[ignore = "requires a Vulkan adapter for offscreen composition readback"]
    fn protected_composition_replaces_desktop_pixels_and_stays_opaque() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter =
            pollster::block_on(instance.request_adapter(&Default::default())).expect("adapter");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&Default::default())).expect("device");
        let extent = Extent::new(8, 8);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("lock composition test output"),
            size: wgpu::Extent3d {
                width: extent.width,
                height: extent.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let target = CompositionTargetView::new(
            texture.create_view(&Default::default()),
            extent,
            texture.format(),
        );
        let output = OutputId::new(1);
        let requests = [CompositionOutputRequest {
            output,
            destination: CompositionDestination::External(target.clone()),
        }];
        let mut composition = LockComposition::new(device.clone(), queue.clone());
        let mut frames = Vec::new();
        for images in [
            HashMap::new(),
            HashMap::from([(
                output,
                Rc::new(LockImage {
                    width: 1,
                    height: 1,
                    pixels: vec![0, 0, 0, 0],
                }),
            )]),
            HashMap::from([(
                output,
                Rc::new(LockImage {
                    width: 1,
                    height: 1,
                    pixels: vec![0, 0, 255, 255],
                }),
            )]),
            HashMap::new(),
        ] {
            let expected = if images.values().any(|image| image.pixels[2] == 255) {
                [255, 0, 0, 255]
            } else {
                [0, 0, 0, 255]
            };
            queue.write_texture(
                texture.as_image_copy(),
                &[255; 8 * 8 * 4],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(32),
                    rows_per_image: Some(8),
                },
                texture.size(),
            );
            let scenes = images
                .into_iter()
                .map(|(output, image)| {
                    (
                        output,
                        Rc::new(LockFrame {
                            logical_width: 8.0,
                            logical_height: 8.0,
                            layers: vec![LockLayer {
                                placement: SurfaceLayerPlacement {
                                    layer: SurfaceLayerId::new(1),
                                    position: LogicalPoint::ZERO,
                                    view: SurfaceContentView {
                                        source_x: 0.0,
                                        source_y: 0.0,
                                        source_width: image.width as f32,
                                        source_height: image.height as f32,
                                        logical_width: 8.0,
                                        logical_height: 8.0,
                                    },
                                },
                                image,
                            }],
                        }),
                    )
                })
                .collect();
            composition.update(scenes);
            composition
                .render_outputs(&requests, &mut frames)
                .expect("protected frame");
            let frame = CompositionFrame::owned(target.clone(), texture.clone());
            let pixels =
                crate::renderer::read_owned_frame_rgba(&device, &queue, &frame).expect("readback");
            assert!(
                pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| *pixel == expected),
                "desktop pixels or transparency escaped protected composition"
            );
        }
        let background = Rc::new(LockImage {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 255],
        });
        let indicator = Rc::new(LockImage {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 255, 255],
        });
        let layer = |id, x, y, width, height, image| LockLayer {
            placement: SurfaceLayerPlacement {
                layer: SurfaceLayerId::new(id),
                position: LogicalPoint::new(x, y),
                view: SurfaceContentView {
                    source_x: 0.0,
                    source_y: 0.0,
                    source_width: 1.0,
                    source_height: 1.0,
                    logical_width: width,
                    logical_height: height,
                },
            },
            image,
        };
        for visible in [true, false] {
            let mut layers = vec![layer(1, 0.0, 0.0, 4.0, 4.0, background.clone())];
            if visible {
                layers.push(layer(2, -1.0, 1.0, 3.0, 1.0, indicator.clone()));
            }
            composition.update(HashMap::from([(
                output,
                Rc::new(LockFrame {
                    logical_width: 4.0,
                    logical_height: 4.0,
                    layers,
                }),
            )]));
            composition
                .render_outputs(&requests, &mut frames)
                .expect("indicator frame");
            let pixels = crate::renderer::read_owned_frame_rgba(
                &device,
                &queue,
                &CompositionFrame::owned(target.clone(), texture.clone()),
            )
            .expect("indicator readback");
            for (index, pixel) in pixels.as_chunks::<4>().0.iter().enumerate() {
                let expected = if visible && index % 8 < 4 && (2..4).contains(&(index / 8)) {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 0, 255]
                };
                assert_eq!(
                    *pixel, expected,
                    "scaled, clipped indicator at pixel {index}"
                );
            }
        }
    }
}

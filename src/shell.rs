//! Bevy-owned compositor scene rendered into a Weld-owned wgpu texture.

use std::sync::Arc;

use anyhow::{Context, Result};
use bevy::{
    app::{App, PluginGroup, PostUpdate, TerminalCtrlCHandlerPlugin},
    camera::{
        Camera, Camera2d, ClearColorConfig, CompositingSpace, ManualTextureViewHandle,
        NormalizedRenderTarget, RenderTarget,
    },
    ecs::{
        message::{MessageCursor, Messages},
        schedule::IntoScheduleConfigs,
        world::World,
    },
    log::LogPlugin,
    math::UVec2,
    prelude::{
        AlignItems, BackgroundColor, BorderRadius, ChildOf, Color, DefaultPlugins, Entity,
        GlobalZIndex, LayoutConfig, Node, PositionType, Scene, UiRect, UiTargetCamera, With,
        Without, px,
    },
    render::{
        RenderApp, RenderPlugin,
        renderer::{
            RenderAdapter, RenderAdapterInfo, RenderDevice, RenderInstance, RenderQueue,
            WgpuWrapper,
        },
        settings::RenderCreation,
        texture::{ManualTextureView, ManualTextureViews},
    },
    scene::{WorldSceneExt, bsn},
    time::TimeReceiver,
    ui::{UiScale, UiSystems},
    window::{ExitCondition, RequestRedraw, WindowPlugin},
};

use crate::composition::{CompositionPlugin, CompositorCamera, set_composition_advance};
use crate::debug::{
    CaptureRequest, DebugProtocolPlugin, complete_capture, configure_remote_debug,
    take_capture_request,
};
use crate::dmabuf::{DmabufImporter, DmabufReleaseId, DmabufSourceCache};
use crate::input::raw::{InputPosition, RawSeatEvent};
use crate::input::{
    GlobalShortcutPlugin, InputBridgePlugin, SeatInputEffect, VirtualTerminalShortcutPlugin,
    enqueue_raw_input, projected_pointer_position, set_input_update_time, take_host_commands,
    take_input_effects, take_virtual_terminal_switch_request,
};
use crate::layer::SHELL_Z_INDEX;
use crate::runtime::HostCommand;
use crate::server::{
    PendingSurfaceBufferContent, PendingSurfaceEvent, PendingSurfaceEventKind,
    PendingSurfaceTreeSnapshot,
};
use crate::surface::{
    HostSurfaceEvent, HostSurfaceEventKind, SurfaceAction, SurfaceBufferContent,
    SurfaceBufferUpdate, SurfacePlugin, SurfaceTreeSnapshot, enqueue_surface_event,
    has_surface_frame, take_surface_actions,
};
use crate::window::{DefaultWindowPlugin, set_output_physical_size, set_output_scale_factor};
use calloop::channel::Sender as CalloopSender;

const COMPOSITION_VIEW: ManualTextureViewHandle = ManualTextureViewHandle(1);
const COMPOSITION_TARGET_COUNT: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CompositionTargetId(usize);

impl CompositionTargetId {
    pub(crate) const FIRST: Self = Self(0);
    pub(crate) const SECOND: Self = Self(1);
}

struct CompositionTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

pub struct ShellRenderer {
    app: App,
    redraw_requests: RedrawRequests,
    device: wgpu::Device,
    composition_targets: [CompositionTarget; COMPOSITION_TARGET_COUNT],
    completed_target: CompositionTargetId,
    dmabuf_importer: Option<DmabufImporter>,
    dmabuf_release_sender: CalloopSender<DmabufReleaseId>,
}

pub(crate) struct ShellRendererOptions<'a> {
    pub(crate) size: UVec2,
    pub(crate) scale_factor: f64,
    pub(crate) remote_debug: Option<&'a str>,
    pub(crate) virtual_terminal_shortcuts: bool,
}

impl ShellRenderer {
    pub fn new(
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        dmabuf_release_sender: CalloopSender<DmabufReleaseId>,
        dmabuf_sources: DmabufSourceCache,
        options: ShellRendererOptions<'_>,
    ) -> Result<Self> {
        let ShellRendererOptions {
            size,
            scale_factor,
            remote_debug,
            virtual_terminal_shortcuts,
        } = options;
        let render_creation = RenderCreation::manual(
            RenderDevice::from(device.clone()),
            RenderQueue(Arc::new(WgpuWrapper::new(queue.clone()))),
            RenderAdapterInfo(WgpuWrapper::new(adapter.get_info())),
            RenderAdapter(Arc::new(WgpuWrapper::new(adapter.clone()))),
            RenderInstance(Arc::new(WgpuWrapper::new(instance.clone()))),
        );
        let render_plugin = RenderPlugin {
            render_creation,
            synchronous_pipeline_compilation: true,
            ..Default::default()
        };
        let window_plugin = WindowPlugin {
            primary_window: None,
            exit_condition: ExitCondition::DontExit,
            ..Default::default()
        };

        let mut app = App::new();
        app.add_plugins(
            DefaultPlugins
                .set(window_plugin)
                .set(render_plugin)
                .disable::<LogPlugin>()
                .disable::<TerminalCtrlCHandlerPlugin>(),
        )
        .add_plugins((
            DebugProtocolPlugin,
            CompositionPlugin,
            SurfacePlugin,
            DefaultWindowPlugin::new(size, scale_factor),
            InputBridgePlugin::new(NormalizedRenderTarget::TextureView(COMPOSITION_VIEW)),
            GlobalShortcutPlugin,
        ))
        .add_systems(
            PostUpdate,
            disable_ui_rounding_on_roots.before(UiSystems::Layout),
        )
        .insert_resource(UiScale(scale_factor as f32));
        if let Some(address) = remote_debug {
            configure_remote_debug(&mut app, address)
                .context("failed to configure remote debugging")?;
        }
        if virtual_terminal_shortcuts {
            app.add_plugins(VirtualTerminalShortcutPlugin);
        }
        app.finish();
        app.cleanup();
        app.get_sub_app(RenderApp).context(
            "Bevy RenderPlugin did not create the non-pipelined RenderApp required by Weld",
        )?;
        disconnect_render_time(&mut app)?;

        let composition_targets = create_composition_targets(device, size.x, size.y);
        insert_manual_view(
            &mut app,
            composition_targets[0].view.clone(),
            size.x,
            size.y,
        );

        let camera = app
            .world_mut()
            .spawn((
                Camera2d,
                Camera {
                    clear_color: ClearColorConfig::Custom(Color::NONE),
                    ..Default::default()
                },
                RenderTarget::TextureView(COMPOSITION_VIEW),
                // Bevy UI shaders emit linear RGB. The manual sRGB target performs
                // the transfer encoding when those values are written.
                CompositingSpace::Linear,
            ))
            .id();
        app.insert_resource(CompositorCamera(camera));
        app.world_mut()
            .spawn_scene(shell_overlay(camera))
            .context("failed to spawn the BSN shell overlay")?
            .insert(UiTargetCamera(camera));
        let redraw_requests = app
            .world()
            .get_resource::<Messages<RequestRedraw>>()
            .map(RedrawRequests::new)
            .context("Bevy WindowPlugin did not register redraw messages")?;
        let dmabuf_importer =
            DmabufImporter::new(device, queue, dmabuf_release_sender.clone(), dmabuf_sources)?;

        Ok(Self {
            app,
            redraw_requests,
            device: device.clone(),
            composition_targets,
            completed_target: CompositionTargetId::FIRST,
            dmabuf_importer,
            dmabuf_release_sender,
        })
    }

    /// Advance Bevy policy and input without necessarily extracting or rendering.
    ///
    /// The host must call this exactly once for each logical advance. Client
    /// surface events are applied only when `composition_advance` is true, so
    /// their asset events remain paired with [`Self::render_composition`].
    /// Bevy time advances here at input/policy rate rather than render rate;
    /// systems must use time deltas instead of assuming one update per frame.
    pub fn advance_main(&mut self, input_time: u32, composition_advance: bool) -> bool {
        set_input_update_time(self.app.world_mut(), input_time);
        set_composition_advance(self.app.world_mut(), composition_advance);
        advance_main_app(&mut self.app, &mut self.redraw_requests)
    }

    /// Extract the preceding main-world advance and render Weld's composition.
    ///
    /// Construction pins Weld to Bevy's current non-pipelined [`RenderApp`].
    /// Main-world trackers are retained across input-only advances and cleared
    /// only after extraction has observed them.
    pub(crate) fn render_composition(&mut self) -> CompositionTargetId {
        // Keep the current presenters pinned to their original target. Direct
        // DRM selects between both targets explicitly once worker ownership is
        // active, avoiding per-frame Bevy bind-group churn before then.
        let target = CompositionTargetId::FIRST;
        self.render_composition_to(target);
        target
    }

    /// Render into one explicitly host-owned composition target.
    ///
    /// Direct presentation uses the target identity to prevent Bevy from
    /// overwriting a texture while the presentation worker still owns it.
    pub(crate) fn render_composition_to(&mut self, target: CompositionTargetId) {
        let composition_target = &self.composition_targets[target.0];
        let size = composition_target.texture.size();
        insert_manual_view(
            &mut self.app,
            composition_target.view.clone(),
            size.width,
            size.height,
        );
        render_composition_app(&mut self.app);
        self.completed_target = target;
    }

    pub fn should_exit(&self) -> bool {
        self.app.should_exit().is_some()
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let composition_targets = create_composition_targets(&self.device, width, height);
        insert_manual_view(
            &mut self.app,
            composition_targets[0].view.clone(),
            width,
            height,
        );
        set_output_physical_size(self.app.world_mut(), UVec2::new(width, height));
        self.composition_targets = composition_targets;
        self.completed_target = CompositionTargetId::FIRST;
    }

    /// Set the compositor-logical to physical scale used by Bevy UI layout.
    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        self.app.world_mut().resource_mut::<UiScale>().0 = scale_factor as f32;
        set_output_scale_factor(self.app.world_mut(), scale_factor);
    }

    pub fn texture_view(&self) -> &wgpu::TextureView {
        self.target_view(self.completed_target)
    }

    pub(crate) const fn completed_target(&self) -> CompositionTargetId {
        self.completed_target
    }

    pub(crate) fn target_view(&self, target: CompositionTargetId) -> &wgpu::TextureView {
        &self.composition_targets[target.0].view
    }

    pub(crate) fn target_texture(&self, target: CompositionTargetId) -> &wgpu::Texture {
        &self.composition_targets[target.0].texture
    }

    pub(crate) const fn target_ids(&self) -> [CompositionTargetId; COMPOSITION_TARGET_COUNT] {
        [CompositionTargetId::FIRST, CompositionTargetId::SECOND]
    }

    pub fn enqueue_surface_event(&mut self, event: PendingSurfaceEvent) {
        let PendingSurfaceEvent { surface, kind } = event;
        match kind {
            PendingSurfaceEventKind::TreeSnapshot(snapshot) => {
                let snapshot = self.prepare_surface_snapshot(surface, snapshot);
                enqueue_surface_event(
                    self.app.world_mut(),
                    HostSurfaceEvent {
                        surface,
                        kind: HostSurfaceEventKind::TreeSnapshot(snapshot),
                    },
                );
            }
            PendingSurfaceEventKind::Destroyed => {
                if let Some(importer) = &mut self.dmabuf_importer {
                    importer.remove_surface(surface);
                }
                enqueue_surface_event(
                    self.app.world_mut(),
                    HostSurfaceEvent {
                        surface,
                        kind: HostSurfaceEventKind::Destroyed,
                    },
                );
            }
            kind => {
                let event = PendingSurfaceEvent { surface, kind };
                if let Some(event) = event.without_tree_payload() {
                    enqueue_surface_event(self.app.world_mut(), event);
                }
            }
        }
    }

    fn prepare_surface_snapshot(
        &mut self,
        surface: crate::surface::SurfaceId,
        snapshot: PendingSurfaceTreeSnapshot,
    ) -> SurfaceTreeSnapshot {
        let PendingSurfaceTreeSnapshot {
            client_mapped,
            root,
            window_geometry,
            overlays,
            inputs,
            buffers,
        } = snapshot;
        let retained = buffers.iter().map(|buffer| buffer.layer).collect();
        if let Some(importer) = &mut self.dmabuf_importer {
            importer.retain_surface_layers(surface, &retained);
        }
        let buffers = buffers
            .into_iter()
            .map(|buffer| {
                let content = match buffer.content {
                    PendingSurfaceBufferContent::Retained => SurfaceBufferContent::Retained,
                    PendingSurfaceBufferContent::ShmPixels(pixels) => {
                        if let Some(importer) = &mut self.dmabuf_importer {
                            importer.remove_layer(surface, buffer.layer);
                        }
                        SurfaceBufferContent::Pixels(pixels)
                    }
                    PendingSurfaceBufferContent::ImportedDmabuf(frame) => {
                        let imported = if let Some(importer) = &mut self.dmabuf_importer {
                            importer
                                .import(
                                    &mut self.app,
                                    surface,
                                    buffer.layer,
                                    frame,
                                    buffer.opaque,
                                )
                                .map_err(|error| {
                                    tracing::warn!(
                                        %error,
                                        ?surface,
                                        layer = ?buffer.layer,
                                        "failed to import a committed DMA-BUF"
                                    );
                                })
                                .ok()
                        } else {
                            let _ = self.dmabuf_release_sender.send(frame.release);
                            tracing::warn!(?surface, layer = ?buffer.layer, "received a DMA-BUF without an importer");
                            None
                        };
                        imported
                            .map(SurfaceBufferContent::RenderImage)
                            .unwrap_or(SurfaceBufferContent::Retained)
                    }
                };
                SurfaceBufferUpdate {
                    layer: buffer.layer,
                    width: buffer.width,
                    height: buffer.height,
                    content,
                    opaque: buffer.opaque,
                }
            })
            .collect();
        SurfaceTreeSnapshot {
            client_mapped,
            root,
            window_geometry,
            overlays,
            inputs,
            buffers,
        }
    }

    pub fn enqueue_input_event(&mut self, event: RawSeatEvent) {
        enqueue_raw_input(self.app.world_mut(), event);
    }

    pub(crate) fn pointer_position(&self) -> Option<InputPosition> {
        projected_pointer_position(self.app.world())
    }

    pub fn take_input_effects(&mut self) -> Vec<SeatInputEffect> {
        take_input_effects(self.app.world_mut())
    }

    pub fn take_host_commands(&mut self) -> Vec<HostCommand> {
        take_host_commands(self.app.world_mut())
    }

    pub fn take_virtual_terminal_switch_request(&mut self) -> Option<i32> {
        take_virtual_terminal_switch_request(self.app.world_mut())
    }

    pub fn take_surface_actions(&mut self) -> Vec<SurfaceAction> {
        take_surface_actions(self.app.world_mut())
    }

    pub fn has_surface_frame(&self) -> bool {
        has_surface_frame(self.app.world())
    }

    pub fn take_capture_request(&mut self) -> Option<CaptureRequest> {
        take_capture_request(self.app.world_mut())
    }

    pub fn complete_capture(&mut self, request_id: u64, result: Result<(), String>) {
        complete_capture(self.app.world_mut(), request_id, result);
    }
}

fn advance_main_app(app: &mut App, redraw_requests: &mut RedrawRequests) -> bool {
    app.main_mut().run_default_schedule();
    let Some(messages) = app.world().get_resource::<Messages<RequestRedraw>>() else {
        return false;
    };
    redraw_requests.take(messages)
}

fn disconnect_render_time(app: &mut App) -> Result<()> {
    // Weld advances the non-pipelined main world independently of RenderApp,
    // so Automatic time must use its documented main-world clock fallback.
    // Dropping the sole receiver is load-bearing: Bevy 0.19's render-side
    // send_time explicitly ignores Disconnected. Re-verify both assumptions
    // when updating Bevy.
    let receiver = app
        .world_mut()
        .remove_resource::<TimeReceiver>()
        .context("Bevy RenderPlugin did not register its main-world TimeReceiver")?;
    drop(receiver);
    Ok(())
}

fn render_composition_app(app: &mut App) {
    app.update_sub_app_by_label(RenderApp);
    app.world_mut().clear_trackers();
}

struct RedrawRequests(MessageCursor<RequestRedraw>);

impl RedrawRequests {
    fn new(messages: &Messages<RequestRedraw>) -> Self {
        Self(messages.get_cursor_current())
    }

    fn take(&mut self, messages: &Messages<RequestRedraw>) -> bool {
        let pending = self.0.len(messages) > 0;
        self.0.clear(messages);
        pending
    }
}

fn create_composition_targets(
    device: &wgpu::Device,
    width: u32,
    height: u32,
) -> [CompositionTarget; COMPOSITION_TARGET_COUNT] {
    std::array::from_fn(|_| create_composition_target(device, width, height))
}

fn create_composition_target(device: &wgpu::Device, width: u32, height: u32) -> CompositionTarget {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("weld Bevy composition"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    CompositionTarget {
        texture,
        view: texture_view,
    }
}

fn insert_manual_view(app: &mut App, texture_view: wgpu::TextureView, width: u32, height: u32) {
    app.world_mut().resource_mut::<ManualTextureViews>().insert(
        COMPOSITION_VIEW,
        ManualTextureView {
            texture_view: texture_view.into(),
            size: UVec2::new(width, height),
            view_format: wgpu::TextureFormat::Rgba8UnormSrgb,
        },
    );
}

fn disable_ui_rounding_on_roots(world: &mut World) {
    let roots = {
        let mut query =
            world.query_filtered::<Entity, (With<Node>, Without<ChildOf>, Without<LayoutConfig>)>();
        query.iter(world).collect::<Vec<_>>()
    };
    for root in roots {
        world.entity_mut(root).insert(LayoutConfig {
            use_rounding: false,
        });
    }
}

fn shell_overlay(_camera: Entity) -> impl Scene {
    bsn! {
        Node {
            position_type: PositionType::Absolute,
            top: px(24),
            right: px(24),
            width: px(240),
            height: px(88),
            padding: UiRect::all(px(16)),
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(18)),
        }
        BackgroundColor(Color::srgba(0.08, 0.34, 0.48, 0.82))
        GlobalZIndex(SHELL_Z_INDEX)
    }
}

#[cfg(test)]
mod tests {
    use bevy::{
        app::{SubApp, Update},
        ecs::{
            message::MessageWriter,
            resource::Resource,
            schedule::{Schedule, ScheduleLabel},
            system::ResMut,
        },
        render::RenderApp,
        time::{Real, Time, TimePlugin, TimeReceiver, create_time_channels},
        window::{ExitCondition, RequestRedraw, WindowPlugin},
    };

    use super::{
        App, Messages, RedrawRequests, advance_main_app, disconnect_render_time,
        render_composition_app,
    };

    #[derive(Resource, Default)]
    struct RenderCount(u32);

    fn request_redraw(mut requests: MessageWriter<RequestRedraw>) {
        requests.write(RequestRedraw);
        requests.write(RequestRedraw);
    }

    fn count_render(mut count: ResMut<RenderCount>) {
        count.0 += 1;
    }

    fn test_app() -> (App, RedrawRequests) {
        let mut app = App::new();
        app.add_plugins(WindowPlugin {
            primary_window: None,
            exit_condition: ExitCondition::DontExit,
            ..Default::default()
        })
        .add_systems(Update, request_redraw);
        let requests = RedrawRequests::new(app.world().resource::<Messages<RequestRedraw>>());
        (app, requests)
    }

    #[test]
    fn consumes_redraw_requests_once_after_each_app_update() {
        let (mut app, mut requests) = test_app();

        // The full Weld app may retain messages longer through TimePlugin; this
        // minimal app exercises the shorter default per-update retention.
        assert!(advance_main_app(&mut app, &mut requests));
        assert!(!requests.take(app.world().resource::<Messages<RequestRedraw>>()));

        assert!(advance_main_app(&mut app, &mut requests));
    }

    #[test]
    fn main_advance_skips_render_app_until_composition() {
        let (mut app, mut requests) = test_app();
        let mut render_app = SubApp::new();
        render_app
            .world_mut()
            .insert_resource(RenderCount::default());
        let mut render_schedule = Schedule::new(Update);
        render_schedule.add_systems(count_render);
        render_app.world_mut().add_schedule(render_schedule);
        render_app.update_schedule = Some(Update.intern());
        app.insert_sub_app(RenderApp, render_app);

        advance_main_app(&mut app, &mut requests);
        assert_eq!(
            app.sub_app(RenderApp).world().resource::<RenderCount>().0,
            0
        );

        render_composition_app(&mut app);
        assert_eq!(
            app.sub_app(RenderApp).world().resource::<RenderCount>().0,
            1
        );
    }

    #[test]
    fn disconnected_render_time_uses_the_main_world_clock_fallback() {
        let mut app = App::new();
        app.add_plugins(TimePlugin);
        let (_sender, receiver) = create_time_channels();
        app.insert_resource(receiver);

        disconnect_render_time(&mut app).expect("test receiver should be removed");
        assert!(!app.world().contains_resource::<TimeReceiver>());
        app.update();
        let first_elapsed = app.world().resource::<Time<Real>>().elapsed();
        app.update();
        let second_elapsed = app.world().resource::<Time<Real>>().elapsed();
        assert!(second_elapsed > first_elapsed);
    }
}

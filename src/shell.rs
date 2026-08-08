//! Bevy-owned compositor scene rendered into a Weld-owned wgpu texture.

use std::sync::Arc;

use anyhow::{Context, Result};
use bevy::{
    app::{App, PluginGroup},
    camera::{
        Camera, Camera2d, ClearColorConfig, CompositingSpace, ManualTextureViewHandle,
        NormalizedRenderTarget, RenderTarget,
    },
    ecs::message::{MessageCursor, Messages},
    log::LogPlugin,
    math::UVec2,
    prelude::{
        AlignItems, BackgroundColor, BorderRadius, Color, DefaultPlugins, Entity, GlobalZIndex,
        Node, PositionType, Scene, UiRect, UiTargetCamera, px,
    },
    render::{
        RenderPlugin,
        renderer::{
            RenderAdapter, RenderAdapterInfo, RenderDevice, RenderInstance, RenderQueue,
            WgpuWrapper,
        },
        settings::RenderCreation,
        texture::{ManualTextureView, ManualTextureViews},
    },
    scene::{WorldSceneExt, bsn},
    window::{ExitCondition, RequestRedraw, WindowPlugin},
};

use crate::compositor::{
    CompositorCamera, HostSurfaceEvent, SurfaceCompositorPlugin, enqueue_surface_event,
    has_surface_frame,
};
use crate::debug::{
    CaptureRequest, DebugProtocolPlugin, complete_capture, configure_remote_debug,
    take_capture_request,
};
use crate::input::{
    InputBridgePlugin, SeatInputEffect, enqueue_raw_input, set_input_update_time,
    take_input_effects,
};
use crate::raw_input::RawSeatEvent;

const COMPOSITION_VIEW: ManualTextureViewHandle = ManualTextureViewHandle(1);

pub struct ShellRenderer {
    app: App,
    redraw_requests: RedrawRequests,
    device: wgpu::Device,
    composition_texture: wgpu::Texture,
    composition_view: wgpu::TextureView,
}

impl ShellRenderer {
    pub fn new(
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        remote_debug: Option<&str>,
    ) -> Result<Self> {
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
                .disable::<LogPlugin>(),
        )
        .add_plugins((
            DebugProtocolPlugin,
            SurfaceCompositorPlugin,
            InputBridgePlugin::new(NormalizedRenderTarget::TextureView(COMPOSITION_VIEW)),
        ));
        if let Some(address) = remote_debug {
            configure_remote_debug(&mut app, address)
                .context("failed to configure remote debugging")?;
        }
        app.finish();
        app.cleanup();

        let (composition_texture, composition_view) =
            create_composition_target(device, width, height);
        insert_manual_view(&mut app, composition_view.clone(), width, height);

        let camera = app
            .world_mut()
            .spawn((
                Camera2d,
                Camera {
                    clear_color: ClearColorConfig::Custom(Color::NONE),
                    ..Default::default()
                },
                RenderTarget::TextureView(COMPOSITION_VIEW),
                CompositingSpace::Srgb,
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

        Ok(Self {
            app,
            redraw_requests,
            device: device.clone(),
            composition_texture,
            composition_view,
        })
    }

    pub fn update(&mut self, input_time: u32) {
        set_input_update_time(self.app.world_mut(), input_time);
        self.app.update();
    }

    /// Consume redraw requests produced by the preceding [`App::update`].
    ///
    /// The host must call this exactly once after every update so requests are
    /// neither dropped nor replayed across event-loop iterations.
    pub fn take_redraw_request(&mut self) -> bool {
        let Some(messages) = self.app.world().get_resource::<Messages<RequestRedraw>>() else {
            return false;
        };
        self.redraw_requests.take(messages)
    }

    pub fn should_exit(&self) -> bool {
        self.app.should_exit().is_some()
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let (composition_texture, composition_view) =
            create_composition_target(&self.device, width, height);
        insert_manual_view(&mut self.app, composition_view.clone(), width, height);
        self.composition_texture = composition_texture;
        self.composition_view = composition_view;
    }

    pub fn texture_view(&self) -> &wgpu::TextureView {
        &self.composition_view
    }

    pub fn enqueue_surface_event(&mut self, event: HostSurfaceEvent) {
        enqueue_surface_event(self.app.world_mut(), event);
    }

    pub fn enqueue_input_event(&mut self, event: RawSeatEvent) {
        enqueue_raw_input(self.app.world_mut(), event);
    }

    pub fn take_input_effects(&mut self) -> Vec<SeatInputEffect> {
        take_input_effects(self.app.world_mut())
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

fn create_composition_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
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
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, texture_view)
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
        GlobalZIndex(100)
    }
}

#[cfg(test)]
mod tests {
    use bevy::{
        app::Update,
        ecs::message::MessageWriter,
        window::{ExitCondition, RequestRedraw, WindowPlugin},
    };

    use super::{App, Messages, RedrawRequests};

    fn request_redraw(mut requests: MessageWriter<RequestRedraw>) {
        requests.write(RequestRedraw);
        requests.write(RequestRedraw);
    }

    #[test]
    fn consumes_redraw_requests_once_after_each_app_update() {
        let mut app = App::new();
        app.add_plugins(WindowPlugin {
            primary_window: None,
            exit_condition: ExitCondition::DontExit,
            ..Default::default()
        })
        .add_systems(Update, request_redraw);
        let mut requests = RedrawRequests::new(app.world().resource::<Messages<RequestRedraw>>());

        // The full Weld app may retain messages longer through TimePlugin; this
        // minimal app exercises the shorter default per-update retention.
        app.update();
        assert!(requests.take(app.world().resource::<Messages<RequestRedraw>>()));
        assert!(!requests.take(app.world().resource::<Messages<RequestRedraw>>()));

        app.update();
        assert!(requests.take(app.world().resource::<Messages<RequestRedraw>>()));
    }
}

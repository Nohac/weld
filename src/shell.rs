//! Bevy-owned shell scene rendered into a Weld-owned wgpu texture.

use std::sync::Arc;

use anyhow::{Context, Result};
use bevy::{
    app::{App, PluginGroup},
    camera::{
        Camera, Camera2d, ClearColorConfig, CompositingSpace, ManualTextureViewHandle, RenderTarget,
    },
    log::LogPlugin,
    math::UVec2,
    prelude::{
        AlignItems, BackgroundColor, BorderRadius, Color, DefaultPlugins, Entity, Node,
        PositionType, Scene, UiRect, UiTargetCamera, px,
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
    window::{ExitCondition, WindowPlugin},
};

use crate::debug::{
    CaptureRequest, DebugProtocolPlugin, complete_capture, configure_remote_debug,
    take_capture_request,
};

const OVERLAY_VIEW: ManualTextureViewHandle = ManualTextureViewHandle(1);

pub struct ShellRenderer {
    app: App,
    device: wgpu::Device,
    texture: wgpu::Texture,
    texture_view: wgpu::TextureView,
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
        .add_plugins(DebugProtocolPlugin);
        if let Some(address) = remote_debug {
            configure_remote_debug(&mut app, address)
                .context("failed to configure remote debugging")?;
        }
        app.finish();
        app.cleanup();

        let (texture, texture_view) = create_overlay_target(device, width, height);
        insert_manual_view(&mut app, texture_view.clone(), width, height);

        let camera = app
            .world_mut()
            .spawn((
                Camera2d,
                Camera {
                    clear_color: ClearColorConfig::Custom(Color::NONE),
                    ..Default::default()
                },
                RenderTarget::TextureView(OVERLAY_VIEW),
                CompositingSpace::Srgb,
            ))
            .id();
        app.world_mut()
            .spawn_scene(shell_overlay(camera))
            .context("failed to spawn the BSN shell overlay")?
            .insert(UiTargetCamera(camera));

        Ok(Self {
            app,
            device: device.clone(),
            texture,
            texture_view,
        })
    }

    pub fn update(&mut self) {
        self.app.update();
    }

    pub fn should_exit(&self) -> bool {
        self.app.should_exit().is_some()
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let (texture, texture_view) = create_overlay_target(&self.device, width, height);
        insert_manual_view(&mut self.app, texture_view.clone(), width, height);
        self.texture = texture;
        self.texture_view = texture_view;
    }

    pub fn texture_view(&self) -> &wgpu::TextureView {
        &self.texture_view
    }

    pub fn take_capture_request(&mut self) -> Option<CaptureRequest> {
        take_capture_request(self.app.world_mut())
    }

    pub fn complete_capture(&mut self, request_id: u64, result: Result<(), String>) {
        complete_capture(self.app.world_mut(), request_id, result);
    }
}

fn create_overlay_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("weld shell overlay"),
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
        OVERLAY_VIEW,
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
    }
}

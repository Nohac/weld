//! Main-thread Godot API shared by all native providers.
mod canvas;
mod decoration;
mod input;
mod native_canvas;
mod overlap;
mod placement;
mod stacking;
mod stereo;
mod workspace;
mod xr;

use crate::playback::session::Session;
use crate::playback::{Controller, Source};
use crate::presentation::{RasterSizing, XrPreferences, fit};
use godot::{
    classes::{Control, Engine, INode, InputEvent, Node, Object, Os, notify::NodeNotification},
    prelude::*,
};
use input::DesktopInput;
use std::path::PathBuf;
use std::time::Instant;
use stereo::ViewLayout;
use weld_client::PresentationRate;
use workspace::{WeldSurface, Workspace};

fn presenter_rate(millihertz: i64) -> Option<PresentationRate> {
    u32::try_from(millihertz)
        .ok()
        .and_then(|rate| PresentationRate::try_from(rate).ok())
}

#[cfg(test)]
mod rate_tests {
    use super::presenter_rate;

    #[test]
    fn invalid_refresh_samples_are_not_fallback_rate_changes() {
        for invalid in [-1, 0, 999, 1_000_001, i64::MAX] {
            assert!(presenter_rate(invalid).is_none());
        }
        for valid in [30_000, 59_940, 75_000, 90_000, 120_000] {
            assert_eq!(
                presenter_rate(valid).expect("rate").millihertz(),
                valid as u32
            );
        }
    }
}

#[derive(GodotClass)]
#[class(base=Node)]
pub struct WeldVideoPlayer {
    controller: Option<Controller>,
    workspace: Option<Workspace>,
    desktop_input: Option<DesktopInput>,
    live_source: bool,
    message: String,
    xr_preferences: Option<XrPreferences>,
    raster_sizing: RasterSizing,
    shape: Option<decoration::Clip>,
    view_layout: ViewLayout,
    base: Base<Node>,
}
#[godot_api]
impl INode for WeldVideoPlayer {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            controller: None,
            workspace: None,
            desktop_input: None,
            live_source: false,
            message: "Native AV1 video fixture".into(),
            xr_preferences: None,
            raster_sizing: RasterSizing::default(),
            shape: None,
            view_layout: ViewLayout::Mono,
        }
    }
    fn ready(&mut self) {
        self.update_processing();
    }
    fn input(&mut self, event: Gd<InputEvent>) {
        if !self.live_source || !self.validate_input_view() {
            return;
        }
        let position = self
            .base()
            .get_viewport()
            .map(|view| view.get_mouse_position())
            .unwrap_or(Vector2::ZERO);
        if let Some(workspace) = &mut self.workspace {
            if let Some((player, view)) = workspace.pick(position) {
                let player = player.bind();
                if let (Some(input), Some(controller)) =
                    (&mut self.desktop_input, &player.controller)
                {
                    input.set_view(view);
                    if input.handle(event, controller)
                        && let Some(mut viewport) = self.base().get_viewport()
                    {
                        viewport.set_input_as_handled();
                    }
                }
            }
            return;
        }
        if let (Some(input), Some(controller)) = (&mut self.desktop_input, &self.controller)
            && input.handle(event, controller)
            && let Some(mut viewport) = self.base().get_viewport()
        {
            viewport.set_input_as_handled();
        }
    }
    fn process(&mut self, _delta: f64) {
        if !self.live_source || !self.validate_input_view() {
            return;
        }
        if let Some(workspace) = &self.workspace {
            if let Some(player) = workspace.selected_player() {
                let player = player.bind();
                if let (Some(input), Some(controller)) = (&self.desktop_input, &player.controller) {
                    input.update_cursor(controller);
                }
            }
            return;
        }
        if let (Some(input), Some(controller)) = (&self.desktop_input, &self.controller) {
            input.update_cursor(controller);
        }
    }
    fn on_notification(&mut self, notification: NodeNotification) {
        if notification == NodeNotification::APPLICATION_PAUSED {
            self.stop();
            return;
        }
        if !self.live_source || !self.validate_input_view() {
            return;
        }
        if let Some(workspace) = &self.workspace {
            if let Some(player) = workspace.selected_player() {
                let player = player.bind();
                if let (Some(input), Some(controller)) =
                    (&mut self.desktop_input, &player.controller)
                {
                    match notification {
                        NodeNotification::WM_WINDOW_FOCUS_OUT
                        | NodeNotification::APPLICATION_FOCUS_OUT => {
                            input.focus_lost(Some(controller))
                        }
                        NodeNotification::WM_WINDOW_FOCUS_IN => {
                            input.reconcile_releases(controller)
                        }
                        NodeNotification::WM_MOUSE_EXIT => input.pointer_left(controller),
                        _ => {}
                    }
                }
            }
            return;
        }
        if let (Some(input), Some(controller)) = (&mut self.desktop_input, &self.controller) {
            match notification {
                NodeNotification::WM_WINDOW_FOCUS_OUT | NodeNotification::APPLICATION_FOCUS_OUT => {
                    input.focus_lost(Some(controller))
                }
                NodeNotification::WM_WINDOW_FOCUS_IN => input.reconcile_releases(controller),
                NodeNotification::WM_MOUSE_EXIT => input.pointer_left(controller),
                _ => {}
            }
        }
    }
    fn exit_tree(&mut self) {
        self.stop();
        self.controller.take();
    }
}
#[godot_api]
impl WeldVideoPlayer {
    /// Resolve headset preferences before opening a stream. Only owned numeric
    /// values reach the coordinator; poses and Godot objects stay here.
    #[func]
    fn configure_xr_presentation(
        &mut self,
        eye: Vector2,
        projections: Array<Projection>,
        envelope: Vector2,
        distance: f64,
        scale: f64,
        sampling: f64,
    ) -> bool {
        if self.controller.is_some() || self.workspace.is_some() || projections.len() > 2 {
            return false;
        }
        self.xr_preferences = XrPreferences::new(
            [f64::from(eye.x), f64::from(eye.y)],
            &projections.iter_shared().collect::<Vec<_>>(),
            [f64::from(envelope.x), f64::from(envelope.y)],
            distance,
            scale,
            sampling,
        );
        self.xr_preferences.is_some()
    }

    /// Synchronous layout on plain Controls; no deferred Container sorting.
    #[func]
    fn layout_video(&self, mut view: Gd<Control>, bounds: Vector2, fill: bool) {
        let size = if fill {
            Some([f64::from(bounds.x), f64::from(bounds.y)])
        } else {
            fit(
                [f64::from(bounds.x), f64::from(bounds.y)],
                f64::from(self.aspect()),
            )
        };
        if let Some(size) = size.filter(|size| size.iter().all(|v| v.is_finite() && *v > 0.0)) {
            let size = Vector2::new(size[0] as f32, size[1] as f32);
            view.set_position((bounds - size) * 0.5);
            view.set_size(size);
        }
    }

    #[func]
    fn xr_panel_size(&self, envelope: Vector2) -> Vector2 {
        fit(
            [f64::from(envelope.x), f64::from(envelope.y)],
            f64::from(self.aspect()),
        )
        .map_or(Vector2::ZERO, |size| {
            Vector2::new(size[0] as f32, size[1] as f32)
        })
    }

    #[func]
    fn xr_viewport_size(&mut self) -> Vector2i {
        if !self.controller.as_ref().is_some_and(Controller::has_frame) {
            return Vector2i::ZERO;
        }
        let Some(pixels) = self
            .xr_preferences
            .and_then(|prefs| prefs.pixels(f64::from(self.aspect())))
        else {
            return Vector2i::ZERO;
        };
        let pixels = self.raster_sizing.update(pixels, Instant::now());
        // Shared receive policy bounds both dimensions to 2048.
        Vector2i::new(pixels[0] as i32, pixels[1] as i32)
    }

    /// Connects this presenter to Rust-owned desktop input. Scenes provide only
    /// the image rectangle; XR and Android do not enable this input source.
    #[func]
    fn configure_input(&mut self, view: Gd<Control>, enabled: bool) {
        if let Some(input) = self.desktop_input.as_mut() {
            input.stop(self.controller.as_ref());
        }
        self.desktop_input = if enabled
            && !Engine::singleton().is_editor_hint()
            && Os::singleton().get_name() != "Android"
            && view.is_instance_valid()
        {
            Some(DesktopInput::new(view))
        } else {
            None
        };
        self.update_processing();
    }
    /// Rust retains the fresh ExternalTexture and material through GPU cleanup.
    /// Callers must not resize/replace that texture's storage during playback.
    #[func]
    fn start(&mut self, texture: Gd<Object>, material: Gd<Object>) -> bool {
        self.start_source(
            texture,
            material,
            Source::Fixture {
                single_frame: false,
            },
        )
    }
    /// Finite local stress fixture; never opens a transport or input route.
    #[func]
    fn start_stress(
        &mut self,
        texture: Gd<Object>,
        material: Gd<Object>,
        path: GString,
        seconds: i64,
        smooth: bool,
    ) -> bool {
        if !(1..=60).contains(&seconds) {
            return false;
        }
        self.start_source(
            texture,
            material,
            Source::Stress {
                path: PathBuf::from(path.to_string()),
                seconds: seconds as u64,
                smooth,
            },
        )
    }
    #[func]
    fn diagnostic_stats(&self) -> VarDictionary {
        let mut values = VarDictionary::new();
        if let Some(controller) = &self.controller {
            for (name, value) in controller.diagnostic_stats() {
                values.set(name, i64::try_from(value).unwrap_or(i64::MAX));
            }
        }
        values
    }
    /// Diagnostic: one AU, no future input and no EOS-assisted flush.
    #[func]
    fn start_single_frame(&mut self, texture: Gd<Object>, material: Gd<Object>) -> bool {
        self.start_source(texture, material, Source::Fixture { single_frame: true })
    }
    #[func]
    fn set_presenter_rate(&mut self, refresh_millihertz: i64) -> bool {
        let Some(rate) = presenter_rate(refresh_millihertz) else {
            return false;
        };
        self.workspace
            .as_ref()
            .is_some_and(|workspace| workspace.session.set_presentation_rate(rate))
    }
    #[func]
    fn start_stream(
        &mut self,
        texture: Gd<Object>,
        material: Gd<Object>,
        directory: GString,
        refresh_millihertz: i64,
    ) -> bool {
        let rate = presenter_rate(refresh_millihertz);
        let Some(rate) = rate else {
            self.message = "Invalid presenter refresh rate".into();
            return false;
        };
        self.stop();
        self.controller.take();
        self.workspace.take();
        match Session::start(
            texture,
            material,
            PathBuf::from(directory.to_string()),
            rate,
            self.xr_preferences,
        ) {
            Ok(session) => {
                self.workspace = Some(Workspace::new(session, self.xr_preferences));
                self.live_source = true;
                self.update_processing();
                true
            }
            Err(error) => {
                self.message = format!("Receiver unavailable: {error:#}");
                false
            }
        }
    }
    fn start_source(&mut self, texture: Gd<Object>, material: Gd<Object>, source: Source) -> bool {
        if let Some(controller) = self.controller.as_mut()
            && !controller.finished()
        {
            return false;
        }
        if let Some(input) = self.desktop_input.as_mut() {
            input.stop(self.controller.as_ref());
        }
        self.controller.take();
        self.live_source = false;
        self.update_processing();
        match Controller::start(texture, material, source) {
            Ok(controller) => {
                self.controller = Some(controller);
                self.live_source = false;
                self.update_processing();
                true
            }
            Err(error) => {
                self.message = format!("Video unavailable: {error:#}");
                godot_error!("{}", self.message);
                false
            }
        }
    }
    #[func]
    fn tick(&mut self) {
        if let Some(workspace) = self.workspace.as_mut()
            && let Err(error) = workspace.tick()
        {
            self.message = format!("Presentation failed: {error:#}");
            godot_error!("{}", self.message);
            self.stop();
        }
        if let Some(controller) = self.controller.as_mut() {
            controller.tick();
        }
        let children: Vec<_> = self.workspace.as_ref().map_or_else(Vec::new, |workspace| {
            workspace
                .panes
                .values()
                .map(|surface| surface.bind().player.clone())
                .collect()
        });
        for child in children {
            if child.is_instance_valid() && child.get_parent().is_none() {
                self.base_mut().add_child(&child);
            }
        }
    }
    #[func]
    fn stop(&mut self) {
        if let Some(workspace) = self.workspace.as_mut() {
            workspace.stop();
        }
        self.live_source = false;
        self.update_processing();
        if let Some(input) = self.desktop_input.as_mut() {
            input.stop(self.controller.as_ref());
        }
        if let Some(controller) = self.controller.as_mut() {
            controller.stop();
        }
    }
    #[func]
    fn status(&self) -> GString {
        if let Some(workspace) = &self.workspace {
            return workspace.session.status().as_str().into();
        }
        self.controller
            .as_ref()
            .map_or_else(|| self.message.clone(), Controller::status)
            .as_str()
            .into()
    }
    #[func]
    fn aspect(&self) -> f32 {
        self.view_layout.aspect(
            self.controller
                .as_ref()
                .map_or(16.0 / 9.0, Controller::aspect),
        )
    }
    #[func]
    fn surfaces(&self) -> Array<Gd<WeldSurface>> {
        self.workspace
            .as_ref()
            .map_or_else(Array::new, Workspace::surfaces)
    }
    #[func]
    fn sort_xr_windows(&mut self, viewer: Vector3, left_eye: Vector3, right_eye: Vector3) {
        if let Some(workspace) = &mut self.workspace {
            workspace.sort_xr_windows(viewer, [left_eye, right_eye]);
        }
    }
}

impl WeldVideoPlayer {
    fn for_surface(controller: Controller, preferences: Option<XrPreferences>) -> Gd<Self> {
        Gd::from_init_fn(|base| Self {
            base,
            controller: Some(controller),
            workspace: None,
            desktop_input: None,
            live_source: true,
            message: String::new(),
            xr_preferences: preferences,
            raster_sizing: RasterSizing::default(),
            shape: None,
            view_layout: ViewLayout::Mono,
        })
    }
    fn xr_targets(
        &self,
    ) -> Vec<(
        Gd<WeldVideoPlayer>,
        Gd<godot::classes::MeshInstance3D>,
        Gd<Control>,
    )> {
        self.workspace.as_ref().map_or_else(Vec::new, |workspace| {
            workspace
                .panes
                .values()
                .filter_map(|surface| {
                    let surface = surface.bind();
                    if !surface.is_mapped() {
                        return None;
                    }
                    Some((
                        surface.player.clone(),
                        surface.panel.clone()?,
                        surface.control.clone()?,
                    ))
                })
                .collect()
        })
    }
    // XR owns its physical actions, but uses the same playback mailbox. It
    // cannot compete with the desktop source or send input to a fixture.
    fn xr_controller(&self) -> Option<&Controller> {
        (self.live_source && self.desktop_input.is_none())
            .then_some(self.controller.as_ref())
            .flatten()
    }
    fn update_processing(&mut self) {
        let enabled = self.live_source && self.desktop_input.is_some();
        self.base_mut().set_process_input(enabled);
        self.base_mut().set_process(enabled);
    }
    fn validate_input_view(&mut self) -> bool {
        if self
            .desktop_input
            .as_ref()
            .is_some_and(|input| !input.is_valid())
        {
            if let Some(mut input) = self.desktop_input.take() {
                input.stop(self.controller.as_ref());
            }
            self.update_processing();
        }
        self.desktop_input.is_some()
    }
}

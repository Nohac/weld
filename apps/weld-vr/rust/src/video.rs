//! Main-thread Godot API shared by all native providers.
mod input;
mod xr;

use crate::playback::{Controller, Source};
use crate::presentation::{RasterSizing, XrPreferences, fit};
use godot::{
    classes::{Control, Engine, INode, InputEvent, Node, Object, Os, notify::NodeNotification},
    prelude::*,
};
use input::DesktopInput;
use std::path::PathBuf;
use std::time::Instant;
use weld_client::PresentationRate;

#[derive(GodotClass)]
#[class(base=Node)]
pub struct WeldVideoPlayer {
    controller: Option<Controller>,
    desktop_input: Option<DesktopInput>,
    live_source: bool,
    message: String,
    xr_preferences: Option<XrPreferences>,
    raster_sizing: RasterSizing,
    base: Base<Node>,
}
#[godot_api]
impl INode for WeldVideoPlayer {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            controller: None,
            desktop_input: None,
            live_source: false,
            message: "Native AV1 video fixture".into(),
            xr_preferences: None,
            raster_sizing: RasterSizing::default(),
        }
    }
    fn ready(&mut self) {
        self.update_processing();
    }
    fn input(&mut self, event: Gd<InputEvent>) {
        if !self.live_source || !self.validate_input_view() {
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
        if self.controller.is_some() || projections.len() > 2 {
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
    /// Diagnostic: one AU, no future input and no EOS-assisted flush.
    #[func]
    fn start_single_frame(&mut self, texture: Gd<Object>, material: Gd<Object>) -> bool {
        self.start_source(texture, material, Source::Fixture { single_frame: true })
    }
    #[func]
    fn start_stream(
        &mut self,
        texture: Gd<Object>,
        material: Gd<Object>,
        directory: GString,
        refresh_millihertz: i64,
    ) -> bool {
        let rate = u32::try_from(refresh_millihertz)
            .ok()
            .and_then(|rate| PresentationRate::try_from(rate).ok());
        let Some(rate) = rate else {
            self.message = "Invalid presenter refresh rate".into();
            return false;
        };
        self.start_source(
            texture,
            material,
            Source::Iroh {
                directory: PathBuf::from(directory.to_string()),
                rate,
                sizing: self.xr_preferences,
            },
        )
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
        let live_source = matches!(&source, Source::Iroh { .. });
        match Controller::start(texture, material, source) {
            Ok(controller) => {
                self.controller = Some(controller);
                self.live_source = live_source;
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
        if let Some(controller) = self.controller.as_mut() {
            controller.tick();
        }
    }
    #[func]
    fn stop(&mut self) {
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
        self.controller
            .as_ref()
            .map_or_else(|| self.message.clone(), Controller::status)
            .as_str()
            .into()
    }
    #[func]
    fn aspect(&self) -> f32 {
        self.controller
            .as_ref()
            .map_or(16.0 / 9.0, Controller::aspect)
    }
}

impl WeldVideoPlayer {
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

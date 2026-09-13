//! Main-thread Godot API shared by all native providers.
use crate::playback::{Controller, Source};
use godot::{
    classes::{INode, Node, Object},
    prelude::*,
};
use std::path::PathBuf;
use weld_client::PresentationRate;

#[derive(GodotClass)]
#[class(base=Node)]
pub struct WeldVideoPlayer {
    controller: Option<Controller>,
    message: String,
    base: Base<Node>,
}
#[godot_api]
impl INode for WeldVideoPlayer {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            controller: None,
            message: "Native AV1 video fixture".into(),
        }
    }
    fn exit_tree(&mut self) {
        self.stop();
        self.controller.take();
    }
}
#[godot_api]
impl WeldVideoPlayer {
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
            },
        )
    }
    fn start_source(&mut self, texture: Gd<Object>, material: Gd<Object>, source: Source) -> bool {
        if let Some(controller) = self.controller.as_mut()
            && !controller.finished()
        {
            return false;
        }
        self.controller.take();
        match Controller::start(texture, material, source) {
            Ok(controller) => {
                self.controller = Some(controller);
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

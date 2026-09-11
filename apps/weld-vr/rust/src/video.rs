//! Main-thread Godot API shared by all native providers.
use crate::playback::Controller;
use godot::{
    classes::{INode, Node, Object},
    prelude::*,
};

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
    /// Caller retains the fresh ExternalTexture identified by `texture`, without
    /// resizing/replacing its storage, until stop completes. The panel owns that
    /// resource; this bridge retains the material but receives only a GL texture ID.
    #[func]
    fn start(&mut self, texture: i64, material: Gd<Object>) -> bool {
        if let Some(controller) = self.controller.as_mut()
            && !controller.finished()
        {
            return false;
        }
        self.controller.take();
        match Controller::start(texture, material) {
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
}

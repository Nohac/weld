//! Godot shell bridge and shared GPU-native video fixture.

mod fixture;
mod native;
mod playback;
mod presentation;
mod video;

use godot::{
    classes::{INode, Node, notify::NodeNotification},
    prelude::{Base, ExtensionLibrary, GString, GodotClass, gdextension, godot_api, godot_print},
};

struct WeldVrExtension;

// SAFETY: gdext generates the entry point and registers this library's classes.
// Native video workers are owned by scene nodes and joined before node teardown
// completes; opening the editor does not start native work.
#[gdextension]
unsafe impl ExtensionLibrary for WeldVrExtension {}

/// A main-thread bridge proving that a Godot button can invoke Rust on-device.
#[derive(GodotClass)]
#[class(base=Node)]
struct WeldBridge {
    calls: u32,
    base: Base<Node>,
}

#[godot_api]
impl INode for WeldBridge {
    fn init(base: Base<Node>) -> Self {
        Self { calls: 0, base }
    }

    fn ready(&mut self) {
        godot_print!("weld-vr: Rust bridge ready ({})", std::env::consts::ARCH);
    }

    fn exit_tree(&mut self) {
        godot_print!("weld-vr: Rust bridge leaving scene");
    }

    fn on_notification(&mut self, notification: NodeNotification) {
        match notification {
            NodeNotification::APPLICATION_PAUSED => godot_print!("weld-vr: application paused"),
            NodeNotification::APPLICATION_RESUMED => godot_print!("weld-vr: application resumed"),
            _ => {}
        }
    }
}

#[godot_api]
impl WeldBridge {
    /// Returns a visible response for each button press, without per-frame work.
    #[func]
    fn ping(&mut self) -> GString {
        self.calls = self.calls.saturating_add(1);
        godot_print!("weld-vr: Rust button response {}", self.calls);
        GString::from(&format!("Hello from Rust!\nTap count: {}", self.calls))
    }
}

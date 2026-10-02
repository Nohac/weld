//! Godot shell bridge and shared GPU-native video fixture.

mod diagnostics;
mod fixture;
mod native;
mod playback;
mod presentation;
mod presentation_rules;
mod video;

use godot::prelude::{ExtensionLibrary, gdextension};

struct WeldVrExtension;

// SAFETY: gdext generates the entry point and registers this library's classes.
// Native video workers are owned by scene nodes and joined before node teardown
// completes; opening the editor does not start native work.
#[gdextension]
unsafe impl ExtensionLibrary for WeldVrExtension {
    fn on_stage_init(level: godot::init::InitStage) {
        if level == godot::init::InitStage::Scene {
            diagnostics::init();
        }
    }
    fn on_main_loop_frame() {
        diagnostics::drain();
    }
}

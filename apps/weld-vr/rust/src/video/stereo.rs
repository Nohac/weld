//! Packed stereo interpretation. Both eyes sample one decoded image; this
//! module introduces no second decoder, media queue, or frame-selection step.
use super::native_canvas::NativeCanvas;
use godot::{
    classes::{
        ColorRect, Control, INode3D, Node3D, OpenXrCompositionLayerQuad, ShaderMaterial,
        SubViewport, open_xr_composition_layer::EyeVisibility, sub_viewport::UpdateMode,
    },
    prelude::*,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ViewLayout {
    #[default]
    Mono,
    SideBySide,
}

/// Two native eye layers with one transform and one source-frame owner.
/// The scene retains its ordinary mesh for picking, controls and decorations.
#[derive(GodotClass)]
#[class(base=Node3D)]
pub(super) struct WeldStereoPanel {
    layers: Vec<Gd<OpenXrCompositionLayerQuad>>,
    canvases: Vec<NativeCanvas>,
    right_viewport: Option<Gd<SubViewport>>,
    right_video: Option<Gd<ColorRect>>,
    base: Base<Node3D>,
}

#[godot_api]
impl INode3D for WeldStereoPanel {
    fn init(base: Base<Node3D>) -> Self {
        Self {
            layers: Vec::new(),
            canvases: Vec::new(),
            right_viewport: None,
            right_video: None,
            base,
        }
    }

    fn exit_tree(&mut self) {
        for layer in &mut self.layers {
            layer.set_layer_viewport(None::<&Gd<SubViewport>>);
        }
        for canvas in &mut self.canvases {
            canvas.detach();
        }
    }
}

#[godot_api]
impl WeldStereoPanel {
    #[func]
    fn initialize(&mut self, left: Gd<SubViewport>, right: Gd<ShaderMaterial>) -> bool {
        if !self.layers.is_empty() || self.right_viewport.is_some() {
            return false;
        }
        let mut viewport = SubViewport::new_alloc();
        viewport.set_disable_3d(true);
        viewport.set_transparent_background(true);
        viewport.set_size(left.get_size());
        viewport.set_update_mode(UpdateMode::ALWAYS);
        self.base_mut().add_child(&viewport);
        let mut video = ColorRect::new_alloc();
        video.set_mouse_filter(godot::classes::control::MouseFilter::IGNORE);
        video.set_material(&right);
        video.set_size(left.get_size().to_vector2());
        viewport.add_child(&video);
        self.right_viewport = Some(viewport.clone());
        self.right_video = Some(video);
        for (eye, source) in [
            (EyeVisibility::LEFT, left),
            (EyeVisibility::RIGHT, viewport),
        ] {
            let mut layer = OpenXrCompositionLayerQuad::new_alloc();
            layer.hide();
            self.base_mut().add_child(&layer);
            layer.set_eye_visibility(eye);
            layer.set_alpha_blend(true);
            layer.set_sort_order(100);
            layer.set_process_priority(150);
            // Overlay the complete 3D projection. Underlays need rectangular
            // holes, which erase scenery beneath translucent window shadows.
            layer.set_enable_hole_punch(false);
            if !layer.is_natively_supported() {
                layer.queue_free();
                for layer in &mut self.layers {
                    layer.set_layer_viewport(None::<&Gd<SubViewport>>);
                }
                godot_error!("Stereo panel requires native OpenXR quad layers");
                return false;
            }
            let Some(canvas) = NativeCanvas::new(source, &mut self.base_mut()) else {
                layer.queue_free();
                return false;
            };
            layer.set_layer_viewport(&canvas.target);
            self.canvases.push(canvas);
            self.layers.push(layer);
        }
        godot_print!("WELD_XR_STEREO native eye layers ready; one decoded image");
        true
    }

    #[func]
    fn sync_panel(&mut self, world: Transform3D, physical: Vector2, raster: Vector2i, order: i32) {
        if !world.is_finite()
            || !physical.is_finite()
            || physical.x <= 0.0
            || physical.y <= 0.0
            || !(1..=4096).contains(&raster.x)
            || !(1..=4096).contains(&raster.y)
        {
            return;
        }
        if let Some(viewport) = &mut self.right_viewport
            && viewport.get_size() != raster
        {
            viewport.set_size(raster);
        }
        for canvas in &mut self.canvases {
            canvas.resize(raster);
        }
        for layer in &mut self.layers {
            if layer.get_global_transform() != world {
                layer.set_global_transform(world);
            }
            if layer.get_quad_size() != physical {
                layer.set_quad_size(physical);
            }
            let native_order = order + 200;
            if layer.get_sort_order() != native_order {
                layer.set_sort_order(native_order);
            }
        }
    }

    #[func]
    fn right_eye_control(&self) -> Option<Gd<ColorRect>> {
        self.right_video.clone()
    }

    #[func]
    fn output_control(&self, right: bool) -> Option<Gd<Control>> {
        self.canvases
            .get(usize::from(right))
            .map(|canvas| canvas.view.clone())
    }

    #[func]
    fn show_panel(&mut self, shown: bool) {
        if self.layers.len() != 2 {
            return;
        }
        for layer in &mut self.layers {
            if layer.is_visible() != shown {
                layer.set_visible(shown);
            }
        }
    }
}

impl ViewLayout {
    pub fn eye_view(self, right: bool) -> Vector4 {
        match (self, right) {
            (Self::Mono, _) => Vector4::new(0.0, 0.0, 1.0, 1.0),
            (Self::SideBySide, false) => Vector4::new(0.0, 0.0, 0.5, 1.0),
            (Self::SideBySide, true) => Vector4::new(0.5, 0.0, 1.0, 1.0),
        }
    }

    pub fn aspect(self, packed: f32) -> f32 {
        match self {
            Self::Mono => packed,
            Self::SideBySide => packed * 0.5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_width_stereo_keeps_each_eye_aspect_and_nonoverlapping_views() {
        let layout = ViewLayout::SideBySide;
        assert!((layout.aspect(1600.0 / 480.0) - 800.0 / 480.0).abs() < 1e-6);
        let left = layout.eye_view(false);
        let right = layout.eye_view(true);
        assert_eq!(left.z, right.x);
        assert_eq!(left.x, 0.0);
        assert_eq!(right.z, 1.0);
        assert_eq!(
            ViewLayout::Mono.eye_view(false),
            ViewLayout::Mono.eye_view(true)
        );
        assert_eq!(ViewLayout::Mono.aspect(4.0 / 3.0), 4.0 / 3.0);
    }
}

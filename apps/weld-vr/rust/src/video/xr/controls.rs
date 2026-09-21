//! Local window chrome; no video texture, decoder or remote input ownership.
use super::geometry;
use crate::video::canvas::{self, Layout};
use crate::video::sizing::{Corner, RESIZE_HANDLE_GAP};
use godot::{
    classes::{ColorRect, Control, MeshInstance3D, QuadMesh, Shader, ShaderMaterial},
    prelude::*,
};

pub(crate) const SIZE: Vector2 = Vector2::new(0.46, 0.05);

pub(super) fn near_edge(uv: Vector2, size: Vector2) -> bool {
    uv.is_finite()
        && size.is_finite()
        && size.x > 0.0
        && size.y > 0.0
        && (uv.x - 0.5).abs() * size.x <= size.x * 0.5 + 0.025
        && (uv.y.abs().min((1.0 - uv.y).abs()) * size.y) <= 0.12_f32.min(size.y * 0.3)
}

pub(super) fn edge_opacity(uv: Vector2, size: Vector2) -> f32 {
    if !near_edge(uv, size) {
        return 0.0;
    }
    // Fade across the approach band inside the window. Across the small gap
    // outside its edge, retain full opacity so the controls stay reachable.
    let inside = uv.y.min(1.0 - uv.y).max(0.0) * size.y;
    let progress = (1.0 - inside / 0.12_f32.min(size.y * 0.3)).clamp(0.0, 1.0);
    progress * progress * (3.0 - 2.0 * progress)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    Drag,
    Close,
    Gamepad,
    SmallerUi,
    LargerUi,
    Resize(Corner),
}

pub(in crate::video) fn corner_at(uv: Vector2, size: Vector2) -> Option<Corner> {
    if !uv.is_finite() || !size.is_finite() || size.x <= 0.0 || size.y <= 0.0 {
        return None;
    }
    let point = (uv - Vector2::splat(0.5)) * size;
    // Float outside the content; generous hit targets must not steal clicks
    // from the application's own corner controls.
    let outside = point.abs() - size * 0.5;
    if outside.x <= 0.0 && outside.y <= 0.0 {
        return None;
    }
    let distance = (outside - Vector2::splat(RESIZE_HANDLE_GAP)).abs();
    if distance.x > 0.035 || distance.y > 0.035 {
        return None;
    }
    Some(match (uv.x < 0.5, uv.y < 0.5) {
        (true, true) => Corner::TopLeft,
        (false, true) => Corner::TopRight,
        (true, false) => Corner::BottomLeft,
        (false, false) => Corner::BottomRight,
    })
}

fn part_at(point: Vector2) -> Option<Part> {
    if !point.is_finite() {
        return None;
    }
    // Match the shader's slim pill; the visible close region is on the left.
    let radius = 0.0225;
    let q = point.abs() - (SIZE * 0.5 - Vector2::splat(radius));
    if Vector2::new(q.x.max(0.0), q.y.max(0.0)).length() + q.x.max(q.y).min(0.0) > radius {
        return None;
    }
    Some(if point.x < -0.15 {
        Part::Close
    } else if point.x < -0.075 {
        Part::SmallerUi
    } else if point.x < 0.055 {
        Part::Drag
    } else if point.x < 0.13 {
        Part::LargerUi
    } else {
        Part::Gamepad
    })
}

pub(crate) struct Bar {
    mesh: Gd<MeshInstance3D>,
    material: Gd<ShaderMaterial>,
    top: bool,
    views: Vec<Gd<ColorRect>>,
    layout: Option<Layout>,
}
impl Bar {
    pub fn new(mut parent: Gd<MeshInstance3D>, views: &[Gd<Control>]) -> Self {
        let mut shader = Shader::new_gd();
        shader.set_code(include_str!("controls.gdshader"));
        let mut material = ShaderMaterial::new_gd();
        material.set_shader(&shader);
        material.set_shader_parameter("opacity", &0.0_f32.to_variant());
        let mut quad = QuadMesh::new_gd();
        quad.set_size(SIZE);
        let mut mesh = MeshInstance3D::new_alloc();
        mesh.set_name("WindowControls");
        mesh.set_mesh(&quad);
        // World-space hit proxy only; both eyes draw the window-owned canvas.
        mesh.set_layer_mask(0);
        mesh.hide();
        parent.add_child(&mesh);
        Self {
            mesh,
            top: false,
            views: views
                .iter()
                .filter_map(|view| canvas::add_overlay(view, &material, 20))
                .collect(),
            material,
            layout: None,
        }
    }
    pub fn place(&mut self, height: f32) {
        let y = (height * 0.5 + SIZE.y * 0.5 + 0.025) * if self.top { 1.0 } else { -1.0 };
        self.mesh.set_position(Vector3::new(0.0, y, 0.0));
        self.sync_canvas();
    }
    pub fn layout(&mut self, layout: Layout) {
        self.layout = Some(layout);
        self.sync_canvas();
    }
    fn sync_canvas(&mut self) {
        let Some(layout) = self.layout else {
            return;
        };
        let rect = layout.rectangle(Vector2::new(0.0, -self.mesh.get_position().y), SIZE);
        for view in &mut self.views {
            if view.is_instance_valid() {
                view.set_position(rect.position);
                view.set_size(rect.size);
            }
        }
    }
    pub fn show(&mut self, opacity: f32) {
        let shown = opacity > 0.0;
        if self.mesh.is_instance_valid() && self.mesh.is_visible() != shown {
            self.mesh.set_visible(shown);
        }
        self.material
            .set_shader_parameter("opacity", &opacity.to_variant());
    }
    pub fn approach(&mut self, uv_y: f32, height: f32) {
        if uv_y < 0.35 {
            self.top = true;
        } else if uv_y > 0.65 {
            self.top = false;
        }
        self.place(height);
    }
    pub fn hit(&self, aim: Transform3D) -> Option<(Part, f32)> {
        let hit =
            geometry::Panel::new(self.mesh.get_global_transform(), Vector3::ZERO, SIZE, SIZE)?
                .project(aim)?;
        if !hit.inside {
            return None;
        }
        Some((part_at(hit.pixels - SIZE * 0.5)?, hit.distance))
    }
    pub fn highlight(&mut self, part: Option<Part>) {
        self.material.set_shader_parameter(
            "hovered",
            &match part {
                None => 0,
                Some(Part::Drag) => 1,
                Some(Part::Close) => 2,
                Some(Part::Gamepad) => 3,
                Some(Part::SmallerUi) => 4,
                Some(Part::LargerUi) => 5,
                Some(Part::Resize(_)) => 0,
            }
            .to_variant(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corner_hit_regions_are_larger_than_strokes_but_exclude_window_interior() {
        let size = Vector2::new(1.6, 1.0);
        let gap = Vector2::splat(RESIZE_HANDLE_GAP) / size;
        assert_eq!(corner_at(-gap, size), Some(Corner::TopLeft));
        assert_eq!(
            corner_at(Vector2::new(1.0 + gap.x, -gap.y), size),
            Some(Corner::TopRight)
        );
        assert_eq!(
            corner_at(Vector2::ONE + gap, size),
            Some(Corner::BottomRight)
        );
        assert_eq!(
            corner_at(Vector2::new(-gap.x, 1.0 + gap.y), size),
            Some(Corner::BottomLeft)
        );
        assert!(corner_at(Vector2::new(0.005, -0.02), size).is_some());
        assert_eq!(corner_at(Vector2::ZERO, size), None);
        assert_eq!(corner_at(Vector2::new(0.01, 0.01), size), None);
        assert_eq!(corner_at(Vector2::new(0.5, 0.0), size), None);
        assert_eq!(corner_at(Vector2::new(f32::NAN, 0.0), size), None);
    }
    #[test]
    fn slim_controls_hit_close_on_left_and_handle_on_right() {
        assert_eq!(part_at(Vector2::new(-0.19, 0.0)), Some(Part::Close));
        assert_eq!(part_at(Vector2::new(0.045, 0.0)), Some(Part::Drag));
        assert_eq!(part_at(Vector2::new(0.18, 0.0)), Some(Part::Gamepad));
        assert_eq!(part_at(Vector2::new(-0.11, 0.0)), Some(Part::SmallerUi));
        assert_eq!(part_at(Vector2::new(0.09, 0.0)), Some(Part::LargerUi));
        assert_eq!(part_at(Vector2::new(0.045, 0.035)), None);
        assert_eq!(part_at(SIZE * 0.5), None);
    }
    #[test]
    fn opacity_increases_smoothly_to_full_at_either_window_edge() {
        let size = Vector2::new(1.6, 1.0);
        for (distance, expected) in [
            (0.5, 0.0),
            (0.12, 0.0),
            (0.06, 0.5),
            (0.0, 1.0),
            (-0.06, 1.0),
        ] {
            for y in [distance, 1.0 - distance] {
                assert!((edge_opacity(Vector2::new(0.5, y), size) - expected).abs() < 1e-5);
            }
        }
        assert_eq!(edge_opacity(Vector2::new(f32::NAN, 0.0), size), 0.0);
    }
    #[test]
    fn controls_reveal_only_near_edges_including_gap_to_strip() {
        let size = Vector2::new(1.6, 1.0);
        for uv in [
            Vector2::new(0.5, 0.05),
            Vector2::new(0.5, 0.95),
            Vector2::new(0.5, -0.08),
        ] {
            assert!(near_edge(uv, size));
        }
        for uv in [
            Vector2::new(0.5, 0.5),
            Vector2::new(1.2, 0.05),
            Vector2::new(0.5, -0.3),
        ] {
            assert!(!near_edge(uv, size));
        }
    }
}

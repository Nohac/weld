//! Local window chrome; no video texture, decoder or remote input ownership.
use super::geometry;
use godot::{
    classes::{MeshInstance3D, QuadMesh, Shader, ShaderMaterial},
    prelude::*,
};

pub(crate) const SIZE: Vector2 = Vector2::new(0.34, 0.05);

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
    Some(if point.x < -0.06 {
        Part::Close
    } else {
        Part::Drag
    })
}

pub(crate) struct Bar {
    mesh: Gd<MeshInstance3D>,
    material: Gd<ShaderMaterial>,
    top: bool,
}
impl Bar {
    pub fn new(mut parent: Gd<MeshInstance3D>) -> Self {
        let mut shader = Shader::new_gd();
        shader.set_code(include_str!("controls.gdshader"));
        let mut material = ShaderMaterial::new_gd();
        material.set_shader(&shader);
        let mut quad = QuadMesh::new_gd();
        quad.set_size(SIZE);
        let mut mesh = MeshInstance3D::new_alloc();
        mesh.set_name("WindowControls");
        mesh.set_mesh(&quad);
        mesh.set_material_override(&material);
        mesh.hide();
        parent.add_child(&mesh);
        Self {
            mesh,
            material,
            top: false,
        }
    }
    pub fn place(&mut self, height: f32) {
        let y = (height * 0.5 + SIZE.y * 0.5 + 0.025) * if self.top { 1.0 } else { -1.0 };
        self.mesh.set_position(Vector3::new(0.0, y, 0.006));
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
            }
            .to_variant(),
        );
    }
}

/// Offset in the layout parent's space, so attached menus still follow a moved window.
#[derive(Default)]
pub(crate) struct Placement {
    base: Transform3D,
    offset: Transform3D,
}
impl Placement {
    pub fn apply(&mut self, base: Transform3D) -> Transform3D {
        self.base = base;
        base * self.offset
    }
    pub fn move_to(&mut self, world: Transform3D) {
        if world.is_finite() && self.base.is_finite() && self.base.basis.determinant().abs() > 1e-6
        {
            self.offset = self.base.affine_inverse() * world;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slim_controls_hit_close_on_left_and_handle_on_right() {
        assert_eq!(part_at(Vector2::new(-0.115, 0.0)), Some(Part::Close));
        assert_eq!(part_at(Vector2::new(0.045, 0.0)), Some(Part::Drag));
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
    #[test]
    fn placement_retains_user_offset_without_accumulating_and_follows_parent() {
        let mut placement = Placement::default();
        let base = Transform3D::new(Basis::IDENTITY, Vector3::new(0.0, 1.0, -1.6));
        assert_eq!(placement.apply(base), base);
        let moved = Transform3D::new(Basis::IDENTITY, base.origin + Vector3::RIGHT);
        placement.move_to(moved);
        for _ in 0..5 {
            assert_eq!(placement.apply(base), moved);
        }
        let new_base = Transform3D::new(Basis::IDENTITY, base.origin + Vector3::UP);
        assert_eq!(placement.apply(new_base).origin, moved.origin + Vector3::UP);
    }
}

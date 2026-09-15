//! Presentation-only rounded outline and soft shadow. Native video stays on
//! its existing quad; decoration never expands its input or decoder extent.
use godot::{
    classes::{MeshInstance3D, QuadMesh, Shader, ShaderMaterial},
    prelude::*,
};
use weld_client::InputPosition;

const MARGIN: f32 = 0.06;

#[derive(Clone, Copy)]
pub(super) struct Shape {
    pub size: Vector2,
    pub radius: f32,
}
impl Shape {
    pub fn new(size: Vector2) -> Option<Self> {
        (size.is_finite() && size.x > 0.0 && size.y > 0.0 && size.x <= 10.0 && size.y <= 10.0)
            .then_some(Self {
                size,
                radius: 0.012_f32.min(size.x.min(size.y) * 0.04),
            })
    }
    pub fn hit(self, rectangle: [f64; 4], position: InputPosition) -> bool {
        let [x, y, width, height] = rectangle;
        if !rectangle
            .into_iter()
            .chain([position.x, position.y])
            .all(f64::is_finite)
            || width <= 0.0
            || height <= 0.0
        {
            return false;
        }
        let uv = [(position.x - x) / width, (position.y - y) / height];
        if uv.iter().any(|v| !(0.0..=1.0).contains(v)) {
            return false;
        }
        let radius = f64::from(self.radius);
        let q = [
            (uv[0] - 0.5).abs() * f64::from(self.size.x) - (f64::from(self.size.x) * 0.5 - radius),
            (uv[1] - 0.5).abs() * f64::from(self.size.y) - (f64::from(self.size.y) * 0.5 - radius),
        ];
        q[0].max(0.0).hypot(q[1].max(0.0)) + q[0].max(q[1]).min(0.0) <= radius
    }
}

pub(super) struct Decoration {
    mesh: Gd<MeshInstance3D>,
    material: Gd<ShaderMaterial>,
    size: Vector2,
    focused: bool,
}
impl Decoration {
    pub fn new(mut parent: Gd<MeshInstance3D>) -> Self {
        let mut shader = Shader::new_gd();
        shader.set_code(include_str!("frame.gdshader"));
        let mut material = ShaderMaterial::new_gd();
        material.set_shader(&shader);
        material.set_shader_parameter("margin", &MARGIN.to_variant());
        let mut mesh = MeshInstance3D::new_alloc();
        mesh.set_name("WindowOutline");
        mesh.set_mesh(&QuadMesh::new_gd());
        mesh.set_material_override(&material);
        // Just in front of the content's hole-punch plane, so the outline also
        // survives at rounded corners. Ordinary depth testing keeps hands ahead.
        mesh.set_position(Vector3::new(0.0, 0.0, 0.001));
        parent.add_child(&mesh);
        Self {
            mesh,
            material,
            size: Vector2::ZERO,
            focused: false,
        }
    }
    pub fn set_focused(&mut self, focused: bool) {
        if self.focused != focused {
            self.focused = focused;
            self.material
                .set_shader_parameter("focused", &focused.to_variant());
        }
    }
    pub fn update(&mut self, shape: Shape, video: &mut Gd<ShaderMaterial>) {
        if self.size == shape.size || !self.mesh.is_instance_valid() {
            return;
        }
        self.size = shape.size;
        if let Some(mut mesh) = self
            .mesh
            .get_mesh()
            .and_then(|mesh| mesh.try_cast::<QuadMesh>().ok())
        {
            mesh.set_size(shape.size + Vector2::splat(2.0 * MARGIN));
        }
        self.material
            .set_shader_parameter("window_size", &shape.size.to_variant());
        self.material
            .set_shader_parameter("corner_radius", &shape.radius.to_variant());
        video.set_shader_parameter("window_size", &shape.size.to_variant());
        video.set_shader_parameter("corner_radius", &shape.radius.to_variant());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rounded_hit_excludes_corners_and_shadow_but_preserves_content_coordinates() {
        let shape = Shape::new(Vector2::new(1.6, 0.9)).expect("shape");
        let rectangle = [10.0, 20.0, 1600.0, 900.0];
        assert!(shape.hit(rectangle, InputPosition::new(810.0, 470.0)));
        assert!(shape.hit(rectangle, InputPosition::new(810.0, 20.0)));
        assert!(!shape.hit(rectangle, InputPosition::new(10.0, 20.0)));
        assert!(!shape.hit(rectangle, InputPosition::new(5.0, 470.0)));
        assert!(!shape.hit([0.0; 4], InputPosition::default()));
        assert!(Shape::new(Vector2::new(f32::NAN, 1.0)).is_none());
    }
}

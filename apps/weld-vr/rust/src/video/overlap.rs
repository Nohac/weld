//! Crossfade two whole-window painter orders without changing their coverage.
//! Only the front family gets a final canvas pass. Its RGB approaches the back
//! family's projected RGB, but its alpha stays intact, including chrome. This
//! is equivalent to blending A-over-B with B-over-A, not fading A into the room.
use godot::{
    classes::{Control, Material, Shader, ShaderMaterial, Viewport},
    prelude::*,
};

const MAX_BACK_LAYERS: usize = 8;

#[derive(Clone, Copy)]
pub(super) struct Plane {
    pub world: Transform3D,
    pub size: Vector2,
}

/// UV-to-UV projective mapping through one eye. The positive denominator also
/// rejects intersections behind the eye; no screen-space monocular mask is used.
fn projection(front: Plane, back: Plane, eye: Vector3) -> Option<Projection> {
    if !front.world.is_finite()
        || !back.world.is_finite()
        || !eye.is_finite()
        || !front.size.is_finite()
        || !back.size.is_finite()
        || front.size.x <= 0.0
        || front.size.y <= 0.0
        || back.size.x <= 0.0
        || back.size.y <= 0.0
        || back.world.basis.determinant().abs() < 1e-6
    {
        return None;
    }
    let inverse = back.world.affine_inverse();
    let eye = inverse * eye;
    if eye.z.abs() < 1e-5 {
        return None;
    }
    let relative = inverse * front.world;
    let origin = relative * Vector3::new(-front.size.x * 0.5, front.size.y * 0.5, 0.0);
    let horizontal = relative.basis * Vector3::new(front.size.x, 0.0, 0.0);
    let vertical = relative.basis * Vector3::new(0.0, -front.size.y, 0.0);
    let column = |point: Vector3, constant: bool| {
        let denominator = point.z - if constant { eye.z } else { 0.0 };
        Vector4::new(
            (eye.x * point.z - eye.z * point.x) / back.size.x + 0.5 * denominator,
            -(eye.y * point.z - eye.z * point.y) / back.size.y + 0.5 * denominator,
            0.0,
            denominator,
        ) * -eye.z.signum()
    };
    Some(Projection::from_cols(
        column(horizontal, false),
        column(vertical, false),
        Vector4::ZERO,
        column(origin, true),
    ))
}

fn overlaps(mapping: Projection) -> bool {
    let mut minimum = Vector2::splat(f32::INFINITY);
    let mut maximum = Vector2::splat(f32::NEG_INFINITY);
    for uv in [Vector2::ZERO, Vector2::RIGHT, Vector2::ONE, Vector2::DOWN] {
        let point = mapping * Vector4::new(uv.x, uv.y, 0.0, 1.0);
        // Conservatively retain quads crossing the projection horizon.
        if !point.is_finite() || point.w <= 1e-5 {
            return true;
        }
        let uv = Vector2::new(point.x, point.y) / point.w;
        minimum = Vector2::new(minimum.x.min(uv.x), minimum.y.min(uv.y));
        maximum = Vector2::new(maximum.x.max(uv.x), maximum.y.max(uv.y));
    }
    maximum.x > 0.0 && maximum.y > 0.0 && minimum.x < 1.0 && minimum.y < 1.0
}

pub(super) struct Canvas {
    pub plane: Plane,
    pub eyes: [Gd<Viewport>; 2],
}

pub(super) struct Pass {
    view: Gd<Control>,
    ordinary: Gd<Material>,
    material: Gd<ShaderMaterial>,
    active: bool,
}
impl Pass {
    pub fn new(source: &Gd<Control>, output: &Gd<Control>) -> Option<Self> {
        let ordinary = output.get_material()?;
        let image = source.get_viewport()?.get_texture()?;
        let mut shader = Shader::new_gd();
        shader.set_code(include_str!("../../../shaders/overlap.gdshader"));
        let mut material = ShaderMaterial::new_gd();
        material.set_shader(&shader);
        material.set_shader_parameter("canvas_image", &image.to_variant());
        Some(Self {
            view: output.clone(),
            ordinary,
            material,
            active: false,
        })
    }
    pub fn clear(&mut self) {
        if self.active && self.view.is_instance_valid() {
            self.view.set_material(&self.ordinary);
            // Remove dependencies too: next frame either family may be behind.
            for index in 0..MAX_BACK_LAYERS {
                self.material
                    .set_shader_parameter(&format!("peer_{index}"), &Variant::nil());
            }
        }
        self.active = false;
    }
    pub fn apply(
        &mut self,
        front: Plane,
        back: &[Canvas],
        eye: Vector3,
        eye_index: usize,
        amount: f32,
    ) -> bool {
        if back.is_empty() || back.len() > MAX_BACK_LAYERS || !self.view.is_instance_valid() {
            return false;
        }
        let Some(inputs) = back
            .iter()
            .map(|canvas| {
                Some((
                    projection(front, canvas.plane, eye)?,
                    canvas.eyes[eye_index].get_texture()?,
                ))
            })
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        let inputs: Vec<_> = inputs
            .into_iter()
            .filter(|(map, _)| overlaps(*map))
            .collect();
        if inputs.is_empty() {
            return true;
        }
        let count = inputs.len() as i32;
        for (index, (map, texture)) in inputs.into_iter().enumerate() {
            self.material
                .set_shader_parameter(&format!("peer_{index}"), &texture.to_variant());
            self.material
                .set_shader_parameter(&format!("map_{index}"), &map.to_variant());
        }
        self.material
            .set_shader_parameter("count", &count.to_variant());
        self.material
            .set_shader_parameter("amount", &amount.to_variant());
        self.view.set_material(&self.material);
        self.active = true;
        true
    }
}
impl Drop for Pass {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plane(position: Vector3) -> Plane {
        Plane {
            world: Transform3D::new(Basis::IDENTITY, position),
            size: Vector2::splat(2.0),
        }
    }
    fn uv(map: Projection, point: Vector2) -> Vector2 {
        let projected = map * Vector4::new(point.x, point.y, 0.0, 1.0);
        assert!(projected.w > 0.0);
        Vector2::new(projected.x, projected.y) / projected.w
    }
    #[test]
    fn projection_preserves_coincident_planes_and_tracks_eye_parallax() {
        let front = plane(Vector3::new(0.0, 0.0, -2.0));
        for eye_x in [-0.032, 0.032] {
            let eye = Vector3::new(eye_x, 0.0, 0.0);
            let map = projection(front, front, eye).unwrap();
            for point in [Vector2::ZERO, Vector2::ONE, Vector2::new(0.2, 0.7)] {
                assert!((uv(map, point) - point).length() < 1e-5);
            }
            let back = plane(Vector3::new(0.0, 0.0, -3.0));
            let point = uv(projection(front, back, eye).unwrap(), Vector2::splat(0.5));
            assert!((point.x - (0.5 - eye_x * 0.25)).abs() < 1e-5);
        }
        assert!(projection(front, front, front.world.origin).is_none());
        assert!(overlaps(projection(front, front, Vector3::ZERO).unwrap()));
        assert!(!overlaps(
            projection(front, plane(Vector3::new(4.0, 0.0, -2.0)), Vector3::ZERO).unwrap()
        ));
    }
    #[test]
    fn preserved_alpha_blend_equals_crossfading_complete_stack_orders() {
        let over = |a: Vector4, b: Vector4| a + b * (1.0 - a.w);
        for alpha in [0.0, 0.2, 1.0] {
            for beta in [0.0, 0.4, 1.0] {
                let a = Vector4::new(0.8, 0.1, 0.0, 1.0) * alpha;
                let b = Vector4::new(0.0, 0.2, 0.7, 1.0) * beta;
                for t in [0.0, 0.5, 1.0] {
                    let mut adjusted = a * (1.0 - beta * t) + b * (alpha * t);
                    adjusted.w = alpha;
                    let expected = over(a, b) * (1.0 - t) + over(b, a) * t;
                    assert!((over(adjusted, b) - expected).length() < 1e-6);
                }
            }
        }
    }
}

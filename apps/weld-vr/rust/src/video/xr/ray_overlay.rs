//! The same world-space laser in each native eye canvas. No extra XR layers,
//! render targets, readback, or decoder work; the output pass preserves alpha.
use godot::{
    classes::{Control, ShaderMaterial},
    prelude::*,
};

#[derive(Clone, Copy)]
pub(in crate::video) struct Ray {
    pub start: Vector3,
    pub end: Vector3,
    pub hit: bool,
}

pub(in crate::video) fn apply(
    outputs: &[Gd<Control>],
    world: Transform3D,
    size: Vector2,
    eyes: Option<[Vector3; 2]>,
    ray: Option<Ray>,
) {
    let projected = ray.zip(eyes).filter(|(ray, eyes)| {
        world.is_finite()
            && world.basis.determinant().abs() > 1e-6
            && size.is_finite()
            && ray.start.is_finite()
            && ray.end.is_finite()
            && eyes.iter().all(|eye| eye.is_finite())
    });
    for (index, output) in outputs.iter().enumerate() {
        if !output.is_instance_valid() {
            continue;
        }
        let Some(mut material) = output
            .get_material()
            .and_then(|material| material.try_cast::<ShaderMaterial>().ok())
        else {
            continue;
        };
        let local = projected.and_then(|(ray, eyes)| {
            let inverse = world.affine_inverse();
            Some((ray, inverse, *eyes.get(index)?))
        });
        material.set_shader_parameter("pointer_visible", &local.is_some().to_variant());
        if let Some((ray, inverse, eye)) = local {
            material.set_shader_parameter("pointer_hit", &ray.hit.to_variant());
            material.set_shader_parameter("pointer_canvas_size", &size.to_variant());
            material.set_shader_parameter("pointer_eye", &(inverse * eye).to_variant());
            material.set_shader_parameter("pointer_start", &(inverse * ray.start).to_variant());
            material.set_shader_parameter("pointer_end", &(inverse * ray.end).to_variant());
        }
    }
}

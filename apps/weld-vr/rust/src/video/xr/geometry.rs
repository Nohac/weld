//! One front-facing QuadMesh plane, including its center offset. Projection
//! remains affine outside the quad so the shared pointer capture can drag there.
use godot::prelude::*;

pub(super) const RANGE: f32 = 3.0;

pub(super) struct Intersection {
    pub pixels: Vector2,
    pub distance: f32,
    pub inside: bool,
}

pub(super) struct Panel {
    inverse: Transform3D,
    center: Vector3,
    size: Vector2,
    viewport: Vector2,
}

impl Panel {
    pub fn new(
        panel: Transform3D,
        center: Vector3,
        size: Vector2,
        viewport: Vector2,
    ) -> Option<Self> {
        if !panel.is_finite()
            || !center.is_finite()
            || !size.is_finite()
            || !viewport.is_finite()
            || size.x <= 0.0
            || size.y <= 0.0
            || viewport.x <= 0.0
            || viewport.y <= 0.0
            || panel.basis.determinant().abs() < 1e-6
        {
            return None;
        }
        let inverse = panel.affine_inverse();
        inverse.is_finite().then_some(Self {
            inverse,
            center,
            size,
            viewport,
        })
    }

    pub fn project(&self, aim: Transform3D) -> Option<Intersection> {
        if !aim.is_finite() {
            return None;
        }
        let forward = -aim.basis.col_c();
        if forward.length_squared() < 1e-6 {
            return None;
        }
        let origin = self.inverse * aim.origin - self.center;
        let direction = self.inverse.basis * forward.normalized();
        if !origin.is_finite() || !direction.is_finite() || direction.z >= -1e-6 {
            return None;
        }
        let distance = -origin.z / direction.z;
        if !distance.is_finite() || distance <= 0.0 || distance > RANGE {
            return None;
        }
        let hit = origin + direction * distance;
        let uv = Vector2::new(hit.x / self.size.x + 0.5, 0.5 - hit.y / self.size.y);
        let pixels = uv * self.viewport;
        pixels.is_finite().then_some(Intersection {
            pixels,
            distance,
            inside: uv.x >= 0.0 && uv.y >= 0.0 && uv.x < 1.0 && uv.y < 1.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn project(
        aim: Transform3D,
        panel: Transform3D,
        center: Vector3,
        size: Vector2,
        viewport: Vector2,
    ) -> Option<Intersection> {
        Panel::new(panel, center, size, viewport)?.project(aim)
    }
    #[test]
    fn transformed_offset_panel_maps_to_pixels_and_does_not_clamp_capture() {
        let panel = Transform3D::new(
            Basis::from_axis_angle(Vector3::UP, 0.3).scaled(Vector3::new(1.2, 0.8, 1.0)),
            Vector3::new(0.4, 1.0, -0.3),
        );
        let center = Vector3::new(0.2, -0.1, 0.15);
        for (local, expected, inside) in [
            (Vector3::ZERO, Vector2::new(800.0, 500.0), true),
            (
                Vector3::new(0.4, 0.25, 0.0),
                Vector2::new(1200.0, 250.0),
                true,
            ),
            (
                Vector3::new(1.6, 0.0, 0.0),
                Vector2::new(2400.0, 500.0),
                false,
            ),
        ] {
            let aim = panel * Transform3D::new(Basis::IDENTITY, center + local + Vector3::BACK);
            let hit = project(
                aim,
                panel,
                center,
                Vector2::new(1.6, 1.0),
                Vector2::new(1600.0, 1000.0),
            )
            .expect("front ray");
            assert!((hit.pixels - expected).length() < 0.01);
            assert_eq!(hit.inside, inside);
        }
    }
    #[test]
    fn downward_pointer_tilt_moves_hit_down_without_moving_origin() {
        let aim = Transform3D::new(
            Basis::from_axis_angle(Vector3::RIGHT, -5.0_f32.to_radians()),
            Vector3::BACK,
        );
        let hit = project(
            aim,
            Transform3D::IDENTITY,
            Vector3::ZERO,
            Vector2::new(1.6, 1.0),
            Vector2::new(1600.0, 1000.0),
        )
        .expect("downward front ray");
        assert!((hit.pixels.x - 800.0).abs() < 0.01);
        assert!((hit.pixels.y - (500.0 + 5.0_f32.to_radians().tan() * 1000.0)).abs() < 0.01);
        assert!(hit.inside);
    }

    #[test]
    fn invalid_parallel_backwards_and_distant_rays_are_rejected() {
        let viewport = Vector2::new(1600.0, 1000.0);
        for aim in [
            Transform3D::new(Basis::IDENTITY, Vector3::FORWARD),
            Transform3D::new(Basis::IDENTITY, Vector3::BACK * 4.0),
            Transform3D::new(
                Basis::from_axis_angle(Vector3::UP, std::f32::consts::FRAC_PI_2),
                Vector3::BACK,
            ),
            Transform3D::new(Basis::IDENTITY, Vector3::new(f32::NAN, 0.0, 1.0)),
        ] {
            assert!(
                project(
                    aim,
                    Transform3D::IDENTITY,
                    Vector3::ZERO,
                    Vector2::ONE,
                    viewport
                )
                .is_none()
            );
        }
        assert!(
            project(
                Transform3D::IDENTITY,
                Transform3D::IDENTITY,
                Vector3::ZERO,
                Vector2::ZERO,
                viewport
            )
            .is_none()
        );
        let singular = Transform3D::new(Basis::from_scale(Vector3::ZERO), Vector3::ZERO);
        assert!(
            project(
                Transform3D::IDENTITY,
                singular,
                Vector3::ZERO,
                Vector2::ONE,
                viewport
            )
            .is_none()
        );
    }
}

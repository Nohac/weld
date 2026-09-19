//! Pinned spherical workspace placement. Head tracking only establishes a new
//! anchor on recenter; moving a window changes its own radius and direction.
use godot::{classes::IRefCounted, prelude::*};

const MIN_RADIUS: f32 = 0.6;
const MAX_RADIUS: f32 = 5.0;
const VERTICAL_FOLLOW: f32 = 0.5;
const BACKWARD_SLANT_DEGREES: f32 = 6.0;
const ORIENTATION_PIVOT_BEHIND: f32 = 1.6;

/// User placement relative to the layout parent. Unmoved dialogs/popups retain
/// application placement; moved windows face the pinned center. Child layers
/// inherit the resulting transform without independently facing the center.
#[derive(Default)]
pub(super) struct Placement {
    base: Transform3D,
    offset: Transform3D,
    center: Vector3,
    orientation_pivot: Vector3,
    moved: bool,
}
impl Placement {
    pub fn anchor(&self) -> (Vector3, Vector3) {
        (self.center, self.orientation_pivot)
    }
    pub fn apply(
        &mut self,
        base: Transform3D,
        center: Vector3,
        orientation_pivot: Vector3,
    ) -> Transform3D {
        self.base = base;
        self.center = center;
        self.orientation_pivot = orientation_pivot;
        let placed = base * self.offset;
        if self.moved {
            facing_center(center, orientation_pivot, placed.origin).unwrap_or(placed)
        } else {
            placed
        }
    }
    pub fn move_to(&mut self, world: Transform3D) {
        if !self.base.is_finite() || self.base.basis.determinant().abs() < 1e-6 {
            return;
        }
        if let Some(world) = facing_center(self.center, self.orientation_pivot, world.origin) {
            self.offset = self.base.affine_inverse() * world;
            self.moved = true;
        }
    }
}

/// Face a separate orientation pivot horizontally, with softened vertical tracking and
/// a small laptop-like backward slant at eye level, never wrist roll.
/// Reach and placement bounds remain relative to the actual workspace center.
pub(super) fn facing_center(
    center: Vector3,
    pivot: Vector3,
    position: Vector3,
) -> Option<Transform3D> {
    if !center.is_finite() || !pivot.is_finite() || !position.is_finite() {
        return None;
    }
    let offset = position - center;
    let radius = offset.length();
    if !radius.is_finite() || radius < 1e-6 {
        return None;
    }
    let elevation = (offset.y / radius)
        .clamp(-1.0, 1.0)
        .asin()
        .clamp(-85.0_f32.to_radians(), 85.0_f32.to_radians());
    let horizontal = Vector3::new(offset.x, 0.0, offset.z);
    let horizontal = if horizontal.length_squared() > 1e-8 {
        horizontal.normalized()
    } else {
        Vector3::FORWARD
    };
    let outward = horizontal * elevation.cos() + Vector3::UP * elevation.sin();
    let position = center + outward * radius.clamp(MIN_RADIUS, MAX_RADIUS);
    let facing = position - pivot;
    let facing_radius = facing.length();
    if !facing_radius.is_finite() || facing_radius < 1e-6 {
        return None;
    }
    let facing_horizontal = Vector3::new(facing.x, 0.0, facing.z);
    let facing_horizontal = if facing_horizontal.length_squared() > 1e-8 {
        facing_horizontal.normalized()
    } else {
        horizontal
    };
    let facing_elevation = (facing.y / facing_radius).clamp(-1.0, 1.0).asin();
    let pitch = facing_elevation * VERTICAL_FOLLOW - BACKWARD_SLANT_DEGREES.to_radians();
    let z = -facing_horizontal * pitch.cos() - Vector3::UP * pitch.sin();
    let x = Vector3::UP.cross(z).normalized();
    let y = z.cross(x);
    Some(Transform3D::new(Basis::from_cols(x, y, z), position))
}

#[derive(Clone, Copy)]
struct Anchor {
    world: Transform3D,
    radius: f32,
}
impl Anchor {
    fn orientation_pivot(self) -> Vector3 {
        self.world.origin + self.world.basis.col_c() * ORIENTATION_PIVOT_BEHIND
    }
    fn new(head: Transform3D, radius: f32) -> Option<Self> {
        if !head.is_finite() || !radius.is_finite() {
            return None;
        }
        let forward = -head.basis.col_c();
        let forward = Vector3::new(forward.x, 0.0, forward.z);
        if forward.length_squared() < 1e-6 {
            return None;
        }
        let z = -forward.normalized();
        Some(Self {
            world: Transform3D::new(
                Basis::from_cols(Vector3::UP.cross(z), Vector3::UP, z),
                head.origin,
            ),
            radius: radius.clamp(MIN_RADIUS, MAX_RADIUS),
        })
    }
    fn spawn(self, slot: i32, companion: bool) -> Transform3D {
        let slot = slot.clamp(0, 7);
        let angle = if companion {
            0.0
        } else {
            (65.0 * ((slot + 1) / 2) as f32 * if slot % 2 == 1 { 1.0 } else { -1.0 }).to_radians()
        };
        let elevation = if companion { -35.0_f32 } else { -6.0_f32 }.to_radians();
        let direction = Vector3::new(
            angle.sin() * elevation.cos(),
            elevation.sin(),
            -angle.cos() * elevation.cos(),
        );
        // The validated anchor and bounded angles cannot produce a zero offset.
        facing_center(
            self.world.origin,
            self.orientation_pivot(),
            self.world * (direction * self.radius),
        )
        .unwrap_or(self.world)
    }
}

/// Shared root-window placement policy for mono and stereo content.
#[derive(GodotClass)]
#[class(base=RefCounted)]
pub(super) struct WeldXrLayout {
    anchor: Option<Anchor>,
    base: Base<RefCounted>,
}
#[godot_api]
impl IRefCounted for WeldXrLayout {
    fn init(base: Base<RefCounted>) -> Self {
        Self { anchor: None, base }
    }
}
#[godot_api]
impl WeldXrLayout {
    #[func]
    fn recenter(&mut self, head: Transform3D, radius: f32) -> bool {
        let Some(anchor) = Anchor::new(head, radius) else {
            return false;
        };
        self.anchor = Some(anchor);
        true
    }
    #[func]
    fn center(&self) -> Vector3 {
        self.anchor
            .map_or(Vector3::ZERO, |anchor| anchor.world.origin)
    }
    #[func]
    fn orientation_pivot(&self) -> Vector3 {
        self.anchor.map_or(Vector3::ZERO, Anchor::orientation_pivot)
    }
    #[func]
    fn spawn_transform(&self, slot: i32, companion: bool) -> Transform3D {
        self.anchor.map_or(Transform3D::IDENTITY, |anchor| {
            anchor.spawn(slot, companion)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_does_not_accumulate_and_children_follow_moved_parent() {
        let mut placement = Placement::default();
        let base = Transform3D::new(Basis::IDENTITY, Vector3::new(0.0, 1.0, -1.6));
        let center = Vector3::UP;
        let pivot = center + Vector3::BACK * 1.6;
        assert_eq!(placement.apply(base, center, pivot), base);
        let moved = Transform3D::new(Basis::IDENTITY, base.origin + Vector3::RIGHT);
        placement.move_to(moved);
        let expected = facing_center(center, pivot, moved.origin).unwrap();
        for _ in 0..5 {
            let actual = placement.apply(base, center, pivot);
            assert!((actual.origin - expected.origin).length() < 1e-5);
            assert!((actual.basis.col_c() - expected.basis.col_c()).length() < 1e-5);
        }
        let parent_shift = Transform3D::new(Basis::from_axis_angle(Vector3::UP, 0.3), Vector3::UP);
        let actual = placement.apply(
            parent_shift * base,
            parent_shift * center,
            parent_shift * pivot,
        );
        assert!((actual.origin - parent_shift * expected.origin).length() < 1e-5);
        let mut child = Placement::default();
        let local = Transform3D::new(Basis::IDENTITY, Vector3::new(0.3, 0.2, 0.03));
        assert_eq!(
            child.apply(actual * local, parent_shift * center, parent_shift * pivot),
            actual * local
        );
    }

    #[test]
    fn all_spawn_slots_face_offset_pivot_without_changing_the_spawn_radius() {
        let head = Transform3D::new(
            Basis::from_axis_angle(Vector3::UP, 0.7),
            Vector3::new(2.0, 1.7, 3.0),
        );
        let anchor = Anchor::new(head, 1.6).unwrap();
        for slot in 0..8 {
            for companion in [false, true] {
                let pose = anchor.spawn(slot, companion);
                let inward = head.origin - pose.origin;
                assert!((inward.length() - 1.6).abs() < 1e-5);
                let toward_pivot = anchor.orientation_pivot() - pose.origin;
                let normal = pose.basis.col_c();
                assert!(
                    Vector3::new(normal.x, 0.0, normal.z)
                        .normalized()
                        .dot(Vector3::new(toward_pivot.x, 0.0, toward_pivot.z).normalized())
                        > 0.9999
                );
                assert!(pose.basis.col_a().y.abs() < 1e-5);
                assert!(pose.basis.col_b().y > 0.0);
            }
        }
    }

    #[test]
    fn vertical_tilt_is_softened_and_biased_without_changing_position() {
        for (elevation, expected_pitch) in [
            (-60.0_f32, -36.0_f32),
            (0.0, -6.0),
            (30.0, 9.0),
            (60.0, 24.0),
        ] {
            let radians = elevation.to_radians();
            let position = Vector3::new(0.0, radians.sin(), -radians.cos()) * 1.6;
            let pose = facing_center(Vector3::ZERO, Vector3::ZERO, position).unwrap();
            assert!((pose.origin - position).length() < 1e-5);
            let pitch = -pose.basis.col_c().y.asin();
            assert!((pitch - expected_pitch.to_radians()).abs() < 1e-5);
            assert!(pose.basis.col_a().y.abs() < 1e-5);
        }
    }

    #[test]
    fn manual_push_changes_only_radius_and_leaning_does_not_change_anchor() {
        let anchor = Anchor::new(Transform3D::IDENTITY, 1.6).unwrap();
        let first = anchor.spawn(0, false);
        let pushed = facing_center(
            anchor.world.origin,
            anchor.orientation_pivot(),
            first.origin * 1.5,
        )
        .unwrap();
        assert!((pushed.origin.length() - 2.4).abs() < 1e-5);
        assert!(pushed.basis.col_a().y.abs() < 1e-5);
        assert_eq!(anchor.spawn(0, false), first);
        let shifted = Anchor::new(Transform3D::new(Basis::IDENTITY, Vector3::RIGHT), 1.6).unwrap();
        assert!((shifted.spawn(0, false).origin - first.origin - Vector3::RIGHT).length() < 1e-5);
    }

    #[test]
    fn invalid_positions_and_poles_are_safe_and_distance_is_bounded() {
        assert!(facing_center(Vector3::ZERO, Vector3::ZERO, Vector3::ZERO).is_none());
        assert!(
            facing_center(
                Vector3::ZERO,
                Vector3::ZERO,
                Vector3::new(f32::NAN, 0.0, 0.0)
            )
            .is_none()
        );
        assert!(facing_center(Vector3::ZERO, Vector3::BACK, Vector3::BACK).is_none());
        for point in [
            Vector3::UP,
            Vector3::DOWN,
            Vector3::FORWARD * 100.0,
            Vector3::FORWARD * 0.01,
        ] {
            let pose = facing_center(Vector3::ZERO, Vector3::ZERO, point).unwrap();
            assert!(pose.is_finite());
            assert!((pose.basis.determinant() - 1.0).abs() < 1e-5);
            assert!((MIN_RADIUS - 1e-5..=MAX_RADIUS + 1e-5).contains(&pose.origin.length()));
        }
    }

    #[test]
    fn rear_pivot_reduces_horizontal_turn_without_moving_or_restricting_depth() {
        let center = Vector3::ZERO;
        let pivot = Vector3::BACK * 1.6;
        let position = Vector3::new(1.0, 0.0, -1.6);
        let direct = facing_center(center, center, position).unwrap();
        let relaxed = facing_center(center, pivot, position).unwrap();
        let yaw = |pose: Transform3D| pose.basis.col_c().x.abs().atan2(pose.basis.col_c().z);
        assert!((yaw(direct).to_degrees() - 32.005).abs() < 0.01);
        assert!((yaw(relaxed).to_degrees() - 17.354).abs() < 0.01);
        assert!((relaxed.origin - position).length() < 1e-5);
        let near = facing_center(center, pivot, Vector3::FORWARD * 0.7).unwrap();
        assert!((near.origin.length() - 0.7).abs() < 1e-5);
    }
}

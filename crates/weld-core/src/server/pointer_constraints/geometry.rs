//! Surface-local confinement and affine mapping, independent of protocol objects.

use smithay::utils::{Logical, Rectangle};
use weld_client::{InputPosition, InputTransform};

pub(super) fn inverse_transform(
    transform: InputTransform,
    position: InputPosition,
) -> Option<InputPosition> {
    let determinant = transform.xx * transform.yy - transform.xy * transform.yx;
    if determinant == 0.0 || !determinant.is_finite() {
        return None;
    }
    let x = position.x - transform.x;
    let y = position.y - transform.y;
    let position = InputPosition::new(
        (transform.yy * x - transform.xy * y) / determinant,
        (transform.xx * y - transform.yx * x) / determinant,
    );
    (position.x.is_finite() && position.y.is_finite()).then_some(position)
}

pub(super) fn contains(region: &[Rectangle<i32, Logical>], position: InputPosition) -> bool {
    region.iter().any(|rect| {
        position.x >= f64::from(rect.loc.x)
            && position.y >= f64::from(rect.loc.y)
            && position.x < f64::from(rect.loc.x) + f64::from(rect.size.w)
            && position.y < f64::from(rect.loc.y) + f64::from(rect.size.h)
    })
}

/// Clip motion against the connected intervals of a rectangle union. Testing
/// only the endpoint would allow a fast pointer to jump across a region's holes.
fn clip_segment(
    start: InputPosition,
    end: InputPosition,
    region: &[Rectangle<i32, Logical>],
) -> InputPosition {
    if !contains(region, start) || !end.x.is_finite() || !end.y.is_finite() {
        return start;
    }
    let mut intervals = Vec::new();
    for rect in region {
        let mut entry = 0.0_f64;
        let mut exit = 1.0_f64;
        for (origin, delta, min, size) in [
            (
                start.x,
                end.x - start.x,
                f64::from(rect.loc.x),
                f64::from(rect.size.w),
            ),
            (
                start.y,
                end.y - start.y,
                f64::from(rect.loc.y),
                f64::from(rect.size.h),
            ),
        ] {
            if delta == 0.0 {
                if origin < min || origin >= min + size {
                    exit = -1.0;
                }
            } else {
                let first = (min - origin) / delta;
                let second = (min + size - origin) / delta;
                entry = entry.max(first.min(second));
                exit = exit.min(first.max(second));
            }
        }
        if entry <= exit {
            intervals.push((entry, exit));
        }
    }
    intervals.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let mut reachable = 0.0_f64;
    for (entry, exit) in intervals {
        if entry > reachable {
            break;
        }
        reachable = reachable.max(exit);
    }
    if reachable >= 1.0 && contains(region, end) {
        return end;
    }
    // Stay one Wayland fixed-point unit inside the reached boundary. A single
    // floating-point ULP can round back onto an excluded edge when translating
    // through a scaled desktop route with a large origin.
    let distance = (end.x - start.x).abs().max((end.y - start.y).abs());
    if distance == 0.0 {
        return start;
    }
    let fraction = (reachable.clamp(0.0, 1.0) - (1.0 / 256.0) / distance).max(0.0);
    let result = InputPosition::new(
        start.x + (end.x - start.x) * fraction,
        start.y + (end.y - start.y) * fraction,
    );
    if contains(region, result) {
        result
    } else {
        start
    }
}

pub(super) fn confine(
    start: InputPosition,
    end: InputPosition,
    region: &[Rectangle<i32, Logical>],
) -> InputPosition {
    let stopped = clip_segment(start, end, region);
    let horizontal = clip_segment(stopped, InputPosition::new(end.x, stopped.y), region);
    clip_segment(horizontal, InputPosition::new(horizontal.x, end.y), region)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confinement_slides_without_crossing_holes_or_disconnected_regions() {
        let region = vec![
            Rectangle::from_extremities((0, 0), (10, 10)),
            Rectangle::from_extremities((20, 0), (30, 10)),
        ];
        let start = InputPosition::new(5., 5.);
        let result = confine(start, InputPosition::new(25., 8.), &region);
        assert!(result.x >= 5. && result.x < 10.);
        assert_eq!(result.y, 8.);
        let adjacent = vec![
            Rectangle::from_extremities((0, 0), (10, 10)),
            Rectangle::from_extremities((10, 0), (20, 10)),
        ];
        assert_eq!(
            confine(start, InputPosition::new(15., 5.), &adjacent),
            InputPosition::new(15., 5.)
        );
        assert_eq!(
            confine(start, InputPosition::new(f64::NAN, 5.), &region),
            start
        );
        assert_eq!(confine(start, InputPosition::new(100., 100.), &[]), start);
    }

    #[test]
    fn inverse_pointer_mapping_handles_scale_rotation_and_invalid_transforms() {
        let transform = InputTransform {
            xx: 0.,
            xy: -2.,
            yx: 3.,
            yy: 0.,
            x: 100.,
            y: 20.,
        };
        let point = InputPosition::new(12., 8.);
        assert_eq!(
            inverse_transform(transform, transform.transform(point)),
            Some(point)
        );
        assert_eq!(
            inverse_transform(
                InputTransform {
                    xx: 0.,
                    ..InputTransform::IDENTITY
                },
                point
            ),
            None
        );
    }
}

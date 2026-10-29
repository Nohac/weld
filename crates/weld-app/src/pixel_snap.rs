//! Screen-space alignment applied between UI layout and clipping.

use bevy::{
    app::{App, Plugin, PostUpdate},
    ecs::{
        component::Component,
        entity::Entity,
        hierarchy::{ChildOf, Children},
        query::{Has, Or, With},
        schedule::{IntoScheduleConfigs, SystemSet},
        system::{Local, Query},
    },
    math::{Affine2, Vec2},
    ui::{CalculatedClip, ComputedNode, Node, UiGlobalTransform, UiSystems},
};

/// Aligns an axis-aligned UI node's top-left corner to physical display pixels.
///
/// Attach to the content that needs a stable pixel origin. Its descendants follow
/// the adjustment, while ancestors retain their continuous layout and motion.
/// Rotated, sheared, and reflected nodes retain their authored transform.
#[derive(Component, Clone, Copy, Debug, Default)]
#[require(Node)]
pub struct PixelSnap;

/// Aligns only this node's painted bounds and inherited clips after layout.
/// Descendant text and controls retain their independently computed transforms.
#[derive(Component, Clone, Copy, Debug, Default)]
#[require(Node)]
pub struct PixelSnapPaint;

/// Exact physical extent supplied by a presenter that knows its raster dimensions.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PixelSnapSize(pub Option<Vec2>);

/// Post-layout snapping completes before Bevy calculates clipping rectangles.
#[derive(SystemSet, Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PixelSnapSystems;

/// Paint bounds and clips are aligned before surface masks are resolved.
#[derive(SystemSet, Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PixelSnapPaintSystems;

/// Installs screen-space snapping for [`PixelSnap`] nodes.
pub struct PixelSnapPlugin;

impl Plugin for PixelSnapPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PostUpdate,
            snap_nodes
                .in_set(PixelSnapSystems)
                .after(UiSystems::Layout)
                .before(UiSystems::PostLayout),
        )
        .add_systems(
            PostUpdate,
            snap_paint
                .in_set(PixelSnapPaintSystems)
                .after(UiSystems::PostLayout),
        );
    }
}

type SnappedClipNodes<'w, 's> =
    Query<'w, 's, &'static mut CalculatedClip, Or<(With<PixelSnap>, With<PixelSnapPaint>)>>;

fn snap_paint(
    mut paints: Query<(&ComputedNode, &mut UiGlobalTransform), With<PixelSnapPaint>>,
    mut clips: SnappedClipNodes,
) {
    for (node, mut transform) in &mut paints {
        if let Some((minimum, maximum)) =
            aligned_bounds(**transform, -0.5 * node.size(), 0.5 * node.size())
        {
            let next = snapped_transform(**transform, node.size(), Some(maximum - minimum));
            if next != **transform {
                *transform = next.into();
            }
        }
    }
    for mut clip in &mut clips {
        let next = aligned_clip(&clip);
        if next != *clip {
            *clip = next;
        }
    }
}

pub(crate) fn aligned_clip(clip: &CalculatedClip) -> CalculatedClip {
    let mut next = clip.clone();
    if let CalculatedClip::Rects(rects) = &mut next {
        for rect in rects {
            if rect.world_to_clip_local.matrix2.determinant() == 0.0 {
                continue;
            }
            let inverse = rect.world_to_clip_local.inverse();
            if let Some((minimum, maximum)) = aligned_bounds(inverse, rect.rect.min, rect.rect.max)
            {
                rect.rect.min = rect.world_to_clip_local.transform_point2(minimum);
                rect.rect.max = rect.world_to_clip_local.transform_point2(maximum);
            }
        }
    }
    next
}

fn aligned_bounds(transform: Affine2, minimum: Vec2, maximum: Vec2) -> Option<(Vec2, Vec2)> {
    let matrix = transform.matrix2;
    if !transform.is_finite()
        || !minimum.is_finite()
        || !maximum.is_finite()
        || matrix.x_axis.y.abs() > f32::EPSILON
        || matrix.y_axis.x.abs() > f32::EPSILON
        || matrix.x_axis.x <= 0.0
        || matrix.y_axis.y <= 0.0
    {
        return None;
    }
    let minimum = transform.transform_point2(minimum).round();
    let maximum = transform.transform_point2(maximum).round();
    ((maximum - minimum).min_element() > 0.0).then_some((minimum, maximum))
}

type SnapNodes<'w, 's> = Query<
    'w,
    's,
    (
        &'static ComputedNode,
        &'static mut UiGlobalTransform,
        Has<PixelSnap>,
        Option<&'static PixelSnapSize>,
    ),
>;

fn snap_nodes(
    marked: Query<Entity, With<PixelSnap>>,
    parents: Query<&ChildOf>,
    children: Query<&Children>,
    mut nodes: SnapNodes,
    mut pending: Local<Vec<(Entity, Affine2)>>,
) {
    for root in &marked {
        if parents
            .iter_ancestors(root)
            .any(|parent| marked.contains(parent))
        {
            continue;
        }
        pending.push((root, Affine2::IDENTITY));
        while let Some((entity, mut correction)) = pending.pop() {
            if let Ok((node, mut transform, snap, target)) = nodes.get_mut(entity) {
                let original = **transform;
                let inherited = correction * original;
                let adjusted = if snap {
                    snapped_transform(inherited, node.size(), target.and_then(|target| target.0))
                } else {
                    inherited
                };
                if adjusted != inherited && original.matrix2.determinant().abs() > f32::EPSILON {
                    correction = adjusted * original.inverse();
                }
                if adjusted != original {
                    *transform = adjusted.into();
                }
            }
            if let Ok(children) = children.get(entity) {
                pending.extend(children.iter().map(|child| (*child, correction)));
            }
        }
    }
}

fn snapped_transform(mut transform: Affine2, size: Vec2, target: Option<Vec2>) -> Affine2 {
    let matrix = transform.matrix2;
    if !transform.is_finite()
        || !size.is_finite()
        || size.min_element() <= 0.0
        || matrix.x_axis.y.abs() > f32::EPSILON
        || matrix.y_axis.x.abs() > f32::EPSILON
        || matrix.x_axis.x <= 0.0
        || matrix.y_axis.y <= 0.0
    {
        return transform;
    }
    let origin = transform.transform_point2(-0.5 * size);
    let extent = transform.transform_vector2(size);
    let snapped_extent = target
        .filter(|target| target.is_finite() && target.min_element() > 0.0)
        .unwrap_or(extent);
    transform.matrix2.x_axis *= snapped_extent.x / extent.x;
    transform.matrix2.y_axis *= snapped_extent.y / extent.y;
    transform.translation = origin.round() + snapped_extent * 0.5;
    transform
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::{component::Component, system::Query};

    #[derive(Component, Clone, Copy)]
    struct LayoutTransform(Affine2);

    fn reset_layout(mut nodes: Query<(&LayoutTransform, &mut UiGlobalTransform)>) {
        for (layout, mut transform) in &mut nodes {
            *transform = layout.0.into();
        }
    }

    #[test]
    fn paint_alignment_preserves_child_transforms_and_is_stable_across_updates() {
        let mut app = App::new();
        app.add_plugins(PixelSnapPlugin)
            .add_systems(PostUpdate, reset_layout.in_set(UiSystems::Layout));
        let size = Vec2::new(101.5, 78.25);
        let original = Affine2::from_translation(Vec2::new(13.75, 17.5) + size * 0.5);
        let clip = CalculatedClip::default().with_rect(
            bevy::math::Rect::from_corners(-0.5 * size, 0.5 * size),
            &original.into(),
        );
        let paint = app
            .world_mut()
            .spawn((
                PixelSnapPaint,
                ComputedNode {
                    size,
                    ..Default::default()
                },
                LayoutTransform(original),
                clip.clone(),
            ))
            .id();
        let child_transform = Affine2::from_translation(Vec2::new(25.75, 29.5));
        let child = app
            .world_mut()
            .spawn((
                Node::default(),
                LayoutTransform(child_transform),
                ChildOf(paint),
            ))
            .id();
        for _ in 0..3 {
            app.update();
            let painted = app.world().get::<UiGlobalTransform>(paint).expect("paint");
            assert!(
                painted
                    .transform_point2(-0.5 * size)
                    .abs_diff_eq(Vec2::new(14.0, 18.0), 0.0001)
            );
            assert!(
                painted
                    .transform_point2(0.5 * size)
                    .abs_diff_eq(Vec2::new(115.0, 96.0), 0.0001)
            );
            assert_eq!(
                **app.world().get::<UiGlobalTransform>(child).expect("child"),
                child_transform
            );
            assert_eq!(
                app.world().get::<CalculatedClip>(paint),
                Some(&aligned_clip(&clip))
            );
        }
        assert!(aligned_bounds(Affine2::from_angle(0.5), -size, size).is_none());
    }

    #[test]
    fn snapping_follows_final_pixels_and_descendants_without_moving_the_frame() {
        let mut app = App::new();
        app.add_plugins(PixelSnapPlugin)
            .add_systems(PostUpdate, reset_layout.in_set(UiSystems::Layout));
        let frame_transform = Affine2::from_translation(Vec2::splat(31.25));
        let frame = app
            .world_mut()
            .spawn((Node::default(), LayoutTransform(frame_transform)))
            .id();
        let size = Vec2::new(697.5, 1297.5);
        let origin = Vec2::splat(13.75);
        let base = Affine2::from_translation(origin + 0.5 * size);
        let content = app
            .world_mut()
            .spawn((
                PixelSnap,
                PixelSnapSize(Some(Vec2::new(698., 1298.))),
                ComputedNode {
                    size,
                    ..Default::default()
                },
                LayoutTransform(base),
                ChildOf(frame),
            ))
            .id();
        let hit_size = Vec2::splat(10.);
        let hit_base = Affine2::from_translation(origin + Vec2::new(20., 30.) + 0.5 * hit_size);
        let hit = app
            .world_mut()
            .spawn((
                Node::default(),
                ComputedNode {
                    size: hit_size,
                    ..Default::default()
                },
                LayoutTransform(hit_base),
                ChildOf(content),
            ))
            .id();
        for _ in 0..3 {
            app.update();
            assert_eq!(
                **app.world().get::<UiGlobalTransform>(frame).expect("frame"),
                frame_transform
            );
            let corrected = **app
                .world()
                .get::<UiGlobalTransform>(content)
                .expect("content");
            assert!(
                corrected
                    .transform_point2(-0.5 * size)
                    .abs_diff_eq(Vec2::splat(14.), 0.0001)
            );
            assert!(
                corrected
                    .transform_vector2(size)
                    .abs_diff_eq(Vec2::new(698., 1298.), 0.001)
            );
            let expected_hit = corrected * base.inverse() * hit_base;
            let actual_hit = **app.world().get::<UiGlobalTransform>(hit).expect("hit");
            assert!(actual_hit.abs_diff_eq(expected_hit, 0.0001));
            let point = Vec2::new(1., 2.);
            assert!(
                actual_hit
                    .inverse()
                    .transform_point2(expected_hit.transform_point2(point))
                    .abs_diff_eq(point, 0.0001)
            );
        }
        app.world_mut().entity_mut(hit).insert(PixelSnap);
        app.update();
        let nested = **app
            .world()
            .get::<UiGlobalTransform>(hit)
            .expect("nested snap");
        let nested_origin = nested.transform_point2(-0.5 * hit_size);
        assert!(nested_origin.abs_diff_eq(nested_origin.round(), 0.0001));
        app.update();
        assert!(
            app.world()
                .get::<UiGlobalTransform>(hit)
                .expect("nested snap")
                .abs_diff_eq(nested, 0.0001)
        );
        app.world_mut().entity_mut(hit).remove::<PixelSnap>();
        app.world_mut().entity_mut(content).remove::<PixelSnap>();
        app.update();
        assert_eq!(
            **app
                .world()
                .get::<UiGlobalTransform>(content)
                .expect("content"),
            base
        );
        assert_eq!(
            **app.world().get::<UiGlobalTransform>(hit).expect("hit"),
            hit_base
        );
    }

    #[test]
    fn position_snapping_preserves_size_and_skips_non_axis_aligned_transforms() {
        let size = Vec2::new(17.25, 23.75);
        let transform = Affine2::from_translation(Vec2::new(10.125, 20.875));
        let snapped = snapped_transform(transform, size, None);
        assert_eq!(snapped.matrix2, transform.matrix2);
        let origin = snapped.transform_point2(-0.5 * size);
        assert_eq!(origin, origin.round());
        for transform in [
            Affine2::from_angle(0.2),
            Affine2::from_scale(Vec2::new(-1., 1.)),
            Affine2::from_scale(Vec2::ZERO),
        ] {
            assert_eq!(
                snapped_transform(transform, size, Some(Vec2::splat(20.))),
                transform
            );
        }
    }
}

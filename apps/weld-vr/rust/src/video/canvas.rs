//! Native-panel padding for shell chrome. Content pixels and input coordinates
//! keep their own rectangle; padding never changes decoder or application size.
use godot::{
    classes::{ColorRect, Control, ShaderMaterial},
    prelude::*,
};

pub(super) const SHADOW_MARGIN: f32 = 0.06;
const CHROME_MARGIN: f32 = 0.1;

#[derive(Clone, Copy)]
pub(super) struct Layout {
    pub raster: Vector2i,
    pub physical: Vector2,
    pub content: Rect2,
    scale: Vector2,
}
impl Layout {
    pub fn new(content: Vector2, raster: Vector2i, decorated: bool) -> Option<Self> {
        if !content.is_finite()
            || content.x <= 0.0
            || content.y <= 0.0
            || !(1..=2048).contains(&raster.x)
            || !(1..=2048).contains(&raster.y)
        {
            return None;
        }
        let physical = if decorated {
            Vector2::new(
                (content.x + 2.0 * SHADOW_MARGIN).max(0.4),
                content.y + 2.0 * CHROME_MARGIN,
            )
        } else {
            content
        };
        let desired = physical * (raster.to_vector2() / content);
        if !desired.is_finite() {
            return None;
        }
        let shrink = (4096.0 / desired.x.max(desired.y)).min(1.0);
        let padded = (desired * shrink).ceil().to_vector2i();
        let padded = Vector2i::new(padded.x.clamp(1, 4096), padded.y.clamp(1, 4096));
        let scale = padded.to_vector2() / physical;
        let content = Rect2::new((physical - content) * 0.5 * scale, content * scale);
        Some(Self {
            raster: padded,
            physical,
            content,
            scale,
        })
    }
    pub fn rectangle(self, center: Vector2, size: Vector2) -> Rect2 {
        Rect2::new(
            (self.physical * 0.5 + center - size * 0.5) * self.scale,
            size * self.scale,
        )
    }
}

pub(super) fn add_overlay(
    view: &Gd<Control>,
    material: &Gd<ShaderMaterial>,
    order: i32,
) -> Option<Gd<ColorRect>> {
    let mut parent = view.get_parent()?;
    let mut rect = ColorRect::new_alloc();
    rect.set_mouse_filter(godot::classes::control::MouseFilter::IGNORE);
    rect.set_material(material);
    rect.set_z_index(order);
    parent.add_child(&rect);
    Some(rect)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn padding_keeps_content_scale_and_contains_chrome() {
        let layout = Layout::new(Vector2::new(1.6, 1.0), Vector2i::new(1600, 1000), true).unwrap();
        assert!((layout.content.size - Vector2::new(1600.0, 1000.0)).length() < 2.0);
        assert!(layout.content.position.x >= 59.0 && layout.content.position.y >= 99.0);
        for y in [-0.55, 0.55] {
            let bar = layout.rectangle(Vector2::new(0.0, y), Vector2::new(0.34, 0.05));
            assert!(bar.position.y >= 0.0 && bar.end().y <= layout.raster.y as f32);
        }
        let plain = Layout::new(Vector2::ONE, Vector2i::new(1024, 1024), false).unwrap();
        assert_eq!(plain.raster, Vector2i::new(1024, 1024));
        assert_eq!(plain.content.position, Vector2::ZERO);
    }
    #[test]
    fn tiny_windows_have_bounded_canvas_allocations() {
        let layout = Layout::new(Vector2::splat(0.001), Vector2i::new(2048, 2048), true).unwrap();
        assert!(layout.raster.x <= 4096 && layout.raster.y <= 4096);
        assert!(Layout::new(Vector2::ZERO, Vector2i::ONE, true).is_none());
    }
}

//! Hit testing for a displayed surface rectangle, independent of its renderer.
use crate::{
    ClientPointerRoute, ClientSurfaceId, InputPosition, InputTransform, SurfaceInputPlacement,
};

/// Input-only snapshot of displayed content. Contains no buffer leases.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceInputGeometry {
    pub surface: ClientSurfaceId,
    /// Surface-local origin of the displayed window geometry.
    pub origin: InputPosition,
    pub logical_size: [f64; 2],
    /// Back-to-front input placements for layers actually displayed.
    pub inputs: Vec<SurfaceInputPlacement>,
}

impl SurfaceInputGeometry {
    /// Maps a presenter position through its actual image rectangle, excluding
    /// letterboxing. The returned affine route also works outside the rectangle
    /// during a captured drag. `rectangle` is x, y, width, height.
    pub fn pointer_route(
        &self,
        rectangle: [f64; 4],
        position: InputPosition,
    ) -> Option<ClientPointerRoute> {
        let [x, y, width, height] = rectangle;
        if !rectangle
            .into_iter()
            .chain(self.logical_size)
            .chain([position.x, position.y, self.origin.x, self.origin.y])
            .all(f64::is_finite)
            || width <= 0.0
            || height <= 0.0
            || self.logical_size.iter().any(|size| *size <= 0.0)
            || position.x < x
            || position.y < y
            || position.x >= x + width
            || position.y >= y + height
        {
            return None;
        }
        let xx = self.logical_size[0] / width;
        let yy = self.logical_size[1] / height;
        for input in self.inputs.iter().rev() {
            let transform = InputTransform {
                xx,
                xy: 0.0,
                yx: 0.0,
                yy,
                x: self.origin.x - f64::from(input.position.x) - x * xx,
                y: self.origin.y - f64::from(input.position.y) - y * yy,
            };
            let local = transform.transform(position);
            if input.regions.iter().any(|region| {
                let left = f64::from(region.position.x);
                let top = f64::from(region.position.y);
                local.x >= left
                    && local.y >= top
                    && local.x < left + f64::from(region.size.width)
                    && local.y < top + f64::from(region.size.height)
            }) {
                return Some(ClientPointerRoute {
                    surface: self.surface,
                    layer: input.layer,
                    transform,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ClientId, ClientSourceId, LogicalPoint, LogicalSize, SurfaceInputRect, SurfaceLayerId,
    };

    #[test]
    fn cropped_origin_scaled_image_and_input_regions_share_coordinates() {
        let geometry = SurfaceInputGeometry {
            surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1),
            origin: InputPosition::new(10.0, 20.0),
            logical_size: [200.0, 100.0],
            inputs: vec![SurfaceInputPlacement {
                layer: SurfaceLayerId::new(1),
                position: LogicalPoint::new(4.0, 6.0),
                regions: vec![SurfaceInputRect {
                    position: LogicalPoint::new(6.0, 14.0),
                    size: LogicalSize::new(200.0, 100.0),
                }],
            }],
        };
        let rect = [100.0, 50.0, 400.0, 200.0];
        let route = geometry
            .pointer_route(rect, InputPosition::new(300.0, 150.0))
            .expect("inside");
        assert_eq!(
            route.transform.transform(InputPosition::new(300.0, 150.0)),
            InputPosition::new(106.0, 64.0)
        );
        assert!(
            geometry
                .pointer_route(rect, InputPosition::new(99.0, 150.0))
                .is_none()
        );
        assert!(
            geometry
                .pointer_route(rect, InputPosition::new(500.0, 150.0))
                .is_none()
        );
        assert!(
            geometry
                .pointer_route([0.0; 4], InputPosition::default())
                .is_none()
        );
        assert!(
            geometry
                .pointer_route(rect, InputPosition::new(f64::NAN, 0.0))
                .is_none()
        );
    }

    #[test]
    fn frontmost_displayed_layer_wins_and_region_holes_do_not_capture() {
        let mut geometry = SurfaceInputGeometry {
            surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1),
            origin: InputPosition::default(),
            logical_size: [100.0, 100.0],
            inputs: vec![SurfaceInputPlacement {
                layer: SurfaceLayerId::new(1),
                position: LogicalPoint::ZERO,
                regions: vec![SurfaceInputRect {
                    position: LogicalPoint::ZERO,
                    size: LogicalSize::new(100.0, 100.0),
                }],
            }],
        };
        let mut overlay = geometry.inputs[0].clone();
        overlay.layer = SurfaceLayerId::new(2);
        overlay.regions[0].size = LogicalSize::new(10.0, 10.0);
        geometry.inputs.push(overlay);
        let rect = [0.0, 0.0, 100.0, 100.0];
        assert_eq!(
            geometry
                .pointer_route(rect, InputPosition::new(5.0, 5.0))
                .expect("overlay")
                .layer,
            SurfaceLayerId::new(2)
        );
        assert_eq!(
            geometry
                .pointer_route(rect, InputPosition::new(50.0, 50.0))
                .expect("root")
                .layer,
            SurfaceLayerId::new(1)
        );
        geometry.inputs.remove(0);
        assert!(
            geometry
                .pointer_route(rect, InputPosition::new(50.0, 50.0))
                .is_none()
        );
    }
}

//! Select logical-pixel or percentage resize amounts for each layout plane.

use bevy::{
    ecs::{
        event::Event,
        observer::On,
        query::With,
        system::{Commands, Query, Res},
    },
    math::Vec2,
};
use weld_float::FloatingResizeRequest;
use weld_tile::{SplitAxis, TileOperation, TileRequest};
use weld_window::{FloatingWindow, FocusedWindow, WindowGeometry};

#[derive(Event, Clone, Copy, Debug, PartialEq)]
pub struct I3ResizeRequest {
    pub axis: SplitAxis,
    pub pixels: Option<f32>,
    pub fraction: Option<f32>,
}

pub(crate) fn request(
    event: On<I3ResizeRequest>,
    focus: Res<FocusedWindow>,
    floating: Query<&WindowGeometry, With<FloatingWindow>>,
    mut commands: Commands,
) {
    if let Some(window) = focus.entity()
        && let Ok(geometry) = floating.get(window)
    {
        let extent = match event.axis {
            SplitAxis::Horizontal => geometry.size.x,
            SplitAxis::Vertical => geometry.size.y,
        };
        let amount = event
            .pixels
            .or_else(|| event.fraction.map(|fraction| fraction * extent))
            .unwrap_or(0.0);
        commands.trigger(FloatingResizeRequest {
            window,
            delta: match event.axis {
                SplitAxis::Horizontal => Vec2::new(amount, 0.0),
                SplitAxis::Vertical => Vec2::new(0.0, amount),
            },
        });
    } else {
        let operation = if let Some(fraction) = event.fraction {
            TileOperation::Resize {
                axis: event.axis,
                fraction,
            }
        } else if let Some(pixels) = event.pixels {
            TileOperation::ResizePixels {
                axis: event.axis,
                pixels,
            }
        } else {
            return;
        };
        commands.trigger(TileRequest::Focused(operation));
    }
}

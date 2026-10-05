//! Keep each contact's down target while sharing one controller touch context.
use super::ServerState;
use smithay::{
    input::touch::{
        DownEvent, GrabStartData, MotionEvent, OrientationEvent, ShapeEvent, TouchDownGrab,
        TouchGrab, TouchInnerHandle, UpEvent,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
};

pub(super) struct ContactGrab(TouchDownGrab<ServerState>);

impl ContactGrab {
    pub fn new(start: GrabStartData<ServerState>) -> Self {
        Self(TouchDownGrab {
            start_data: start,
            touch_points: 1,
        })
    }
}

impl TouchGrab<ServerState> for ContactGrab {
    fn down(
        &mut self,
        data: &mut ServerState,
        handle: &mut TouchInnerHandle<'_, ServerState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &DownEvent,
    ) {
        handle.down(data, focus, event);
        self.0.touch_points += 1;
    }
    fn motion(
        &mut self,
        data: &mut ServerState,
        handle: &mut TouchInnerHandle<'_, ServerState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, focus, event);
    }
    fn up(
        &mut self,
        data: &mut ServerState,
        handle: &mut TouchInnerHandle<'_, ServerState>,
        event: &UpEvent,
    ) {
        self.0.up(data, handle, event);
    }
    fn frame(&mut self, data: &mut ServerState, handle: &mut TouchInnerHandle<'_, ServerState>) {
        self.0.frame(data, handle);
    }
    fn cancel(&mut self, data: &mut ServerState, handle: &mut TouchInnerHandle<'_, ServerState>) {
        self.0.cancel(data, handle);
    }
    fn shape(
        &mut self,
        data: &mut ServerState,
        handle: &mut TouchInnerHandle<'_, ServerState>,
        event: &ShapeEvent,
    ) {
        self.0.shape(data, handle, event);
    }
    fn orientation(
        &mut self,
        data: &mut ServerState,
        handle: &mut TouchInnerHandle<'_, ServerState>,
        event: &OrientationEvent,
    ) {
        self.0.orientation(data, handle, event);
    }
    fn start_data(&self) -> &GrabStartData<ServerState> {
        &self.0.start_data
    }
    fn unset(&mut self, data: &mut ServerState) {
        self.0.unset(data);
    }
}

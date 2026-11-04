//! The controller's cursor is sampled with pixels, without borrowing desktop input.

use super::State;
use anyhow::Result;
use std::{rc::Rc, time::Instant};
use weld_client::{
    ClientBufferLease, ClientSurfaceCommit, ClientSurfaceId, ComposedBuffer, CompositionCursor,
    InputPosition, LogicalPoint, SurfaceBufferChange,
};

impl State {
    pub(super) fn position_cursor(&mut self, surface: ClientSurfaceId, position: InputPosition) {
        if self.positions.insert(surface, position) != Some(position) {
            self.cursor_dirty.insert(surface);
        }
    }

    pub(super) fn captured_frame(
        &self,
        surface: ClientSurfaceId,
        frame: &ClientSurfaceCommit,
    ) -> Result<ClientSurfaceCommit> {
        let cursor = self.positions.get(&surface).and_then(|position| {
            let owner = self.input.pointer_surface(surface).or_else(|| {
                self.views
                    .hit(surface, *position)
                    .map(|target| target.surface)
            })?;
            Some(CompositionCursor {
                image: self.cursors.get(&owner).cloned().unwrap_or_default(),
                position: LogicalPoint::new(position.x as f32, position.y as f32),
            })
        });
        let mut frame = frame.clone().into_state();
        for update in &mut frame.buffers {
            if let SurfaceBufferChange::Replaced { buffer, metadata } = &mut update.change
                && let Some(scene) = buffer.access::<ComposedBuffer>()
            {
                let mut scene = scene.clone();
                scene.cursor = cursor.clone();
                *buffer = ClientBufferLease::new(
                    buffer.buffer(),
                    buffer.use_id(),
                    *metadata,
                    Rc::new(scene),
                    |_| {},
                )?;
            }
        }
        Ok(frame.into())
    }

    pub(super) fn record_frame(
        &mut self,
        surface: ClientSurfaceId,
        frame: &ClientSurfaceCommit,
        now: Instant,
        cursor_only: bool,
    ) {
        self.cursor_dirty.remove(&surface);
        if !self
            .recordings
            .values()
            .any(|active| active.grant.surface == surface)
        {
            return;
        }
        let captured = self.captured_frame(surface, frame);
        for active in self
            .recordings
            .values_mut()
            .filter(|active| active.grant.surface == surface)
        {
            if active.error.is_some() {
                continue;
            }
            let result = match &captured {
                Ok(frame) if cursor_only => active.recorder.observe_cursor(frame, now),
                Ok(frame) => active.recorder.observe(frame, now),
                Err(error) => Err(anyhow::anyhow!(error.to_string())),
            };
            if let Err(error) = result {
                active.error = Some(error.to_string());
            }
        }
    }
}

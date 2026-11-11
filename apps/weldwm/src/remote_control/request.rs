//! Bounded handoff waits owned by the current window-control incarnation.

use super::{ControlRequested, ControlledWindow};
use bevy::prelude::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Instant,
};
use weld_window::attention::AttentionForWindow;

#[derive(Component)]
#[require(ControlRequested)]
pub(super) struct PendingInputRequest {
    control: String,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    completion: Option<mpsc::SyncSender<Result<(), String>>>,
}

pub(super) fn install(app: &mut App) {
    app.add_observer(
        |event: On<Add<PendingInputRequest>>, mut commands: Commands| {
            commands
                .entity(event.entity)
                .insert(AttentionForWindow(event.entity));
        },
    );
    app.add_observer(
        |event: On<Remove<PendingInputRequest>>, mut commands: Commands| {
            commands
                .entity(event.entity)
                .try_remove::<AttentionForWindow>();
        },
    );
}

impl PendingInputRequest {
    pub(super) fn new(
        control: String,
        deadline: Instant,
        cancelled: Arc<AtomicBool>,
        completion: mpsc::SyncSender<Result<(), String>>,
    ) -> Self {
        Self {
            control,
            deadline,
            cancelled,
            completion: Some(completion),
        }
    }

    fn finish(&mut self, result: Result<(), String>) {
        if let Some(completion) = self.completion.take() {
            let _ = completion.try_send(result);
        }
    }
}

impl Drop for PendingInputRequest {
    fn drop(&mut self) {
        self.finish(Err("window control request was cancelled".into()));
    }
}

pub(super) fn complete_requests(
    mut commands: Commands,
    mut requests: Query<(Entity, &mut PendingInputRequest, Option<&ControlledWindow>)>,
    mut redraw: MessageWriter<bevy::window::RequestRedraw>,
) {
    let now = Instant::now();
    for (window, mut request, grant) in &mut requests {
        let result = if request.cancelled.load(Ordering::Acquire) || request.deadline <= now {
            Some(Err("window control request timed out".into()))
        } else {
            match grant {
                Some(grant) if grant.0.active() && grant.0.id() == request.control => {
                    grant.0.remote_input_available().then_some(Ok(()))
                }
                _ => Some(Err(
                    "window control was revoked or the application was replaced".into(),
                )),
            }
        };
        if let Some(result) = result {
            request.finish(result);
            commands
                .entity(window)
                .remove::<(PendingInputRequest, ControlRequested)>();
            redraw.write(bevy::window::RequestRedraw);
        }
    }
}

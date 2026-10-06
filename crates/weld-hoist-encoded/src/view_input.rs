//! Routes viewport input back to native surfaces, retaining contact and drag targets.

use std::collections::{HashMap, HashSet};
use weld_client::{
    ButtonState, ClientInputTarget, ClientSurfaceId, InputEventKind, LinuxButtonCode,
    MAX_TOUCH_CONTACTS, RawScrollPhase, RawScrollSource, TouchEvent, TouchId,
};
use weld_hoist_protocol::{DestinationEnvelope, DestinationMessage};

use crate::view::{SourceViews, ViewTarget};

#[derive(Default)]
struct Pointer {
    target: Option<ViewTarget>,
    buttons: HashSet<LinuxButtonCode>,
    gesture: bool,
    scrolling: bool,
}

#[derive(Default)]
pub(crate) struct ViewInput {
    pointers: HashMap<ClientSurfaceId, Pointer>,
    contacts: HashMap<(ClientSurfaceId, TouchId), ViewTarget>,
    touched: HashMap<ClientSurfaceId, Vec<ViewTarget>>,
}

impl ViewInput {
    pub fn route(
        &mut self,
        views: &SourceViews,
        mut envelope: DestinationEnvelope,
    ) -> Vec<DestinationEnvelope> {
        let DestinationMessage::Input(input) = &mut envelope.message else {
            return vec![envelope];
        };
        let root = input.target.surface();
        if views.root(root) != Some(root) || views.session(root) != Some(envelope.session) {
            return vec![envelope];
        }
        let target = match (&input.target, &mut input.event) {
            (ClientInputTarget::Touch { .. }, InputEventKind::Touch { event }) => match event {
                TouchEvent::Down { id, position } => {
                    if self.contacts.len() >= MAX_TOUCH_CONTACTS
                        || self.contacts.contains_key(&(root, *id))
                    {
                        return Vec::new();
                    }
                    let Some(target) = views.hit(root, *position) else {
                        return Vec::new();
                    };
                    self.contacts.insert((root, *id), target);
                    *position = target.local(*position);
                    target
                }
                TouchEvent::Motion { id, position } => {
                    let Some(target) = self.contacts.get(&(root, *id)).copied() else {
                        return Vec::new();
                    };
                    *position = target.local(*position);
                    target
                }
                TouchEvent::Up { id } => {
                    let Some(target) = self.contacts.remove(&(root, *id)) else {
                        return Vec::new();
                    };
                    target
                }
                TouchEvent::Frame | TouchEvent::Cancel => {
                    let mut targets = self.touched.remove(&root).unwrap_or_default();
                    for ((owner, _), target) in &self.contacts {
                        if *owner == root && !targets.contains(target) {
                            targets.push(*target);
                        }
                    }
                    if matches!(event, TouchEvent::Cancel) {
                        self.contacts.retain(|(owner, _), _| *owner != root);
                    }
                    return targets
                        .into_iter()
                        .map(|target| {
                            let mut envelope = envelope.clone();
                            envelope.session = target.session;
                            if let DestinationMessage::Input(input) = &mut envelope.message {
                                input.target = ClientInputTarget::Touch {
                                    surface: target.surface,
                                    layer: target.layer,
                                };
                            }
                            envelope
                        })
                        .collect();
                }
            },
            (ClientInputTarget::Pointer { .. }, event) => {
                let pointer = self.pointers.entry(root).or_default();
                let leaving = matches!(event, InputEventKind::PointerLeft { .. });
                let position = match event {
                    InputEventKind::PointerMotion { position, .. }
                    | InputEventKind::PointerLeft { position } => Some(position),
                    InputEventKind::PointerButton { position, .. }
                    | InputEventKind::PointerAxis { position, .. } => position.as_mut(),
                    _ => None,
                };
                if let Some(position) = position {
                    if !leaving
                        && pointer.buttons.is_empty()
                        && !pointer.gesture
                        && !pointer.scrolling
                    {
                        pointer.target = views.hit(root, *position);
                    }
                    if let Some(target) = pointer.target {
                        *position = target.local(*position);
                    }
                }
                let Some(target) = pointer.target else {
                    return Vec::new();
                };
                if let InputEventKind::PointerButton { button, state, .. } = event {
                    match state {
                        ButtonState::Pressed => {
                            pointer.buttons.insert(*button);
                        }
                        ButtonState::Released => {
                            pointer.buttons.remove(button);
                        }
                    }
                }
                if let InputEventKind::PointerGesture { gesture } = event {
                    if gesture.is_begin() {
                        pointer.gesture = true;
                    }
                    if gesture.is_end() {
                        pointer.gesture = false;
                    }
                }
                if let InputEventKind::PointerAxis { axis, .. } = event
                    && axis.source == RawScrollSource::Finger
                {
                    pointer.scrolling = !matches!(
                        axis.phase,
                        RawScrollPhase::Ended | RawScrollPhase::Cancelled
                    );
                }
                if leaving && pointer.buttons.is_empty() {
                    pointer.target = None;
                }
                target
            }
            _ => return vec![envelope],
        };
        envelope.session = target.session;
        input.target = match input.target {
            ClientInputTarget::Touch { .. } => {
                let touched = self.touched.entry(root).or_default();
                if !touched.contains(&target) {
                    touched.push(target);
                }
                ClientInputTarget::Touch {
                    surface: target.surface,
                    layer: target.layer,
                }
            }
            _ => ClientInputTarget::Pointer {
                surface: target.surface,
                layer: target.layer,
            },
        };
        vec![envelope]
    }

    pub fn remove(&mut self, surface: ClientSurfaceId) {
        self.pointers.retain(|root, pointer| {
            *root != surface
                && pointer
                    .target
                    .is_none_or(|target| target.surface != surface)
        });
        self.contacts
            .retain(|(root, _), target| *root != surface && target.surface != surface);
        self.touched.retain(|root, targets| {
            targets.retain(|target| target.surface != surface);
            *root != surface && !targets.is_empty()
        });
    }
}

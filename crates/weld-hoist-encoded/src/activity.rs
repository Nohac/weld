//! Port-local attention derived from authorized relay traffic, never client damage.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use weld_client::{
    ButtonState, ClientInputTarget, ClientRequest, ClientSurfaceId, ClientSurfaceRequestKind,
    ClientSurfaceRole, InputEventKind, InputPosition, KeyboardKeyState, LinuxButtonCode,
    PointerGesture, SurfaceBitratePreference, SurfaceLayerId, TouchpadHold, TouchpadPinch,
    TouchpadSwipe,
};
use weld_hoist_protocol::{DestinationMessage, HoistSessionId};

/// Scheduling preferences, not bitrate allocations or hardware-capacity estimates.
#[derive(Clone, Copy, Debug)]
pub struct SchedulingPolicy {
    /// How long a discrete interaction retains its boost after the last event.
    pub interaction_grace: Duration,
    /// Longest gap in a continuous sequence of changed pointer positions.
    pub motion_grace: Duration,
    /// Motion duration needed to reach interactive priority.
    pub motion_dwell: Duration,
    /// Override weighted choice after this much continuously observed ready time.
    pub starvation_age: Duration,
    /// Background, focused, moving, interactive weights. Zero is treated as one.
    pub weights: [u16; 4],
}

impl Default for SchedulingPolicy {
    fn default() -> Self {
        Self {
            interaction_grace: Duration::from_millis(750),
            motion_grace: Duration::from_millis(150),
            motion_dwell: Duration::from_millis(75),
            starvation_age: Duration::from_millis(100),
            weights: [1, 2, 4, 8],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Priority {
    Background,
    Focused,
    Moving,
    Interactive,
}

impl Priority {
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Background => 0,
            Self::Focused => 1,
            Self::Moving => 2,
            Self::Interactive => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct Group {
    pub session: HoistSessionId,
    pub root: ClientSurfaceId,
}

#[derive(Default)]
struct Attention {
    interaction: Option<Instant>,
    position: Option<InputPosition>,
    layer: Option<SurfaceLayerId>,
    motion_start: Option<Instant>,
    motion_last: Option<Instant>,
    quality_motion: Option<Instant>,
    buttons: HashSet<LinuxButtonCode>,
    resizing: bool,
}

struct Surface {
    session: HoistSessionId,
    owner: Option<ClientSurfaceId>,
    mapped: bool,
    attention: Attention,
    bitrate: Option<SurfaceBitratePreference>,
}

#[derive(Default)]
pub(crate) struct Activity {
    surfaces: HashMap<ClientSurfaceId, Surface>,
    focus: Option<ClientSurfaceId>,
}

/// Aggregated trusted observations. Consumers choose their own grace periods;
/// sustained motion is classified once using the window interaction policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct GroupAttention {
    pub focused: bool,
    pub interaction: Option<Instant>,
    pub sustained_motion: Option<Instant>,
    pub motion: Option<Instant>,
    /// Historical dwell-qualified motion, retained across continuity resets.
    pub quality_motion: Option<Instant>,
}

impl GroupAttention {
    pub(crate) fn priority(
        self,
        now: Instant,
        interaction_grace: Duration,
        motion_grace: Duration,
    ) -> Priority {
        let fresh = |at: Option<Instant>, grace| {
            at.is_some_and(|at| now.saturating_duration_since(at) < grace)
        };
        if fresh(self.interaction, interaction_grace) || fresh(self.sustained_motion, motion_grace)
        {
            Priority::Interactive
        } else if fresh(self.motion, motion_grace) {
            Priority::Moving
        } else if self.focused {
            Priority::Focused
        } else {
            Priority::Background
        }
    }
}

/// Reusable group observations; unmapped groups have no attention entry.
#[derive(Default)]
pub(crate) struct ActivitySnapshot {
    pub groups: HashMap<ClientSurfaceId, Group>,
    pub attention: HashMap<Group, GroupAttention>,
}

impl Activity {
    pub(crate) fn register(&mut self, session: HoistSessionId, surface: ClientSurfaceId) {
        if self
            .surfaces
            .get(&surface)
            .is_some_and(|entry| entry.session == session)
        {
            return;
        }
        self.remove(surface);
        self.surfaces.insert(
            surface,
            Surface {
                session,
                owner: None,
                mapped: false,
                attention: Attention::default(),
                bitrate: None,
            },
        );
    }

    pub(crate) fn role(&mut self, surface: ClientSurfaceId, role: ClientSurfaceRole) {
        if let Some(entry) = self.surfaces.get_mut(&surface) {
            let owner = match role {
                ClientSurfaceRole::Popup(popup) => Some(popup.owner),
                ClientSurfaceRole::Toplevel(_) => None,
            };
            if entry.owner != owner {
                entry.attention = Attention::default();
                entry.owner = owner;
            }
        }
    }

    pub(crate) fn set_bitrate_preference(
        &mut self,
        session: HoistSessionId,
        surface: ClientSurfaceId,
        preference: Option<SurfaceBitratePreference>,
    ) -> bool {
        let Some(entry) = self
            .surfaces
            .get_mut(&surface)
            .filter(|s| s.session == session)
        else {
            return false;
        };
        if entry.bitrate == preference {
            return false;
        }
        entry.bitrate = preference;
        true
    }

    /// Reuse validated popup ownership, including malformed-owner fallback.
    /// A popup's own hint cannot override its root's quality policy.
    pub(crate) fn bitrate_preference(
        &self,
        surface: ClientSurfaceId,
    ) -> Option<SurfaceBitratePreference> {
        self.surfaces.get(&self.group(surface)?.root)?.bitrate
    }

    pub(crate) fn mapped(&mut self, surface: ClientSurfaceId, mapped: bool) {
        if !mapped {
            self.clear_surface_attention(surface);
        }
        if let Some(entry) = self.surfaces.get_mut(&surface) {
            entry.mapped = mapped;
        }
    }

    fn clear_surface_attention(&mut self, surface: ClientSurfaceId) {
        if self.focus == Some(surface)
            || self
                .focus
                .and_then(|focus| self.group(focus))
                .is_some_and(|group| group.root == surface)
        {
            self.clear_focus();
        }
        let members = self
            .surfaces
            .keys()
            .copied()
            .filter(|member| {
                *member == surface
                    || self
                        .group(*member)
                        .is_some_and(|group| group.root == surface)
            })
            .collect::<Vec<_>>();
        for member in members {
            if let Some(entry) = self.surfaces.get_mut(&member) {
                entry.attention = Attention::default();
            }
        }
    }

    pub(crate) fn remove(&mut self, surface: ClientSurfaceId) {
        self.clear_surface_attention(surface);
        self.surfaces.remove(&surface);
    }

    pub(crate) fn clear_focus(&mut self) {
        let previous = self.focus.and_then(|focus| self.group(focus));
        self.focus = None;
        self.reset_group_motion(previous);
    }

    fn reset_group_motion(&mut self, group: Option<Group>) {
        let Some(group) = group else {
            return;
        };
        let members = self
            .surfaces
            .keys()
            .copied()
            .filter(|surface| self.group(*surface) == Some(group))
            .collect::<Vec<_>>();
        for surface in members {
            if let Some(entry) = self.surfaces.get_mut(&surface) {
                entry.attention.motion_start = None;
                entry.attention.motion_last = None;
                entry.attention.position = None;
            }
        }
    }

    pub(crate) fn group(&self, surface: ClientSurfaceId) -> Option<Group> {
        let entry = self.surfaces.get(&surface)?;
        let fallback = Group {
            session: entry.session,
            root: surface,
        };
        let mut root = surface;
        // Malformed/cyclic or cross-session ownership cannot aggregate priority.
        for _ in 0..self.surfaces.len() {
            let Some(current) = self.surfaces.get(&root) else {
                return Some(fallback);
            };
            if current.session != entry.session {
                return Some(fallback);
            }
            match current.owner {
                Some(owner) => root = owner,
                None => {
                    return Some(Group {
                        session: entry.session,
                        root,
                    });
                }
            }
        }
        Some(fallback)
    }

    pub(crate) fn snapshot(&self, policy: SchedulingPolicy, snapshot: &mut ActivitySnapshot) {
        snapshot.groups.clear();
        snapshot.attention.clear();
        for surface in self.surfaces.keys() {
            if let Some(group) = self.group(*surface) {
                snapshot.groups.insert(*surface, group);
            }
        }
        let focused = self
            .focus
            .and_then(|surface| snapshot.groups.get(&surface))
            .copied();
        for (surface, entry) in &self.surfaces {
            let Some(group) = snapshot.groups.get(surface).copied() else {
                continue;
            };
            if !entry.mapped {
                continue;
            }
            let summary = snapshot.attention.entry(group).or_default();
            summary.focused |= focused == Some(group);
            let attention = &entry.attention;
            summary.interaction = summary.interaction.max(attention.interaction);
            summary.quality_motion = summary.quality_motion.max(attention.quality_motion);
            let sustained = attention
                .motion_start
                .zip(attention.motion_last)
                .is_some_and(|(start, last)| {
                    last.saturating_duration_since(start) >= policy.motion_dwell
                });
            if sustained {
                summary.sustained_motion = summary.sustained_motion.max(attention.motion_last);
            } else {
                summary.motion = summary.motion.max(attention.motion_last);
            }
        }
    }

    pub(crate) fn observe(
        &mut self,
        session: HoistSessionId,
        message: &DestinationMessage,
        now: Instant,
        policy: SchedulingPolicy,
    ) {
        match message {
            DestinationMessage::Request(ClientRequest::Focus(focus)) => {
                let next = focus.surface.filter(|surface| {
                    self.surfaces
                        .get(surface)
                        .is_some_and(|entry| entry.session == session && entry.mapped)
                });
                if self.focus != next {
                    self.clear_focus();
                    self.reset_group_motion(next.and_then(|surface| self.group(surface)));
                    self.focus = next;
                }
            }
            DestinationMessage::Request(ClientRequest::ClearFocus) => self.clear_focus(),
            DestinationMessage::Request(ClientRequest::Surface(request)) => {
                if let ClientSurfaceRequestKind::Configure { resizing, .. } = request.kind
                    && let Some(entry) = self.surfaces.get_mut(&request.surface)
                    && entry.session == session
                    && entry.mapped
                {
                    if resizing && !entry.attention.resizing {
                        entry.attention.interaction = Some(now);
                    }
                    entry.attention.resizing = resizing;
                }
            }
            DestinationMessage::Input(input) => {
                let surface = input.target.surface();
                let focused = self.group(surface).is_some_and(|group| {
                    self.focus.and_then(|focus| self.group(focus)) == Some(group)
                });
                let Some(entry) = self
                    .surfaces
                    .get_mut(&surface)
                    .filter(|entry| entry.session == session && entry.mapped)
                else {
                    return;
                };
                let attention = &mut entry.attention;
                if let ClientInputTarget::Pointer { layer, .. } = input.target
                    && attention.layer != Some(layer)
                {
                    attention.position = None;
                    attention.motion_start = None;
                    attention.motion_last = None;
                    attention.layer = Some(layer);
                }
                match &input.event {
                    InputEventKind::Keyboard { state, .. } => {
                        if *state == KeyboardKeyState::Pressed
                            || (*state == KeyboardKeyState::Repeated && focused)
                        {
                            attention.interaction = Some(now);
                        }
                    }
                    InputEventKind::PointerButton {
                        button,
                        state,
                        position,
                    } => match state {
                        ButtonState::Pressed => {
                            // Attention is not the authoritative release ledger.
                            // Bound even malformed button streams without affecting delivery.
                            if attention.buttons.len() < 32 {
                                attention.buttons.insert(*button);
                            }
                            attention.interaction = Some(now);
                            attention.position = *position;
                        }
                        ButtonState::Released => {
                            attention.buttons.remove(button);
                        }
                    },
                    InputEventKind::PointerMotion { position } => {
                        let changed = position.x.is_finite()
                            && position.y.is_finite()
                            && attention
                                .position
                                .is_some_and(|previous| previous != *position);
                        attention.position = Some(*position);
                        if changed && (focused || !attention.buttons.is_empty()) {
                            if !attention.buttons.is_empty() {
                                attention.interaction = Some(now);
                            }
                            if attention.motion_last.is_none_or(|last| {
                                now.saturating_duration_since(last) >= policy.motion_grace
                            }) {
                                attention.motion_start = Some(now);
                            }
                            attention.motion_last = Some(now);
                            if attention.motion_start.is_some_and(|start| {
                                now.saturating_duration_since(start) >= policy.motion_dwell
                            }) {
                                attention.quality_motion = Some(now);
                            }
                        }
                    }
                    InputEventKind::PointerLeft { .. } => {
                        attention.position = None;
                        attention.motion_start = None;
                        attention.motion_last = None;
                    }
                    InputEventKind::PointerAxis { axis, .. } => {
                        if axis.phase != weld_client::RawScrollPhase::Cancelled
                            && (axis.horizontal != 0.0 || axis.vertical != 0.0)
                        {
                            attention.interaction = Some(now);
                        }
                    }
                    InputEventKind::PointerGesture { gesture } => {
                        let active = match gesture {
                            PointerGesture::Swipe(TouchpadSwipe::Begin { .. })
                            | PointerGesture::Pinch(TouchpadPinch::Begin { .. })
                            | PointerGesture::Hold(TouchpadHold::Begin { .. }) => true,
                            PointerGesture::Swipe(TouchpadSwipe::Update { delta }) => {
                                delta.x != 0.0 || delta.y != 0.0
                            }
                            PointerGesture::Pinch(TouchpadPinch::Update {
                                delta,
                                scale,
                                rotation,
                            }) => {
                                delta.x != 0.0
                                    || delta.y != 0.0
                                    || *scale != 1.0
                                    || *rotation != 0.0
                            }
                            _ => false,
                        };
                        if active {
                            attention.interaction = Some(now);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;

//! Persistent window slots matched to replacement clients within a session.

use std::collections::HashSet;

use bevy::{
    ecs::{
        component::Component,
        entity::Entity,
        event::Event,
        message::{Message, MessageWriter},
        observer::On,
        query::{With, Without},
        system::{Commands, Query, Res, SystemParam},
    },
    window::RequestRedraw,
};
use weld_app::surface::{ClientToplevelHints, ClientToplevelParent, ClientWindowMetadata};
use weld_client::{ClientSourceId, ClientSurfaceMetadata, ToplevelKind};

use crate::{
    FocusedWindow, ManagedWindow, WindowClientResolver, WindowOccupant, WindowPresentationOverride,
};

/// Identifier captured from the current occupant for exact replacement matching.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PersistenceMatch {
    /// Native app ID, or X11 class and instance; title is the final fallback.
    #[default]
    Auto,
    AppId,
    Class,
    Instance,
    Title,
}

/// Enable captures the occupant identity once; toggle enables or disables retention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceAction {
    Enable(PersistenceMatch),
    Disable,
    Toggle(PersistenceMatch),
}

/// Change an explicit window's persistence, or the focused window when omitted.
#[derive(Event, Clone, Copy, Debug)]
pub struct WindowPersistenceRequest {
    pub window: Option<Entity>,
    pub action: PersistenceAction,
}

/// Result of an interactive persistence toggle, including its captured identity.
#[derive(Message, Clone, Debug)]
pub struct WindowPersistenceToggled {
    pub window: Entity,
    pub enabled: bool,
    pub matching: String,
}

/// A retained layout slot and its source-scoped replacement identity.
#[derive(Component, Clone, Debug)]
pub struct PersistentWindow {
    source: ClientSourceId,
    identity: Identity,
    kind: ToplevelKind,
    parented: bool,
    label: String,
}

#[derive(Clone, Debug)]
enum Identity {
    AppId(String),
    Class {
        class: String,
        instance: Option<String>,
    },
    Instance(String),
    Title(String),
}

impl PersistentWindow {
    /// Description retained while the slot has no occupant.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Exact identifier comparisons used when selecting a replacement occupant.
    pub fn match_description(&self) -> String {
        match &self.identity {
            Identity::AppId(value) => format!("app_id = {value:?}"),
            Identity::Class {
                class,
                instance: Some(instance),
            } => format!("class = {class:?}\ninstance = {instance:?}"),
            Identity::Class {
                class,
                instance: None,
            } => format!("class = {class:?}"),
            Identity::Instance(value) => format!("instance = {value:?}"),
            Identity::Title(value) => format!("title = {value:?}"),
        }
    }
}

impl Identity {
    fn capture(metadata: &ClientSurfaceMetadata, field: PersistenceMatch) -> Option<Self> {
        let nonempty = |value: &str| (!value.is_empty()).then(|| value.to_owned());
        match field {
            PersistenceMatch::Auto => {
                if let Some(class) = metadata.x11_class().and_then(nonempty) {
                    Some(Self::Class {
                        class,
                        instance: metadata.x11_instance().and_then(nonempty),
                    })
                } else if let Some(id) = nonempty(metadata.app_id()) {
                    Some(Self::AppId(id))
                } else {
                    nonempty(metadata.title()).map(Self::Title)
                }
            }
            PersistenceMatch::AppId => metadata
                .x11_class()
                .is_none()
                .then(|| nonempty(metadata.app_id()))
                .flatten()
                .map(Self::AppId),
            PersistenceMatch::Class => {
                metadata
                    .x11_class()
                    .and_then(nonempty)
                    .map(|class| Self::Class {
                        class,
                        instance: None,
                    })
            }
            PersistenceMatch::Instance => metadata
                .x11_instance()
                .and_then(nonempty)
                .map(Self::Instance),
            PersistenceMatch::Title => nonempty(metadata.title()).map(Self::Title),
        }
    }

    fn matches(&self, metadata: &ClientSurfaceMetadata) -> bool {
        match self {
            Self::AppId(id) => metadata.x11_class().is_none() && metadata.app_id() == id,
            Self::Class { class, instance } => {
                metadata.x11_class() == Some(class.as_str())
                    && instance
                        .as_ref()
                        .is_none_or(|instance| metadata.x11_instance() == Some(instance.as_str()))
            }
            Self::Instance(instance) => metadata.x11_instance() == Some(instance.as_str()),
            Self::Title(title) => metadata.title() == title,
        }
    }
}

#[derive(SystemParam)]
pub(crate) struct PersistenceClients<'w, 's> {
    metadata: Query<'w, 's, &'static ClientWindowMetadata>,
    hints: Query<'w, 's, &'static ClientToplevelHints>,
    parents: Query<'w, 's, (), With<ClientToplevelParent>>,
}

#[derive(SystemParam)]
pub(crate) struct PersistenceFeedback<'w> {
    redraw: MessageWriter<'w, RequestRedraw>,
    toggles: MessageWriter<'w, WindowPersistenceToggled>,
}

pub(crate) fn request(
    event: On<WindowPersistenceRequest>,
    focus: Res<FocusedWindow>,
    windows: Query<Option<&PersistentWindow>, With<ManagedWindow>>,
    clients: WindowClientResolver,
    facts: PersistenceClients,
    mut commands: Commands,
    mut feedback: PersistenceFeedback,
) {
    let Some(window) = event.window.or(focus.entity()) else {
        return;
    };
    let Ok(persistent) = windows.get(window) else {
        return;
    };
    let field = match event.action {
        PersistenceAction::Disable | PersistenceAction::Toggle(_) if persistent.is_some() => {
            if matches!(event.action, PersistenceAction::Toggle(_))
                && let Some(persistent) = persistent
            {
                feedback.toggles.write(WindowPersistenceToggled {
                    window,
                    enabled: false,
                    matching: persistent.match_description(),
                });
            }
            commands.entity(window).remove::<PersistentWindow>();
            feedback.redraw.write(RequestRedraw);
            return;
        }
        PersistenceAction::Disable => return,
        PersistenceAction::Enable(_) if persistent.is_some() => return,
        PersistenceAction::Enable(field) | PersistenceAction::Toggle(field) => field,
    };
    let Some(client) = clients.mapped_client(window) else {
        return;
    };
    let Ok(metadata) = facts.metadata.get(client.entity()) else {
        return;
    };
    let Some(identity) = Identity::capture(&metadata.0, field) else {
        tracing::warn!(?field, "window has no usable identifier for persistence");
        return;
    };
    let persistent = PersistentWindow {
        source: client.surface().source(),
        identity,
        kind: facts
            .hints
            .get(client.entity())
            .map_or(ToplevelKind::Normal, |hints| hints.0.kind),
        parented: facts.parents.contains(client.entity()),
        label: if metadata.0.title().is_empty() {
            metadata.0.app_id()
        } else {
            metadata.0.title()
        }
        .to_owned(),
    };
    if matches!(event.action, PersistenceAction::Toggle(_)) {
        feedback.toggles.write(WindowPersistenceToggled {
            window,
            enabled: true,
            matching: persistent.match_description(),
        });
    }
    commands.entity(window).insert(persistent);
    feedback.redraw.write(RequestRedraw);
}

type VacantPersistentWindows<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static ManagedWindow, &'static PersistentWindow),
    (Without<WindowOccupant>, Without<WindowPresentationOverride>),
>;

pub(crate) type AvailablePersistentWindows<'w, 's> = Query<
    'w,
    's,
    (),
    (
        With<PersistentWindow>,
        Without<WindowOccupant>,
        Without<WindowPresentationOverride>,
    ),
>;

/// Admission reserves each candidate once before deferred occupancy updates land.
#[derive(SystemParam)]
pub(crate) struct PersistenceAdmission<'w, 's> {
    persistent_windows: VacantPersistentWindows<'w, 's>,
    clients: PersistenceClients<'w, 's>,
}

impl PersistenceAdmission<'_, '_> {
    pub(crate) fn take_match(
        &self,
        client: Entity,
        source: ClientSourceId,
        reserved: &mut HashSet<Entity>,
    ) -> Option<Entity> {
        let metadata = self.clients.metadata.get(client).ok()?;
        let kind = self
            .clients
            .hints
            .get(client)
            .map_or(ToplevelKind::Normal, |hints| hints.0.kind);
        let parented = self.clients.parents.contains(client);
        let (window, _, _) = self
            .persistent_windows
            .iter()
            .filter(|(window, _, persistence)| {
                !reserved.contains(window)
                    && persistence.source == source
                    && persistence.kind == kind
                    && persistence.parented == parented
                    && persistence.identity.matches(&metadata.0)
            })
            .min_by_key(|(_, managed, _)| managed.id)?;
        reserved.insert(window);
        Some(window)
    }
}

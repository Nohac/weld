//! Publish local frame demand independently of surface mapping and ownership.

use crate::{
    WindowClientResolver, WindowOutputIntersections, WindowPreferredOutput,
    WindowPresentationOverride, WindowVisibility, fullscreen::FullscreenOccluded,
};
use bevy::ecs::{
    entity::Entity,
    query::{Has, With},
    resource::Resource,
    system::{Query, ResMut},
};
use std::collections::HashMap;
use weld_app::{
    output::{OutputGeometry, PresentationRate},
    surface::{
        ClientPopup, ClientSurface, ClientToplevel, MappedSurface, SurfaceAction,
        SurfaceActionQueue, SurfaceId,
    },
};

#[derive(Resource, Default)]
pub(crate) struct PublishedActivity {
    previous: HashMap<SurfaceId, Option<PresentationRate>>,
    next: HashMap<SurfaceId, Option<PresentationRate>>,
}

type ActivityWindows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static WindowVisibility,
        &'static WindowOutputIntersections,
        &'static WindowPreferredOutput,
        Has<FullscreenOccluded>,
        Has<WindowPresentationOverride>,
    ),
>;

pub(crate) fn publish(
    windows: ActivityWindows,
    clients: WindowClientResolver,
    toplevels: Query<&ClientToplevel, With<MappedSurface>>,
    popups: Query<(&ClientSurface, &ClientPopup), With<MappedSurface>>,
    outputs: Query<&OutputGeometry>,
    mut published: ResMut<PublishedActivity>,
    mut actions: ResMut<SurfaceActionQueue>,
) {
    published.next.clear();
    // Hoisted sources stay mapped without a local occupant. Await the first
    // mapped frame before publishing demand so a remote bootstrap can finish.
    for toplevel in &toplevels {
        published.next.insert(toplevel.surface, None);
    }
    for (window, visibility, intersections, preferred, occluded, overridden) in &windows {
        let Some(client) = clients.mapped_client(window) else {
            continue;
        };
        if *visibility != WindowVisibility::Visible || occluded || overridden {
            continue;
        }
        let rate = preferred
            .entity()
            .and_then(|output| outputs.get(output).ok())
            .map(|output| output.presentation_rate())
            .or_else(|| {
                intersections
                    .iter()
                    .filter_map(|output| outputs.get(output).ok())
                    .map(|output| output.presentation_rate())
                    .max()
            });
        published.next.insert(client.surface(), rate);
    }
    for (surface, popup) in &popups {
        // Desktop-layer popups have their own presenter outside window policy.
        let Some(rate) = published.next.get(&popup.owner).copied() else {
            continue;
        };
        published.next.insert(surface.surface, rate);
    }
    for (&surface, &rate) in &published.next {
        if published.previous.get(&surface) != Some(&rate) {
            actions.push(SurfaceAction::SetPresentation { surface, rate });
        }
    }
    let PublishedActivity { previous, next } = &mut *published;
    std::mem::swap(previous, next);
}

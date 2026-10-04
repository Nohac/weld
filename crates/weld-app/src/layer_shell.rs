//! Desktop surface presentation and temporary launcher keyboard focus.

use crate::{
    layer,
    output::{OutputCompositionCamera, WeldOutput},
    surface::{
        ClientPopup, ClientSurface, MappedSurface, SurfaceAction, SurfaceId, SurfaceNode,
        SurfaceSystems, SurfaceView,
    },
};
use bevy::{
    app::{App, PreUpdate},
    picking::{
        Pickable,
        events::{Pointer, Press},
    },
    prelude::*,
    ui::{GlobalZIndex, UiTargetCamera},
};
use std::collections::{HashMap, HashSet};
use weld_client::{DesktopLayer, LayerKeyboardInteractivity, LayerSurfaceState};

/// Host-arranged output-local desktop role; ordinary window admission selects toplevels.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct ClientLayerSurface(pub LayerSurfaceState);

/// Output-local desktop presentation selected by the active shell policy.
#[derive(Component, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DesktopLayerVisibility {
    #[default]
    All,
    OverlayOnly,
    Hidden,
}

impl DesktopLayerVisibility {
    fn includes(self, layer: DesktopLayer) -> bool {
        match self {
            Self::All => true,
            Self::OverlayOnly => layer == DesktopLayer::Overlay,
            Self::Hidden => false,
        }
    }
}

#[derive(Component)]
struct LayerPresentation(SurfaceId);

#[derive(Resource, Default)]
struct LayerPresentations(HashMap<SurfaceId, (Entity, Placement)>);

#[derive(Clone, Copy, PartialEq)]
struct Placement {
    position: Vec2,
    size: Vec2,
    z: i32,
    camera: Entity,
    parent: Option<Entity>,
}

#[derive(Resource, Default)]
struct LayerFocus {
    ordinary: Option<SurfaceId>,
    on_demand: Option<SurfaceId>,
    exclusive: Option<SurfaceId>,
    applied: Option<SurfaceId>,
    reassert: bool,
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<LayerPresentations>()
        .init_resource::<LayerFocus>()
        .add_systems(
            PreUpdate,
            present_layers.in_set(SurfaceSystems::FallbackPresentation),
        )
        .add_observer(pointer_focus);
}

fn z_index(state: LayerSurfaceState) -> i32 {
    let base = match state.layer {
        DesktopLayer::Background => layer::BACKGROUND_Z_INDEX,
        DesktopLayer::Bottom => layer::BOTTOM_Z_INDEX,
        DesktopLayer::Top => layer::TOP_Z_INDEX,
        DesktopLayer::Overlay => layer::OVERLAY_Z_INDEX,
    };
    base + i32::try_from(state.stack_index.min(999_999)).unwrap_or(999_999)
}

fn present_layers(
    mut commands: Commands,
    layers: Query<(&ClientSurface, &ClientLayerSurface, &MappedSurface)>,
    popups: Query<(&ClientSurface, &ClientPopup, &MappedSurface)>,
    outputs: Query<(
        &WeldOutput,
        &OutputCompositionCamera,
        Option<&DesktopLayerVisibility>,
    )>,
    mut presentations: ResMut<LayerPresentations>,
    mut focus: ResMut<LayerFocus>,
) {
    let mut active = HashSet::new();
    let mut exclusive = None;
    for (surface, layer, mapped) in &layers {
        let Some(camera) = outputs.iter().find_map(|(output, camera, visibility)| {
            (output.id.raw() == layer.0.output.raw()
                && visibility
                    .copied()
                    .unwrap_or_default()
                    .includes(layer.0.layer))
            .then(|| camera.entity())
            .flatten()
        }) else {
            continue;
        };
        let position = Vec2::new(layer.0.position.x, layer.0.position.y);
        let z = z_index(layer.0);
        let parent = mount(
            &mut commands,
            &mut presentations,
            surface.surface,
            Placement {
                position,
                size: mapped.visual_size,
                z,
                camera,
                parent: None,
            },
        );
        active.insert(surface.surface);
        if layer.0.keyboard == LayerKeyboardInteractivity::Exclusive
            && layer.0.layer >= DesktopLayer::Top
            && exclusive.is_none_or(|(previous, _)| z > previous)
        {
            exclusive = Some((z, surface.surface));
        }
        let owner = surface.surface;
        for (surface, popup, mapped) in &popups {
            if popup.owner != owner {
                continue;
            }
            mount(
                &mut commands,
                &mut presentations,
                surface.surface,
                Placement {
                    position: popup.position + mapped.visual_offset,
                    size: mapped.visual_size,
                    z: popup.stack_index,
                    camera,
                    parent: Some(parent),
                },
            );
            active.insert(surface.surface);
        }
    }
    focus.exclusive = exclusive.map(|(_, surface)| surface);
    if focus.on_demand.is_some_and(|selected| {
        !active.contains(&selected)
            || !layers.iter().any(|(surface, role, _)| {
                surface.surface == selected && role.0.keyboard != LayerKeyboardInteractivity::None
            })
    }) {
        focus.on_demand = None;
    }
    presentations.0.retain(|surface, (entity, _)| {
        if active.contains(surface) {
            true
        } else {
            commands.entity(*entity).try_despawn();
            false
        }
    });
}

fn mount(
    commands: &mut Commands,
    presentations: &mut LayerPresentations,
    surface: SurfaceId,
    placement: Placement,
) -> Entity {
    let entity = if let Some((entity, previous)) = presentations.0.get_mut(&surface) {
        if *previous == placement {
            return *entity;
        }
        *previous = placement;
        *entity
    } else {
        let entity = commands
            .spawn((LayerPresentation(surface), Pickable::IGNORE))
            .id();
        // SurfaceNode owns its rendering/input children. Keep protocol popups
        // under the layout root so content synchronization retains their parent.
        commands.spawn((
            SurfaceNode {
                surface,
                view: SurfaceView::FullSurface,
            },
            Pickable::IGNORE,
            Node {
                position_type: PositionType::Absolute,
                ..default()
            },
            ChildOf(entity),
        ));
        presentations.0.insert(surface, (entity, placement));
        entity
    };
    let Placement {
        position,
        size,
        z,
        camera,
        parent,
    } = placement;
    if let Some(parent) = parent {
        commands.entity(entity).insert((ChildOf(parent), ZIndex(z)));
    } else {
        commands
            .entity(entity)
            .insert((GlobalZIndex(z), UiTargetCamera(camera)));
    }
    commands.entity(entity).insert((Node {
        position_type: PositionType::Absolute,
        left: px(position.x),
        top: px(position.y),
        width: px(size.x),
        height: px(size.y),
        ..default()
    },));
    entity
}

fn pointer_focus(
    event: On<Pointer<Press>>,
    parents: Query<&ChildOf>,
    roots: Query<&LayerPresentation>,
    layers: Query<(&ClientSurface, &ClientLayerSurface)>,
    popups: Query<(&ClientSurface, &ClientPopup)>,
    mut focus: ResMut<LayerFocus>,
) {
    if event.original_event_target() != event.entity {
        return;
    }
    let root = std::iter::once(event.entity)
        .chain(parents.iter_ancestors(event.entity))
        .find_map(|entity| roots.get(entity).ok());
    let surface = root.map(|root| {
        popups
            .iter()
            .find_map(|(surface, popup)| (surface.surface == root.0).then_some(popup.owner))
            .unwrap_or(root.0)
    });
    // The host clears keyboard routes on focus loss. A click must reassert
    // focus even when the retained layer selection has not changed.
    focus.reassert |= surface.is_some_and(|surface| {
        layers.iter().any(|(candidate, role)| {
            candidate.surface == surface && role.0.keyboard != LayerKeyboardInteractivity::None
        })
    });
    focus.on_demand = surface.filter(|surface| {
        layers.iter().any(|(candidate, role)| {
            candidate.surface == *surface && role.0.keyboard == LayerKeyboardInteractivity::OnDemand
        })
    });
}

pub(crate) fn resolve_focus_actions(
    world: &mut World,
    actions: Vec<SurfaceAction>,
) -> Vec<SurfaceAction> {
    let Some(mut focus) = world.get_resource_mut::<LayerFocus>() else {
        return actions;
    };
    let mut result = Vec::with_capacity(actions.len() + 1);
    let mut reassert = std::mem::take(&mut focus.reassert);
    for action in actions {
        if let SurfaceAction::Focus { surface } = action {
            focus.ordinary = surface;
            focus.on_demand = None;
            reassert = true;
        } else {
            result.push(action);
        }
    }
    let selected = focus.exclusive.or(focus.on_demand).or(focus.ordinary);
    if selected != focus.applied || reassert {
        focus.applied = selected;
        result.push(SurfaceAction::Focus { surface: selected });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::camera::NormalizedRenderTarget;
    use bevy::picking::{
        backend::HitData,
        pointer::{Location, PointerButton, PointerId},
    };

    #[test]
    fn layer_popups_share_the_owner_stack_and_retire_with_its_presentation() {
        let mut app = App::new();
        register(&mut app);
        let output = app
            .world_mut()
            .spawn(WeldOutput {
                id: crate::output::OutputId::new(1),
            })
            .id();
        app.world_mut().spawn(crate::output::RendersOutput(output));
        let owner = SurfaceId::for_test(1);
        let popup = SurfaceId::for_test(2);
        let mapped = MappedSurface {
            logical_size: Vec2::new(200.0, 30.0),
            visual_size: Vec2::new(200.0, 30.0),
            visual_offset: Vec2::ZERO,
            opaque: false,
            alpha_mode: Default::default(),
        };
        let layer = app
            .world_mut()
            .spawn((
                ClientSurface { surface: owner },
                ClientLayerSurface(LayerSurfaceState {
                    output: weld_client::ClientOutputId::new(1),
                    position: weld_client::LogicalPoint::new(40.0, 50.0),
                    layer: DesktopLayer::Top,
                    keyboard: LayerKeyboardInteractivity::None,
                    stack_index: 0,
                }),
                mapped,
            ))
            .id();
        app.world_mut().spawn((
            ClientSurface { surface: popup },
            ClientPopup {
                owner,
                position: Vec2::new(12.0, 30.0),
                stack_index: 17,
            },
            mapped,
        ));
        app.update();
        let roots = &app.world().resource::<LayerPresentations>().0;
        let parent = roots[&owner].0;
        let child = roots[&popup].0;
        assert_eq!(
            app.world()
                .get::<ChildOf>(child)
                .expect("popup parent")
                .parent(),
            parent
        );
        assert_eq!(app.world().get::<ZIndex>(child).expect("popup stack").0, 17);
        assert!(app.world().get::<GlobalZIndex>(child).is_none());
        assert_eq!(
            app.world().get::<Node>(child).expect("popup geometry").left,
            px(12.0)
        );
        app.update();
        assert_eq!(
            app.world().resource::<LayerPresentations>().0[&popup].0,
            child
        );
        app.world_mut()
            .entity_mut(output)
            .insert(DesktopLayerVisibility::OverlayOnly);
        app.update();
        assert!(app.world().resource::<LayerPresentations>().0.is_empty());
        app.world_mut()
            .get_mut::<ClientLayerSurface>(layer)
            .expect("layer")
            .0
            .layer = DesktopLayer::Overlay;
        app.world_mut()
            .get_mut::<ClientLayerSurface>(layer)
            .expect("layer")
            .0
            .keyboard = LayerKeyboardInteractivity::Exclusive;
        app.update();
        assert_eq!(app.world().resource::<LayerPresentations>().0.len(), 2);
        assert_eq!(app.world().resource::<LayerFocus>().exclusive, Some(owner));
        app.world_mut()
            .entity_mut(output)
            .insert(DesktopLayerVisibility::Hidden);
        app.update();
        assert!(app.world().resource::<LayerPresentations>().0.is_empty());
        assert!(app.world().resource::<LayerFocus>().exclusive.is_none());
        app.world_mut()
            .entity_mut(output)
            .remove::<DesktopLayerVisibility>();
        app.update();
        let roots = &app.world().resource::<LayerPresentations>().0;
        let parent = roots[&owner].0;
        let child = roots[&popup].0;
        app.world_mut().entity_mut(layer).remove::<MappedSurface>();
        app.update();
        assert!(app.world().get_entity(parent).is_err());
        assert!(app.world().get_entity(child).is_err());
        assert!(app.world().resource::<LayerPresentations>().0.is_empty());
    }

    #[test]
    fn clicking_a_launcher_input_child_reasserts_its_existing_keyboard_route() {
        for keyboard in [
            LayerKeyboardInteractivity::Exclusive,
            LayerKeyboardInteractivity::OnDemand,
        ] {
            let mut app = App::new();
            register(&mut app);
            let surface = SurfaceId::for_test(2);
            app.world_mut().spawn((
                ClientSurface { surface },
                ClientLayerSurface(LayerSurfaceState {
                    output: weld_client::ClientOutputId::new(1),
                    position: weld_client::LogicalPoint::ZERO,
                    layer: DesktopLayer::Overlay,
                    keyboard,
                    stack_index: 0,
                }),
            ));
            let root = app.world_mut().spawn(LayerPresentation(surface)).id();
            let input = app.world_mut().spawn(ChildOf(root)).id();
            {
                let mut focus = app.world_mut().resource_mut::<LayerFocus>();
                focus.exclusive =
                    (keyboard == LayerKeyboardInteractivity::Exclusive).then_some(surface);
                focus.on_demand =
                    (keyboard == LayerKeyboardInteractivity::OnDemand).then_some(surface);
                focus.applied = Some(surface);
            }
            app.world_mut().trigger(Pointer::new(
                PointerId::Mouse,
                Location {
                    target: NormalizedRenderTarget::None {
                        width: 800,
                        height: 600,
                    },
                    position: Vec2::ZERO,
                },
                Press {
                    button: PointerButton::Primary,
                    hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
                    count: 1,
                },
                input,
            ));
            assert_eq!(
                resolve_focus_actions(app.world_mut(), vec![]),
                vec![SurfaceAction::Focus {
                    surface: Some(surface)
                }]
            );
            assert!(resolve_focus_actions(app.world_mut(), vec![]).is_empty());
        }
    }

    #[test]
    fn exclusive_launchers_restore_the_latest_window_choice_without_reasserting_each_frame() {
        let mut world = World::new();
        world.init_resource::<LayerFocus>();
        let ordinary = SurfaceId::for_test(1);
        let launcher = SurfaceId::for_test(2);
        let other = SurfaceId::for_test(3);
        let action = |surface| SurfaceAction::Focus {
            surface: Some(surface),
        };
        assert_eq!(
            resolve_focus_actions(&mut world, vec![action(ordinary)]),
            vec![action(ordinary)]
        );
        world.resource_mut::<LayerFocus>().exclusive = Some(launcher);
        assert_eq!(
            resolve_focus_actions(&mut world, vec![]),
            vec![action(launcher)]
        );
        assert!(resolve_focus_actions(&mut world, vec![]).is_empty());
        world.resource_mut::<LayerFocus>().reassert = true;
        assert_eq!(
            resolve_focus_actions(&mut world, vec![]),
            vec![action(launcher)]
        );
        assert!(resolve_focus_actions(&mut world, vec![]).is_empty());
        assert_eq!(
            resolve_focus_actions(&mut world, vec![action(other)]),
            vec![action(launcher)]
        );
        world.resource_mut::<LayerFocus>().exclusive = None;
        assert_eq!(
            resolve_focus_actions(&mut world, vec![]),
            vec![action(other)]
        );
        world.resource_mut::<LayerFocus>().on_demand = Some(launcher);
        assert_eq!(
            resolve_focus_actions(&mut world, vec![]),
            vec![action(launcher)]
        );
        assert_eq!(
            resolve_focus_actions(&mut world, vec![action(other)]),
            vec![action(other)]
        );
    }
}

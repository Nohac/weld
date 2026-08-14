//! Picking, implicit grabs, and protocol-neutral client input effects.

use std::collections::{HashSet, VecDeque};

use bevy::{
    app::{App, PreUpdate},
    ecs::{
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Query, Res, ResMut, SystemParam},
        world::World,
    },
    math::Vec2,
    picking::{
        PickingSystems,
        pointer::{PointerId, PointerInteraction, PointerLocation},
    },
    ui::{ComputedNode, Display, Node},
};
use leafwing_input_manager::plugin::InputManagerSystem;
use tracing::trace;

use super::{
    InputSystems,
    raw::{ButtonState, InputPosition, LinuxButtonCode, RawSeatEvent, RawSeatEventKind},
    shortcuts::consume_shortcut_event,
    state::{ConsumedShortcutKeys, InputUpdateTime, PendingSeatInput, PointerPositionState},
};
use crate::surface::{SurfaceId, SurfaceInputNode, SurfaceLayerId};
use weld_core::input::{SeatInputEffect, SeatInputEffectKind, SurfaceHit};

pub(super) fn register(app: &mut App) {
    app.init_resource::<InputEffects>()
        .init_resource::<PointerRoutingState>()
        .add_systems(
            PreUpdate,
            resolve_input_effects
                .in_set(InputSystems::Resolve)
                .in_set(PickingSystems::PostHover)
                .after(InputManagerSystem::Update),
        );
}

#[derive(Resource, Default)]
struct InputEffects(VecDeque<SeatInputEffect>);

#[derive(Clone, Copy, Debug, PartialEq)]
struct ResolvedPointer {
    position: InputPosition,
    target: Option<SurfaceHit>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ImplicitPointerGrab {
    surface: SurfaceId,
    layer: SurfaceLayerId,
    origin: InputPosition,
}

impl ImplicitPointerGrab {
    fn new(position: InputPosition, hit: SurfaceHit) -> Self {
        Self {
            surface: hit.surface,
            layer: hit.layer,
            origin: InputPosition::new(
                position.x - hit.local_position.x,
                position.y - hit.local_position.y,
            ),
        }
    }

    fn target_at(self, position: InputPosition) -> SurfaceHit {
        SurfaceHit {
            surface: self.surface,
            layer: self.layer,
            local_position: InputPosition::new(
                position.x - self.origin.x,
                position.y - self.origin.y,
            ),
        }
    }
}

#[derive(Resource, Default)]
struct PointerRoutingState {
    pointer: PointerPositionState,
    last_sent: Option<ResolvedPointer>,
    pressed_buttons: HashSet<LinuxButtonCode>,
    implicit_grab: Option<ImplicitPointerGrab>,
}

pub(crate) fn take_input_effects(world: &mut World) -> Vec<SeatInputEffect> {
    world
        .get_resource_mut::<InputEffects>()
        .map(|mut effects| effects.0.drain(..).collect())
        .unwrap_or_default()
}

fn resolve_input_effects(
    pointers: Query<(&PointerId, &PointerInteraction, &PointerLocation)>,
    picked_nodes: Query<(Option<&SurfaceInputNode>, Option<&ComputedNode>)>,
    surface_nodes: Query<(&SurfaceInputNode, &Node)>,
    update_time: Res<InputUpdateTime>,
    resources: InputRoutingResources,
) {
    let InputRoutingResources {
        mut pending,
        mut effects,
        mut routing,
        mut consumed_shortcuts,
    } = resources;
    // A runtime scale change can leave picking on the previous ComputedNode
    // layout for this update; Bevy refreshes UI layout later in PostUpdate.
    // The following update reconciles a stationary pointer to the new hit.
    let geometric_target = pointers
        .iter()
        .find(|(pointer, _, _)| **pointer == PointerId::Mouse)
        .and_then(|(_, interaction, location)| {
            location.location().and_then(|_| {
                resolve_pick_candidates(interaction.iter().map(|(entity, hit)| {
                    let (surface, node) = picked_nodes.get(*entity).unwrap_or((None, None));
                    PickCandidate {
                        target: surface.copied(),
                        centered_position: hit.position.map(|position| {
                            InputPosition::new(position.x.into(), position.y.into())
                        }),
                        size: node.map(logical_pick_size),
                    }
                }))
            })
        });
    if routing.implicit_grab.is_some_and(|grab| {
        !surface_nodes.iter().any(|(surface, node)| {
            surface.surface == grab.surface
                && surface.layer == grab.layer
                && node.display != Display::None
        })
    }) {
        routing.implicit_grab = None;
    }

    // Pointer positions and grab-relative targets replay per raw event. A
    // non-grab SurfaceHit still comes from Bevy's single end-of-update pick,
    // so its local position can be sampled later than the replayed position.
    for raw_event in pending.0.drain(..) {
        if consume_shortcut_event(&mut consumed_shortcuts.0, &raw_event) {
            continue;
        }
        if let Some(effect) = replay_seat_event(&mut routing, geometric_target, raw_event) {
            effects.0.push_back(effect);
        }
    }

    refresh_grab_origin(&mut routing, geometric_target);
    let current = resolved_pointer(&routing, geometric_target);
    if let Some(effect) = reconcile_pointer(routing.last_sent, current, update_time.0) {
        effects.0.push_back(effect);
        routing.last_sent = current;
    }
}

#[derive(SystemParam)]
struct InputRoutingResources<'w> {
    pending: ResMut<'w, PendingSeatInput>,
    effects: ResMut<'w, InputEffects>,
    routing: ResMut<'w, PointerRoutingState>,
    consumed_shortcuts: ResMut<'w, ConsumedShortcutKeys>,
}

fn replay_seat_event(
    routing: &mut PointerRoutingState,
    geometric_target: Option<SurfaceHit>,
    raw_event: RawSeatEvent,
) -> Option<SeatInputEffect> {
    let RawSeatEvent { event, time } = raw_event;
    match event {
        RawSeatEventKind::PointerMotion { position } => {
            routing.pointer.apply(position);
            let current = resolved_pointer(routing, geometric_target)?;
            routing.last_sent = Some(current);
            Some(pointer_motion_effect(current, time))
        }
        RawSeatEventKind::PointerLeft { position } => {
            routing.pointer.apply(position);
            if routing.implicit_grab.is_some() {
                let current = resolved_pointer(routing, geometric_target)?;
                routing.last_sent = Some(current);
                Some(pointer_motion_effect(current, time))
            } else {
                routing.pointer.clear_host();
                routing.last_sent = None;
                Some(SeatInputEffect::new(
                    SeatInputEffectKind::PointerMotion {
                        position,
                        target: None,
                    },
                    time,
                ))
            }
        }
        RawSeatEventKind::PointerButton {
            position,
            button,
            state,
        } => {
            if let Some(position) = position {
                routing.pointer.apply(position);
            }
            let position = position.unwrap_or(routing.pointer.last_known_position);
            if state == ButtonState::Pressed {
                let first_button = routing.pressed_buttons.is_empty();
                routing.pressed_buttons.insert(button);
                if first_button {
                    routing.implicit_grab =
                        geometric_target.map(|target| ImplicitPointerGrab::new(position, target));
                }
            }
            let target = routed_target(routing, geometric_target, position);
            let effect = SeatInputEffect::new(
                SeatInputEffectKind::PointerButton {
                    position,
                    target,
                    button,
                    state,
                },
                time,
            );
            routing.last_sent = routing
                .pointer
                .host_position
                .map(|position| ResolvedPointer { position, target });
            if state == ButtonState::Released {
                routing.pressed_buttons.remove(&button);
                if routing.pressed_buttons.is_empty() {
                    routing.implicit_grab = None;
                }
            }
            Some(effect)
        }
        RawSeatEventKind::PointerAxis { position, axis } => {
            if let Some(position) = position {
                routing.pointer.apply(position);
            }
            let position = position.unwrap_or(routing.pointer.last_known_position);
            let target = routed_target(routing, geometric_target, position);
            trace!(?axis, ?target, "routed raw pointer axis");
            Some(SeatInputEffect::new(
                SeatInputEffectKind::PointerAxis { axis },
                time,
            ))
        }
        RawSeatEventKind::PointerGesture { gesture } => Some(SeatInputEffect::new(
            SeatInputEffectKind::PointerGesture { gesture },
            time,
        )),
        RawSeatEventKind::Keyboard { keycode, state, .. } => Some(SeatInputEffect::new(
            SeatInputEffectKind::Keyboard { keycode, state },
            time,
        )),
        RawSeatEventKind::HostFocusLost => {
            routing.pointer.clear_host();
            clear_pointer_routing(routing);
            Some(SeatInputEffect::new(
                SeatInputEffectKind::HostFocusLost,
                time,
            ))
        }
    }
}

fn clear_pointer_routing(routing: &mut PointerRoutingState) {
    routing.pressed_buttons.clear();
    routing.implicit_grab = None;
    routing.last_sent = None;
}

fn logical_pick_size(node: &ComputedNode) -> Vec2 {
    node.size() * node.inverse_scale_factor()
}

#[derive(Clone, Copy)]
struct PickCandidate {
    target: Option<SurfaceInputNode>,
    centered_position: Option<InputPosition>,
    size: Option<Vec2>,
}

/// The first interaction is authoritative with Weld's single UI picking
/// backend. Revisit this if another picking backend introduces cross-backend
/// ordering ties.
fn resolve_pick_candidates(
    candidates: impl IntoIterator<Item = PickCandidate>,
) -> Option<SurfaceHit> {
    let candidate = candidates.into_iter().next()?;
    let target = candidate.target?;
    let position = candidate.centered_position?;
    let size = candidate.size?;
    Some(SurfaceHit {
        surface: target.surface,
        layer: target.layer,
        // `size` is surface-logical because SurfacePlugin explicitly
        // sizes each node to its committed viewport destination (or the full
        // logical buffer when no viewport exists). That invariant keeps Bevy
        // transforms and clipping composable while the protocol receives
        // coordinates in the client's logical space.
        local_position: InputPosition::new(
            (position.x + 0.5) * f64::from(size.x) + f64::from(target.local_origin.x),
            (position.y + 0.5) * f64::from(size.y) + f64::from(target.local_origin.y),
        ),
    })
}

fn refresh_grab_origin(routing: &mut PointerRoutingState, geometric: Option<SurfaceHit>) {
    let (Some(position), Some(grab), Some(hit)) = (
        routing.pointer.host_position,
        routing.implicit_grab.as_mut(),
        geometric,
    ) else {
        return;
    };
    if grab.surface == hit.surface && grab.layer == hit.layer {
        *grab = ImplicitPointerGrab::new(position, hit);
    }
}

fn routed_target(
    routing: &PointerRoutingState,
    geometric: Option<SurfaceHit>,
    position: InputPosition,
) -> Option<SurfaceHit> {
    routing
        .implicit_grab
        .map(|grab| grab.target_at(position))
        .or_else(|| {
            routing
                .pressed_buttons
                .is_empty()
                .then_some(geometric)
                .flatten()
        })
}

fn resolved_pointer(
    routing: &PointerRoutingState,
    geometric: Option<SurfaceHit>,
) -> Option<ResolvedPointer> {
    routing
        .pointer
        .host_position
        .map(|position| ResolvedPointer {
            position,
            target: routed_target(routing, geometric, position),
        })
}

fn pointer_motion_effect(pointer: ResolvedPointer, time: u32) -> SeatInputEffect {
    SeatInputEffect::new(
        SeatInputEffectKind::PointerMotion {
            position: pointer.position,
            target: pointer.target,
        },
        time,
    )
}

fn reconcile_pointer(
    previous: Option<ResolvedPointer>,
    current: Option<ResolvedPointer>,
    time: u32,
) -> Option<SeatInputEffect> {
    if previous == current {
        return None;
    }
    match current {
        Some(current) => Some(pointer_motion_effect(current, time)),
        None => previous.map(|previous| {
            SeatInputEffect::new(
                SeatInputEffectKind::PointerMotion {
                    position: previous.position,
                    target: None,
                },
                time,
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use bevy::{math::Vec2, ui::ComputedNode};

    use super::*;
    use crate::input::raw::{
        InputPosition, LinuxButtonCode, RawScrollFrame, RawScrollPhase, RawSeatEvent,
        RawSeatEventKind,
    };

    #[test]
    fn host_focus_loss_clears_pointer_grabs() {
        let surface = SurfaceId::new(3);
        let mut routing = PointerRoutingState {
            last_sent: Some(ResolvedPointer {
                position: InputPosition::new(4.0, 5.0),
                target: None,
            }),
            implicit_grab: Some(ImplicitPointerGrab {
                surface,
                layer: SurfaceLayerId::new(1),
                origin: InputPosition::default(),
            }),
            ..Default::default()
        };
        routing.pressed_buttons.insert(LinuxButtonCode(0x110));

        clear_pointer_routing(&mut routing);

        assert!(routing.pressed_buttons.is_empty());
        assert_eq!(routing.implicit_grab, None);
        assert_eq!(routing.last_sent, None);
    }

    fn surface_candidate(
        surface: SurfaceId,
        centered_position: InputPosition,
        size: Vec2,
    ) -> PickCandidate {
        PickCandidate {
            target: Some(SurfaceInputNode {
                surface,
                layer: SurfaceLayerId::new(1),
                local_origin: Vec2::ZERO,
            }),
            centered_position: Some(centered_position),
            size: Some(size),
        }
    }

    #[test]
    fn topmost_surface_pick_is_authoritative() {
        let top = SurfaceId::new(1);
        let lower = SurfaceId::new(2);
        let hit = resolve_pick_candidates([
            surface_candidate(top, InputPosition::default(), Vec2::splat(100.0)),
            surface_candidate(lower, InputPosition::default(), Vec2::splat(100.0)),
        ])
        .expect("top surface should be routed");

        assert_eq!(hit.surface, top);
    }

    #[test]
    fn topmost_overlay_blocks_the_surface_below() {
        let overlay = PickCandidate {
            target: None,
            centered_position: Some(InputPosition::default()),
            size: Some(Vec2::splat(100.0)),
        };
        let surface = surface_candidate(
            SurfaceId::new(1),
            InputPosition::default(),
            Vec2::splat(100.0),
        );

        assert_eq!(resolve_pick_candidates([overlay, surface]), None);
    }

    #[test]
    fn centered_ui_coordinates_convert_to_surface_pixels() {
        let surface = SurfaceId::new(1);
        let top_left = resolve_pick_candidates([surface_candidate(
            surface,
            InputPosition::new(-0.5, -0.5),
            Vec2::new(640.0, 480.0),
        )])
        .expect("corner should be inside the picked surface");
        let transformed = resolve_pick_candidates([surface_candidate(
            surface,
            InputPosition::new(0.25, -0.25),
            Vec2::new(640.0, 480.0),
        )])
        .expect("transformed local hit should be routed");
        let bottom_right = resolve_pick_candidates([surface_candidate(
            surface,
            InputPosition::new(0.5, 0.5),
            Vec2::new(640.0, 480.0),
        )])
        .expect("corner should be inside the picked surface");

        assert_eq!(top_left.local_position, InputPosition::new(0.0, 0.0));
        assert_eq!(transformed.local_position, InputPosition::new(480.0, 120.0));
        assert_eq!(
            bottom_right.local_position,
            InputPosition::new(640.0, 480.0)
        );
    }

    #[test]
    fn input_region_origin_is_restored_for_client_pointer_coordinates() {
        let surface = SurfaceId::new(1);
        let hit = resolve_pick_candidates([PickCandidate {
            target: Some(SurfaceInputNode {
                surface,
                layer: SurfaceLayerId::new(1),
                local_origin: Vec2::new(24.0, 32.0),
            }),
            centered_position: Some(InputPosition::new(-0.5, -0.5)),
            size: Some(Vec2::new(640.0, 480.0)),
        }])
        .expect("window geometry should remain pickable");

        assert_eq!(hit.local_position, InputPosition::new(24.0, 32.0));
    }

    #[test]
    fn physical_computed_node_size_becomes_surface_logical_for_hits() {
        let node = ComputedNode {
            size: Vec2::new(800.0, 600.0),
            // Reciprocal of the host scale 1.25 used by this scaling slice.
            inverse_scale_factor: 0.8,
            ..Default::default()
        };

        assert_eq!(logical_pick_size(&node), Vec2::new(640.0, 480.0));
    }

    #[test]
    fn losing_the_last_hit_emits_pointer_focus_clear() {
        let previous = ResolvedPointer {
            position: InputPosition::new(42.0, 24.0),
            target: Some(SurfaceHit {
                surface: SurfaceId::new(1),
                layer: SurfaceLayerId::new(1),
                local_position: InputPosition::new(2.0, 3.0),
            }),
        };

        assert_eq!(
            reconcile_pointer(Some(previous), None, 9),
            Some(SeatInputEffect::new(
                SeatInputEffectKind::PointerMotion {
                    position: previous.position,
                    target: None,
                },
                9,
            ))
        );
    }

    #[test]
    fn unchanged_pointer_does_not_emit_reconciliation_motion() {
        let current = ResolvedPointer {
            position: InputPosition::new(5.0, 6.0),
            target: None,
        };

        assert_eq!(reconcile_pointer(Some(current), Some(current), 10), None);
    }

    #[test]
    fn stationary_pointer_entering_a_surface_emits_motion() {
        let current = ResolvedPointer {
            position: InputPosition::new(50.0, 60.0),
            target: Some(SurfaceHit {
                surface: SurfaceId::new(2),
                layer: SurfaceLayerId::new(1),
                local_position: InputPosition::new(10.0, 20.0),
            }),
        };

        assert_eq!(
            reconcile_pointer(None, Some(current), 11),
            Some(pointer_motion_effect(current, 11))
        );
    }

    #[test]
    fn implicit_grab_keeps_routing_outside_the_surface() {
        let surface = SurfaceId::new(7);
        let mut routing = PointerRoutingState {
            pointer: PointerPositionState {
                host_position: Some(InputPosition::new(90.0, 100.0)),
                last_known_position: InputPosition::new(90.0, 100.0),
            },
            implicit_grab: Some(ImplicitPointerGrab::new(
                InputPosition::new(30.0, 40.0),
                SurfaceHit {
                    surface,
                    layer: SurfaceLayerId::new(1),
                    local_position: InputPosition::new(10.0, 15.0),
                },
            )),
            ..Default::default()
        };
        routing.pressed_buttons.insert(LinuxButtonCode(0x110));

        assert_eq!(
            routed_target(&routing, None, routing.pointer.host_position.unwrap()),
            Some(SurfaceHit {
                surface,
                layer: SurfaceLayerId::new(1),
                local_position: InputPosition::new(70.0, 75.0),
            })
        );
    }

    #[test]
    fn press_without_surface_grab_swallows_client_routing() {
        let mut routing = PointerRoutingState::default();
        routing.pressed_buttons.insert(LinuxButtonCode(0x110));
        let geometric = SurfaceHit {
            surface: SurfaceId::new(9),
            layer: SurfaceLayerId::new(1),
            local_position: InputPosition::new(1.0, 2.0),
        };

        assert_eq!(
            routed_target(&routing, Some(geometric), InputPosition::new(5.0, 6.0)),
            None
        );
    }

    #[test]
    fn final_grab_release_reconciles_to_pointer_leave() {
        let position = InputPosition::new(90.0, 100.0);
        let grabbed = ResolvedPointer {
            position,
            target: Some(SurfaceHit {
                surface: SurfaceId::new(10),
                layer: SurfaceLayerId::new(1),
                local_position: InputPosition::new(70.0, 75.0),
            }),
        };
        let routing = PointerRoutingState {
            pointer: PointerPositionState {
                host_position: Some(position),
                last_known_position: position,
            },
            ..Default::default()
        };
        let current = resolved_pointer(&routing, None);

        assert_eq!(
            reconcile_pointer(Some(grabbed), current, 12),
            Some(SeatInputEffect::new(
                SeatInputEffectKind::PointerMotion {
                    position,
                    target: None,
                },
                12,
            ))
        );
    }

    #[test]
    fn grabbed_motion_batch_replays_each_position_before_reanchoring() {
        let surface = SurfaceId::new(11);
        let initial_position = InputPosition::new(10.0, 10.0);
        let mut routing = PointerRoutingState {
            pointer: PointerPositionState {
                host_position: Some(initial_position),
                last_known_position: initial_position,
            },
            implicit_grab: Some(ImplicitPointerGrab::new(
                initial_position,
                SurfaceHit {
                    surface,
                    layer: SurfaceLayerId::new(1),
                    local_position: InputPosition::new(2.0, 3.0),
                },
            )),
            ..Default::default()
        };
        routing.pressed_buttons.insert(LinuxButtonCode(0x110));
        let first_position = InputPosition::new(20.0, 20.0);
        let final_position = InputPosition::new(30.0, 25.0);

        let effects = [
            replay_seat_event(
                &mut routing,
                None,
                RawSeatEvent::new(
                    RawSeatEventKind::PointerMotion {
                        position: first_position,
                    },
                    2,
                ),
            )
            .expect("grabbed motion should route"),
            replay_seat_event(
                &mut routing,
                None,
                RawSeatEvent::new(
                    RawSeatEventKind::PointerMotion {
                        position: final_position,
                    },
                    3,
                ),
            )
            .expect("grabbed motion should route"),
        ];

        assert_eq!(
            effects,
            [
                SeatInputEffect::new(
                    SeatInputEffectKind::PointerMotion {
                        position: first_position,
                        target: Some(SurfaceHit {
                            surface,
                            layer: SurfaceLayerId::new(1),
                            local_position: InputPosition::new(12.0, 13.0),
                        }),
                    },
                    2,
                ),
                SeatInputEffect::new(
                    SeatInputEffectKind::PointerMotion {
                        position: final_position,
                        target: Some(SurfaceHit {
                            surface,
                            layer: SurfaceLayerId::new(1),
                            local_position: InputPosition::new(22.0, 18.0),
                        }),
                    },
                    3,
                ),
            ]
        );

        let final_hit = SurfaceHit {
            surface,
            layer: SurfaceLayerId::new(1),
            local_position: InputPosition::new(40.0, 50.0),
        };
        refresh_grab_origin(&mut routing, Some(final_hit));
        assert_eq!(
            routing
                .implicit_grab
                .expect("grab should remain active")
                .target_at(final_position),
            final_hit
        );
    }

    #[test]
    fn agreeing_geometric_hit_refreshes_grab_origin() {
        let surface = SurfaceId::new(8);
        let mut routing = PointerRoutingState {
            pointer: PointerPositionState {
                host_position: Some(InputPosition::new(80.0, 90.0)),
                last_known_position: InputPosition::new(80.0, 90.0),
            },
            implicit_grab: Some(ImplicitPointerGrab {
                surface,
                layer: SurfaceLayerId::new(1),
                origin: InputPosition::default(),
            }),
            ..Default::default()
        };
        let hit = SurfaceHit {
            surface,
            layer: SurfaceLayerId::new(1),
            local_position: InputPosition::new(20.0, 30.0),
        };

        refresh_grab_origin(&mut routing, Some(hit));

        assert_eq!(
            routing
                .implicit_grab
                .unwrap()
                .target_at(routing.pointer.host_position.unwrap()),
            hit
        );
    }

    #[test]
    fn pointer_axis_does_not_absorb_a_pending_focus_reconciliation() {
        let position = InputPosition::new(80.0, 90.0);
        let previous = ResolvedPointer {
            position,
            target: Some(SurfaceHit {
                surface: SurfaceId::new(1),
                layer: SurfaceLayerId::new(1),
                local_position: InputPosition::new(10.0, 20.0),
            }),
        };
        let next = SurfaceHit {
            surface: SurfaceId::new(2),
            layer: SurfaceLayerId::new(1),
            local_position: InputPosition::new(30.0, 40.0),
        };
        let mut routing = PointerRoutingState {
            pointer: PointerPositionState {
                host_position: Some(position),
                last_known_position: position,
            },
            last_sent: Some(previous),
            ..Default::default()
        };

        let effect = replay_seat_event(
            &mut routing,
            Some(next),
            RawSeatEvent::new(
                RawSeatEventKind::PointerAxis {
                    position: Some(position),
                    axis: RawScrollFrame {
                        source: crate::input::raw::RawScrollSource::Wheel,
                        phase: RawScrollPhase::Moved,
                        horizontal: 0.0,
                        vertical: 15.0,
                        horizontal_v120: None,
                        vertical_v120: Some(120),
                        horizontal_stop: false,
                        vertical_stop: false,
                    },
                },
                11,
            ),
        );

        assert!(matches!(
            effect,
            Some(SeatInputEffect {
                event: SeatInputEffectKind::PointerAxis { .. },
                time: 11,
            })
        ));
        assert_eq!(routing.last_sent, Some(previous));
        assert_eq!(
            reconcile_pointer(
                routing.last_sent,
                resolved_pointer(&routing, Some(next)),
                11
            ),
            Some(pointer_motion_effect(
                ResolvedPointer {
                    position,
                    target: Some(next),
                },
                11,
            ))
        );
    }
}

use futures_lite::future::{block_on, poll_once};
use weld_client::{
    ButtonState, ClientId, ClientSourceId, ClientSurfaceId, InputPosition, LinuxButtonCode,
    LinuxKeycode, RawScrollFrame, RawScrollPhase, RawScrollSource, SurfaceLayerId,
    WireClientInputEvent,
};
use weld_hoist_protocol::HoistSessionId;

use super::*;

#[test]
fn gamepad_analog_coalesces_but_press_release_and_end_remain_ordered() {
    block_on(async {
        use weld_hoist_protocol::gamepad::{GAMEPAD_SESSION, GamepadRequest, GamepadState};
        let queue = InputOutbox::default();
        let packet = |request| DestinationEnvelope {
            session: GAMEPAD_SESSION,
            message: DestinationMessage::Gamepad(request),
        };
        let mut state = GamepadState::default();
        for axis in 1..100 {
            state.left[0] = axis;
            queue
                .push(packet(GamepadRequest::State {
                    generation: 1,
                    state,
                }))
                .expect("analog");
        }
        state.buttons.south = true;
        queue
            .push(packet(GamepadRequest::State {
                generation: 1,
                state,
            }))
            .expect("press");
        state.buttons.south = false;
        queue
            .push(packet(GamepadRequest::State {
                generation: 1,
                state,
            }))
            .expect("release");
        queue
            .push(packet(GamepadRequest::End { generation: 1 }))
            .expect("end");
        assert_eq!(queue.state.lock().expect("state").records.len(), 4);
        for pressed in [false, true, false] {
            assert!(matches!(queue.recv().await.expect("state").message,
            DestinationMessage::Gamepad(GamepadRequest::State { state, .. })
            if state.buttons.south == pressed && state.left[0] == 99));
        }
        assert!(matches!(
            queue.recv().await.expect("end").message,
            DestinationMessage::Gamepad(GamepadRequest::End { generation: 1 })
        ));
    });
}

fn motion(time: u32) -> DestinationEnvelope {
    DestinationEnvelope {
        session: HoistSessionId::new(1),
        message: DestinationMessage::Input(WireClientInputEvent {
            target: ClientInputTarget::Pointer {
                surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 1), 1),
                layer: SurfaceLayerId::new(0),
            },
            event: InputEventKind::PointerMotion {
                relative: None,
                position: InputPosition::new(f64::from(time), 0.0),
            },
            time,
        }),
    }
}

#[test]
fn coalesced_relative_motion_sums_deltas_and_preserves_absolute_barriers() {
    block_on(async {
        use weld_client::{InputDelta, RelativeMotion};
        let queue = InputOutbox::default();
        for time in 1..=3 {
            let mut packet = motion(time);
            if let DestinationMessage::Input(input) = &mut packet.message
                && let InputEventKind::PointerMotion { relative, .. } = &mut input.event
            {
                *relative = Some(RelativeMotion {
                    delta: InputDelta::new(2.0, -1.0),
                    unaccelerated: InputDelta::new(1.0, -0.5),
                    time_micros: u64::from(time) * 1000,
                });
            }
            queue.push(packet).expect("motion");
        }
        queue.push(motion(4)).expect("absolute barrier");
        let DestinationMessage::Input(input) = queue.recv().await.expect("relative").message else {
            panic!("input");
        };
        assert_eq!(input.time, 3);
        assert!(
            matches!(input.event, InputEventKind::PointerMotion { position, relative: Some(relative) }
        if position.x == 3.0 && relative.delta == InputDelta::new(6.0, -3.0)
        && relative.unaccelerated == InputDelta::new(3.0, -1.5) && relative.time_micros == 3000)
        );
        let DestinationMessage::Input(input) = queue.recv().await.expect("absolute").message else {
            panic!("input");
        };
        assert_eq!(input.time, 4);
        assert!(
            matches!(input.event, InputEventKind::PointerMotion { relative: None, position } if position.x == 4.0)
        );
    });
}

#[test]
fn observations_distinguish_coalescing_dequeue_and_completed_writes() {
    block_on(async {
        let queue = InputOutbox::default();
        queue.push(motion(1)).expect("first");
        let old_enqueued = Instant::now() - Duration::from_secs(1);
        queue
            .state
            .lock()
            .expect("state")
            .records
            .front_mut()
            .expect("first")
            .enqueued_at = old_enqueued;
        queue.push(motion(2)).expect("replacement");
        {
            let state = queue.state.lock().expect("state");
            assert!(state.records.front().expect("replacement").enqueued_at > old_enqueued);
        }
        queue
            .push(DestinationEnvelope {
                session: HoistSessionId::new(1),
                message: DestinationMessage::Reclaim,
            })
            .expect("barrier");
        let packet = queue.recv().await.expect("motion");
        {
            let state = queue.state.lock().expect("state");
            assert_eq!(state.received, [2, 0, 0, 0, 1, 0]);
            assert_eq!(state.coalesced, 1);
            assert_eq!(state.observations.motions_received, 2);
            assert_eq!(state.observations.dequeued, 1);
            assert_eq!(state.observations.writes_completed, 0);
            assert_eq!(state.observations.queue_high_water, 2);
        }
        queue.record_written(&packet, 60, Duration::from_millis(2));
        queue
            .state
            .lock()
            .expect("state")
            .records
            .front_mut()
            .expect("reclaim")
            .enqueued_at = Instant::now() - Duration::from_secs(2);
        let packet = queue.recv().await.expect("reclaim");
        queue.record_written(&packet, 8, Duration::from_millis(1));
        let state = queue.state.lock().expect("state");
        assert_eq!(state.observations.dequeued, 2);
        assert_eq!(state.observations.writes_completed, 2);
        assert_eq!(state.observations.motions_written, 1);
        assert_eq!(state.observations.framed_bytes, 68);
        assert!(state.observations.retained_wait_max >= Duration::from_secs(2));
        assert_eq!(state.observations.write_wall_max, Duration::from_millis(2));
    });
}

#[test]
fn motion_burst_preserves_latest_position_time_and_release() {
    block_on(async {
        let queue = InputOutbox::default();
        for time in 0..4096 {
            queue.push(motion(time)).expect("motion");
        }
        let mut release = motion(4096);
        if let DestinationMessage::Input(input) = &mut release.message {
            input.event = InputEventKind::PointerButton {
                position: None,
                button: LinuxButtonCode(272),
                state: ButtonState::Released,
            };
        }
        queue.push(release).expect("release");
        let DestinationMessage::Input(input) = queue.recv().await.expect("latest").message else {
            panic!("input")
        };
        assert_eq!(input.time, 4095);
        assert_eq!(
            input.event,
            InputEventKind::PointerMotion {
                relative: None,
                position: InputPosition::new(4095.0, 0.0)
            }
        );
        assert!(matches!(
            queue.recv().await.expect("release").message,
            DestinationMessage::Input(WireClientInputEvent {
                event: InputEventKind::PointerButton {
                    state: ButtonState::Released,
                    ..
                },
                ..
            })
        ));
    });
}

#[test]
fn leave_keys_and_control_are_coalescing_barriers() {
    block_on(async {
        for event in [
            InputEventKind::PointerLeft {
                position: InputPosition::default(),
            },
            InputEventKind::Keyboard {
                keycode: LinuxKeycode(30),
                state: weld_client::KeyboardKeyState::Pressed,
            },
            InputEventKind::Keyboard {
                keycode: LinuxKeycode(30),
                state: weld_client::KeyboardKeyState::Repeated,
            },
            InputEventKind::Keyboard {
                keycode: LinuxKeycode(30),
                state: weld_client::KeyboardKeyState::Released,
            },
            InputEventKind::PointerAxis {
                position: None,
                axis: RawScrollFrame {
                    source: RawScrollSource::Wheel,
                    phase: RawScrollPhase::Moved,
                    horizontal: 0.0,
                    vertical: 1.0,
                    horizontal_v120: None,
                    vertical_v120: Some(120),
                    horizontal_stop: false,
                    vertical_stop: false,
                },
            },
            InputEventKind::PointerButton {
                position: None,
                button: LinuxButtonCode(272),
                state: ButtonState::Pressed,
            },
        ] {
            let queue = InputOutbox::default();
            let mut barrier = motion(2);
            if let DestinationMessage::Input(input) = &mut barrier.message {
                input.event = event;
            }
            queue.push(motion(1)).expect("before");
            queue.push(barrier).expect("barrier");
            queue.push(motion(3)).expect("after");
            assert_eq!(queue.state.lock().expect("state").records.len(), 3);
        }
        let mut other_session = motion(2);
        other_session.session = HoistSessionId::new(2);
        let mut other_target = motion(2);
        if let DestinationMessage::Input(input) = &mut other_target.message {
            input.target = ClientInputTarget::Pointer {
                surface: input.target.surface(),
                layer: SurfaceLayerId::new(1),
            };
        }
        for barrier in [
            other_session,
            other_target,
            DestinationEnvelope {
                session: HoistSessionId::new(1),
                message: DestinationMessage::Reclaim,
            },
            DestinationEnvelope {
                session: HoistSessionId::new(1),
                message: DestinationMessage::CursorReceived {
                    surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 1), 1),
                    sequence: 1,
                },
            },
        ] {
            let queue = InputOutbox::default();
            queue.push(motion(1)).expect("before");
            queue.push(barrier).expect("barrier");
            queue.push(motion(3)).expect("after");
            assert_eq!(queue.state.lock().expect("state").records.len(), 3);
        }
    });
}

#[test]
fn touch_motion_coalesces_only_within_a_contact_and_frame() {
    block_on(async {
        use weld_client::{TouchEvent, TouchId};
        let queue = InputOutbox::default();
        let packet = |id, x| {
            let mut packet = motion(x);
            if let DestinationMessage::Input(input) = &mut packet.message {
                let surface = input.target.surface();
                input.target = ClientInputTarget::Touch {
                    surface,
                    layer: SurfaceLayerId::new(1),
                };
                input.event = InputEventKind::Touch {
                    event: TouchEvent::Motion {
                        id: TouchId(id),
                        position: InputPosition::new(f64::from(x), 0.0),
                    },
                };
            }
            packet
        };
        queue.push(packet(1, 1)).expect("first motion");
        queue.push(packet(1, 2)).expect("same contact");
        queue.push(packet(2, 3)).expect("different contact");
        let mut frame = packet(2, 4);
        if let DestinationMessage::Input(input) = &mut frame.message {
            input.event = InputEventKind::Touch {
                event: TouchEvent::Frame,
            };
        }
        queue.push(frame).expect("frame");
        queue.push(packet(2, 5)).expect("next frame motion");
        assert_eq!(queue.state.lock().expect("state").records.len(), 4);
        let newest = queue.recv().await.expect("motion");
        assert!(
            matches!(newest.message, DestinationMessage::Input(input) if matches!(input.event,
        InputEventKind::Touch { event: TouchEvent::Motion { id: TouchId(1), position } } if position.x == 2.0))
        );
    });
}

#[test]
fn close_wakes_receiver_and_rejects_further_records() {
    block_on(async {
        let queue = InputOutbox::default();
        let mut waiting = Box::pin(queue.recv());
        assert!(poll_once(waiting.as_mut()).await.is_none());
        queue.close();
        assert!(
            poll_once(waiting.as_mut())
                .await
                .expect("closed wake")
                .is_err()
        );
        assert!(queue.push(motion(1)).is_err());
        assert!(queue.recv().await.is_err());
    });
}

#[test]
fn full_tail_can_coalesce_but_discrete_overload_remains_bounded() {
    block_on(async {
        let queue = InputOutbox::default();
        for _ in 0..QUEUE_CAPACITY - 1 {
            queue
                .push(DestinationEnvelope {
                    session: HoistSessionId::new(1),
                    message: DestinationMessage::Reclaim,
                })
                .expect("control");
        }
        queue.push(motion(1)).expect("last slot");
        queue.push(motion(2)).expect("replace full tail");
        let error = queue
            .push(DestinationEnvelope {
                session: HoistSessionId::new(1),
                message: DestinationMessage::Reclaim,
            })
            .expect_err("bound");
        assert!(error.to_string().contains("coalesced_motions=1"));
        assert_eq!(
            queue.state.lock().expect("state").records.len(),
            QUEUE_CAPACITY
        );
    });
}

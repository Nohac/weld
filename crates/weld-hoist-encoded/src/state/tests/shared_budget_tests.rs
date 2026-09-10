//! Shared policy exercised through the real source state and strict fake codec.

use super::*;
use crate::{EncoderBitrateLimits, InsufficientBitrateBudget};
use weld_client::{ClientFocusRequest, ClientSurfaceRole, LogicalPoint, PopupState};

fn budgeted(
    budget: &SharedBitrateBudget,
) -> (
    EncodedSourceState,
    Rc<RefCell<FakeEncoderState>>,
    EncoderRateControl,
) {
    let fake = Rc::new(RefCell::new(FakeEncoderState {
        bitrate_limits: Some(
            EncoderBitrateLimits::try_new(128_000, 8_000_000, 8_000_000).expect("limits"),
        ),
        generation_limit: Some(16),
        ..Default::default()
    }));
    let mut state = EncodedSourceState::new(Box::new(FakeEncoder(fake.clone())));
    let control = state.rates.as_ref().expect("rates").control();
    state.budget = Some(budget.attach(control.clone()).expect("membership"));
    (state, fake, control)
}

fn id(local: u64) -> ClientSurfaceId {
    surface(ClientSourceId::new(1), 1, local)
}

fn send(
    state: &mut EncodedSourceState,
    local: u64,
    revision: u64,
    layers: &[(u64, u32)],
) -> Result<()> {
    let buffers = layers
        .iter()
        .map(|(layer, width)| {
            let metadata = ClientBufferMetadata::new(Extent::new(*width, 1), true);
            SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(*layer),
                change: SurfaceBufferChange::Replaced {
                    metadata,
                    buffer: shm_lease(ClientSourceId::new(1), revision * 100 + layer, 1, metadata),
                },
            }
        })
        .collect();
    state.enqueue(HoistSessionId::new(1), commit(id(local), revision, buffers))
}

fn finish_batch(state: &mut EncodedSourceState, fake: &Rc<RefCell<FakeEncoderState>>) {
    while let Some(batch) = &state.in_flight {
        complete(fake, batch.active.token, batch.active.frame, 1);
        state.drain().expect("completion");
    }
    state.take_output();
}

#[test]
fn complete_first_inventory_is_budgeted_before_freezing_and_active_batch_stays_frozen() {
    let budget = SharedBitrateBudget::new(8_000_000).expect("budget");
    let (mut state, fake, control) = budgeted(&budget);
    send(&mut state, 1, 1, &[(1, 10), (2, 10)]).expect("two layers");
    let first_rates = control
        .streams()
        .expect("registered")
        .iter()
        .map(|state| state.requested.bits_per_second)
        .collect::<Vec<_>>();
    assert_eq!(first_rates, [3_776_000, 3_776_000]);
    assert_eq!(fake.borrow().submitted[0].1.generation.raw(), 1);
    budget.set_target(4_000_000).expect("change during batch");
    finish_batch(&mut state, &fake);
    assert_eq!(
        fake.borrow().submitted_bitrates,
        [Some(first_rates[0]), Some(first_rates[1])]
    );
    send(&mut state, 1, 2, &[(1, 10), (2, 10)]).expect("new batch");
    finish_batch(&mut state, &fake);
    assert_eq!(
        fake.borrow()
            .submitted
            .iter()
            .map(|(_, frame, _)| frame.generation.raw())
            .collect::<Vec<_>>(),
        [1, 1, 2, 2]
    );
    assert!(
        fake.borrow().submitted_bitrates[2..]
            .iter()
            .flatten()
            .sum::<u64>()
            <= 4_000_000
    );
    assert_eq!(fake.borrow().generations.len(), 2);
}

#[test]
fn add_remove_and_cancel_release_shares_without_synthetic_encode_work() {
    let budget = SharedBitrateBudget::new(8_000_000).expect("budget");
    let (mut state, fake, control) = budgeted(&budget);
    send(&mut state, 1, 1, &[(1, 100)]).expect("first");
    finish_batch(&mut state, &fake);
    send(&mut state, 2, 1, &[(1, 100)]).expect("other window");
    finish_batch(&mut state, &fake);
    assert_eq!(
        fake.borrow().submitted.len(),
        2,
        "allocation never generates pixels"
    );
    send(&mut state, 1, 2, &[(1, 100)]).expect("first uses reduced share");
    finish_batch(&mut state, &fake);
    assert_eq!(fake.borrow().submitted[2].1.generation.raw(), 2);
    state.cancel_surface(id(2)).expect("cancel");
    assert_eq!(budget.snapshot().expect("released").streams, 1);
    assert_eq!(
        control.streams().expect("restored intent")[0]
            .requested
            .bits_per_second,
        7_552_000
    );
    send(&mut state, 1, 3, &[]).expect("remove layer");
    assert_eq!(budget.snapshot().expect("removed").streams, 0);
}

#[test]
fn impossible_new_inventory_does_not_consume_ids_or_register_streams() {
    let budget = SharedBitrateBudget::new(200_000).expect("budget");
    let (mut state, fake, control) = budgeted(&budget);
    assert!(
        send(&mut state, 1, 1, &[(1, 10), (2, 10)])
            .expect_err("minima")
            .is::<InsufficientBitrateBudget>()
    );
    assert_eq!(state.next_stream, Some(1));
    assert!(state.streams.is_empty());
    assert!(control.streams().expect("no mutation").is_empty());
    assert!(fake.borrow().submitted.is_empty());
    drop(state);
    assert_eq!(budget.snapshot().expect("no stale reservation").streams, 0);
}

#[test]
fn focused_activity_and_repeated_inventory_do_not_rotate_encoders() {
    let budget = SharedBitrateBudget::new(8_000_000).expect("budget");
    let (mut state, fake, control) = budgeted(&budget);
    send(&mut state, 1, 1, &[(1, 100)]).expect("first");
    finish_batch(&mut state, &fake);
    let original = control.streams().expect("original")[0].requested;
    for revision in 2..=8 {
        state.activity.observe(
            HoistSessionId::new(1),
            &DestinationMessage::Request(ClientRequest::Focus(ClientFocusRequest {
                source: ClientSourceId::new(1),
                surface: Some(id(1)),
            })),
            Instant::now(),
            state.policy,
        );
        send(&mut state, 1, revision, &[(1, 100)]).expect("same inventory");
        finish_batch(&mut state, &fake);
    }
    assert_eq!(control.streams().expect("unchanged")[0].requested, original);
    assert!(
        fake.borrow()
            .submitted
            .iter()
            .all(|(_, frame, _)| frame.generation.raw() == 1)
    );
    assert!(fake.borrow().retirements.is_empty());
}

#[test]
fn tiny_popup_open_close_and_owner_changes_use_the_existing_group_relationship() {
    let budget = SharedBitrateBudget::new(8_000_000).expect("budget");
    let (mut state, fake, control) = budgeted(&budget);
    send(&mut state, 1, 1, &[(1, 8_294_400)]).expect("parent");
    finish_batch(&mut state, &fake);
    let parent = control.streams().expect("parent")[0];
    state
        .enqueue(
            HoistSessionId::new(1),
            ClientSurfaceEvent {
                surface: id(2),
                kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(PopupState {
                    owner: id(1),
                    position: LogicalPoint::new(0.0, 0.0),
                    stack_index: 0,
                })),
            },
        )
        .expect("popup role");
    state.take_output();
    send(&mut state, 2, 1, &[(1, 576)]).expect("tiny popup");
    finish_batch(&mut state, &fake);
    assert_eq!(
        control.streams().expect("parent still stable")[0].requested,
        parent.requested
    );
    assert_eq!(state.activity.group(id(1)), state.activity.group(id(2)));
    state.cancel_surface(id(2)).expect("close popup");
    send(&mut state, 1, 2, &[(1, 8_294_400)]).expect("parent unchanged");
    finish_batch(&mut state, &fake);
    assert_eq!(fake.borrow().submitted[2].1.generation.raw(), 1);

    send(&mut state, 3, 1, &[(1, 100)]).expect("independent dialog");
    finish_batch(&mut state, &fake);
    let before = control.streams().expect("independent");
    state
        .enqueue(
            HoistSessionId::new(1),
            ClientSurfaceEvent {
                surface: id(3),
                kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(PopupState {
                    owner: id(1),
                    position: LogicalPoint::new(0.0, 0.0),
                    stack_index: 0,
                })),
            },
        )
        .expect("role changes allocation");
    // No new encode was fabricated. The membership itself changes immediately;
    // target churn remains subject to the asymmetric preservation policy.
    assert_eq!(state.activity.group(id(1)), state.activity.group(id(3)));
    budget
        .set_target(4_000_000)
        .expect("exercise regrouped ideals");
    let after = control.streams().expect("regrouped targets");
    assert_eq!(after.len(), before.len());
    assert_eq!(after[0].requested.bits_per_second, 3_776_000);
    assert_eq!(after[1].requested.bits_per_second, 128_000);
    assert_eq!(fake.borrow().submitted.len(), 4);
    assert_eq!(before.len(), 2);
}

#[test]
fn two_windows_then_tiny_popup_preserve_headroom_without_another_window_switch() {
    let budget = SharedBitrateBudget::new(8_000_000).expect("budget");
    let (mut state, fake, control) = budgeted(&budget);
    send(&mut state, 1, 1, &[(1, 8_294_400)]).expect("A");
    finish_batch(&mut state, &fake);
    let original = control.streams().expect("A")[0].requested;
    send(&mut state, 2, 1, &[(1, 8_294_400)]).expect("B");
    finish_batch(&mut state, &fake);
    let before_popup = control.streams().expect("A and B");
    assert_eq!(before_popup[0].requested.revision, original.revision + 1);
    assert_eq!(before_popup[0].requested.bits_per_second, 3_776_000);
    send(&mut state, 1, 2, &[(1, 8_294_400)]).expect("A applies reduction");
    finish_batch(&mut state, &fake);
    assert_eq!(fake.borrow().submitted[2].1.generation.raw(), 2);
    state
        .enqueue(
            HoistSessionId::new(1),
            ClientSurfaceEvent {
                surface: id(3),
                kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(PopupState {
                    owner: id(2),
                    position: LogicalPoint::new(0.0, 0.0),
                    stack_index: 0,
                })),
            },
        )
        .expect("popup role");
    state.take_output();
    send(&mut state, 3, 1, &[(1, 576)]).expect("popup on B");
    finish_batch(&mut state, &fake);
    let after_popup = control.streams().expect("popup targets");
    assert_eq!(after_popup[0].requested, before_popup[0].requested);
    assert_eq!(after_popup[1].requested, before_popup[1].requested);
    assert_eq!(after_popup[2].requested.bits_per_second, 128_000);
    send(&mut state, 1, 3, &[(1, 8_294_400)]).expect("A unchanged");
    finish_batch(&mut state, &fake);
    send(&mut state, 2, 2, &[(1, 8_294_400)]).expect("B unchanged");
    finish_batch(&mut state, &fake);
    assert_eq!(fake.borrow().submitted[4].1.generation.raw(), 2);
    assert_eq!(fake.borrow().submitted[5].1.generation.raw(), 1);
    assert_eq!(fake.borrow().retirements.len(), 1);
}

#[test]
fn managed_source_does_not_silently_keep_a_frozen_rate_when_its_actuator_is_lost() {
    let budget = SharedBitrateBudget::new(8_000_000).expect("budget");
    let (mut state, fake, _) = budgeted(&budget);
    send(&mut state, 1, 1, &[(1, 100)]).expect("first");
    finish_batch(&mut state, &fake);
    state.rates.take();
    assert!(send(&mut state, 1, 2, &[(1, 100)]).is_err());
    assert_eq!(fake.borrow().submitted.len(), 1);
}

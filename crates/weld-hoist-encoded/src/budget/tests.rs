use super::*;
use crate::bitrate::EncoderRates;
use weld_client::{ClientId, ClientSourceId, ClientSurfaceId, SurfaceLayerId};
use weld_hoist_core::HoistSessionId;

fn group(root: u64) -> Group {
    Group {
        session: HoistSessionId::new(1),
        root: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), root),
    }
}

fn input(port: u64, stream: u64, root: u64, pixels: u64) -> AllocationInput {
    AllocationInput {
        key: (port, MediaStreamId::new(stream)),
        group: (port, group(root)),
        pixels,
        limits: EncoderBitrateLimits::try_new(128_000, 8_000_000, 8_000_000).expect("limits"),
        current: None,
    }
}

#[test]
fn windows_share_equally_and_popup_layers_split_their_owners_share() {
    let inputs = [input(1, 1, 1, 100), input(1, 2, 1, 50), input(1, 3, 2, 100)];
    let rates = allocate(8_000_000, &inputs).expect("allocation");
    assert!(rates[0] > rates[1]);
    assert!(rates[0] + rates[1] <= rates[2] + 192_000);
    assert!(rates.iter().sum::<u64>() <= 8_000_000);
    let one = allocate(8_000_000, &[input(1, 1, 1, 100)]).expect("single");
    assert_eq!(one, [7_552_000]);
}

#[test]
fn largest_layer_caps_and_remainder_goes_to_other_layers() {
    let mut inputs = [input(1, 1, 1, 10_000), input(1, 2, 1, 1)];
    inputs[0].limits = EncoderBitrateLimits::try_new(128_000, 1_000_000, 1_000_000).expect("cap");
    let rates = allocate(8_000_000, &inputs).expect("capped");
    assert_eq!(rates[0], 960_000);
    assert!(rates[1] > 6_000_000);
}

#[test]
fn tiny_popup_uses_headroom_without_parent_churn_and_small_restoration_waits() {
    let mut parent = input(1, 1, 1, 8_294_400);
    parent.current = Some(7_552_000);
    let rates = allocate(8_000_000, &[parent, input(1, 2, 1, 576)]).expect("popup");
    assert_eq!(rates, [7_552_000, 128_000]);
    let mut parent = input(1, 1, 1, 8_294_400);
    parent.current = Some(rates[0]);
    assert_eq!(allocate(8_000_000, &[parent]).expect("closed"), [7_552_000]);
}

#[test]
fn forced_decreases_fit_and_low_custom_targets_do_not_round_to_one() {
    let mut inputs = [input(1, 1, 1, 1), input(2, 1, 1, 1)];
    inputs[0].current = Some(7_552_000);
    let rates = allocate(8_000_000, &inputs).expect("second port");
    assert!(rates.iter().sum::<u64>() <= 8_000_000);
    for input in &mut inputs {
        input.current = None;
        input.limits = EncoderBitrateLimits::try_new(1, 100_000, 100_000).expect("custom");
    }
    assert_eq!(allocate(1_000, &inputs).expect("small"), [475, 475]);
    assert!(
        allocate(1, &inputs)
            .expect_err("floor")
            .is::<InsufficientBitrateBudget>()
    );
}

#[test]
fn wide_arithmetic_and_asymmetric_preservation_stay_within_limits() {
    for count in 1..=16 {
        for target in [2_048_000, 8_000_000, 16_000_000, u64::MAX] {
            let inputs = (0..count)
                .map(|index| input(1, index, index % 3, u64::MAX - index))
                .collect::<Vec<_>>();
            let rates = allocate(target, &inputs).expect("fits");
            assert!(rates.iter().map(|rate| u128::from(*rate)).sum::<u128>() <= u128::from(target));
            for (rate, input) in rates.iter().zip(&inputs) {
                input.limits.validate(*rate).expect("range");
            }
        }
    }
}

fn member(budget: &SharedBitrateBudget) -> (EncoderRates, BudgetMembership, EncoderRateControl) {
    let rates = EncoderRates::new(
        EncoderBitrateLimits::try_new(128_000, 8_000_000, 8_000_000).expect("limits"),
    );
    let control = rates.control();
    let member = budget.attach(control.clone()).expect("attach");
    rates
        .register(MediaStreamId::new(1), group(1).root, SurfaceLayerId::new(1))
        .expect("stream");
    (rates, member, control)
}

fn demands() -> Vec<StreamDemand> {
    vec![StreamDemand {
        stream: MediaStreamId::new(1),
        group: group(1),
        pixels: 100,
    }]
}

#[test]
fn shared_membership_scopes_ids_excludes_manual_requests_and_releases_on_drop() {
    let budget = SharedBitrateBudget::new(8_000_000).expect("budget");
    let (_rates1, member1, control1) = member(&budget);
    let (_rates2, member2, control2) = member(&budget);
    member1.update(demands()).expect("first");
    member2.update(demands()).expect("second");
    assert_eq!(budget.snapshot().expect("snapshot").streams, 2);
    assert!(control1.request(MediaStreamId::new(1), 1_000_000).is_err());
    assert!(
        control2.streams().expect("second")[0]
            .requested
            .bits_per_second
            < 4_000_000
    );
    drop(member1);
    assert_eq!(
        control2.streams().expect("restored")[0]
            .requested
            .bits_per_second,
        7_552_000
    );
    assert_eq!(budget.snapshot().expect("released").streams, 1);
}

#[test]
fn borrowed_drop_is_repaired_by_plain_preflight_without_inventory_change() {
    let budget = SharedBitrateBudget::new(8_000_000).expect("budget");
    let (_rates1, member1, _) = member(&budget);
    let (_rates2, member2, control2) = member(&budget);
    member1.update(demands()).expect("first");
    member2.update(demands()).expect("second");
    let held = budget.0.state.borrow();
    drop(member1);
    drop(held);
    member2.preflight(1).expect("ordinary admission");
    assert_eq!(
        control2.streams().expect("restored")[0]
            .requested
            .bits_per_second,
        7_552_000
    );
}

#[test]
fn impossible_target_preserves_previous_state_and_failed_member_releases_share() {
    let budget = SharedBitrateBudget::new(300_000).expect("budget");
    let (_rates1, member1, control1) = member(&budget);
    let (_rates2, member2, _) = member(&budget);
    member1.update(demands()).expect("first");
    member2.update(demands()).expect("second");
    assert!(
        member2
            .preflight(2)
            .expect_err("too many")
            .is::<InsufficientBitrateBudget>()
    );
    let before = budget.snapshot().expect("before");
    assert!(budget.set_target(200_000).is_err());
    assert_eq!(budget.snapshot().expect("unchanged"), before);
    drop(member2);
    assert_eq!(
        control1.streams().expect("restored")[0]
            .requested
            .bits_per_second,
        256_000
    );
}

#[test]
fn unavailable_actuator_isolated_from_healthy_members_and_cannot_rejoin() {
    let budget = SharedBitrateBudget::new(8_000_000).expect("budget");
    let (rates1, member1, _) = member(&budget);
    let (_rates2, member2, control2) = member(&budget);
    member1.update(demands()).expect("first");
    member2.update(demands()).expect("second");
    drop(rates1);
    budget
        .set_target(4_000_000)
        .expect("healthy port still allocates");
    assert_eq!(
        control2.streams().expect("healthy")[0]
            .requested
            .bits_per_second,
        3_776_000
    );
    assert_eq!(budget.snapshot().expect("pruned").streams, 1);
    assert!(member1.preflight(1).is_err());
    assert!(member1.update(demands()).is_err());
    member2
        .update(demands())
        .expect("unchanged healthy admission");
}

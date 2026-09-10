use super::*;
use crate::EncoderRateApplication;
use crate::activity::tests::{focus, input as observe_input, mapped, surface};
use crate::activity::{ActivitySnapshot, SchedulingPolicy};
use crate::bitrate::EncoderRates;
use std::time::Duration;
use weld_client::{
    ClientId, ClientSourceId, ClientSurfaceId, InputEventKind, InputPosition, SurfaceLayerId,
};
use weld_hoist_core::HoistSessionId;
use weld_media::{MediaFrameId, StreamGeneration};

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
        weight: 1,
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
fn interactive_group_weight_is_not_multiplied_by_its_tiny_popups() {
    let mut inputs = vec![input(1, 1, 1, 8_294_400), input(1, 2, 2, 100)];
    inputs[0].weight = 12;
    let before = allocate(8_000_000, &inputs).expect("two groups");
    for stream in 3..=5 {
        let mut popup = input(1, stream, 1, 576);
        popup.weight = 12;
        inputs.push(popup);
    }
    let after = allocate(8_000_000, &inputs).expect("owner plus popups");
    assert!(after[0] > 6_000_000);
    assert_eq!(&after[2..], &[128_000; 3]);
    assert!(
        after[1].abs_diff(before[1]) <= 64_000,
        "one group entitlement, not four"
    );
    assert!(after.iter().sum::<u64>() <= 8_000_000);
}

#[test]
fn quality_motion_survives_pauses_without_extending_queue_priority_or_encoder_churn() {
    let now = Instant::now();
    let mut fixture = PriorityFixture::new(now);
    let mut activity = mapped();
    let scheduling = SchedulingPolicy::default();
    let mut snapshot = ActivitySnapshot::default();
    focus(&mut activity, 1, now);
    for ms in [0, 10, 90, 120] {
        observe_input(
            &mut activity,
            1,
            InputEventKind::PointerMotion {
                position: InputPosition::new(ms as f64, 0.0),
            },
            now + Duration::from_millis(ms),
        );
    }
    activity.snapshot(scheduling, &mut snapshot);
    fixture.attention = snapshot.attention.clone();
    fixture.publish(now + Duration::from_millis(120));
    let boosted = fixture.requests();
    assert!(boosted[0].bits_per_second > 6_000_000);
    let resumed = now + Duration::from_secs(1);
    observe_input(
        &mut activity,
        1,
        InputEventKind::PointerMotion {
            position: InputPosition::new(200.0, 0.0),
        },
        resumed,
    );
    activity.snapshot(scheduling, &mut snapshot);
    assert_eq!(
        snapshot.attention[&group(1)].priority(
            resumed,
            scheduling.interaction_grace,
            scheduling.motion_grace
        ),
        Priority::Moving
    );
    fixture.attention = snapshot.attention.clone();
    fixture.publish(resumed);
    assert_eq!(
        fixture.requests(),
        boosted,
        "resumed initial motion keeps quality hold"
    );
    observe_input(
        &mut activity,
        1,
        InputEventKind::PointerMotion {
            position: InputPosition::new(300.0, 0.0),
        },
        resumed + Duration::from_millis(90),
    );
    activity.snapshot(scheduling, &mut snapshot);
    fixture.attention = snapshot.attention.clone();
    fixture.publish(resumed + Duration::from_millis(90));
    assert_eq!(
        fixture.requests(),
        boosted,
        "continued motion only renews timestamp"
    );
    fixture.publish(now + Duration::from_secs(12));
    assert!(fixture.requests()[0].bits_per_second < boosted[0].bits_per_second);
    activity.mapped(surface(1), false);
    activity.mapped(surface(1), true);
    activity.snapshot(scheduling, &mut snapshot);
    assert!(snapshot.attention[&group(1)].quality_motion.is_none());
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

// Existing structural-policy fixtures publish no attention. Production publishes
// its trusted snapshot between inventory reconciliation and the single refresh.
impl BudgetMembership {
    fn update(&self, demands: Vec<StreamDemand>) -> Result<()> {
        self.update_inventory(demands)?;
        self.update_attention(&HashMap::new(), Instant::now())
    }
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
    member2
        .preflight(1, Instant::now())
        .expect("ordinary admission");
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
            .preflight(2, Instant::now())
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
    assert!(member1.preflight(1, Instant::now()).is_err());
    assert!(member1.update(demands()).is_err());
    member2
        .update(demands())
        .expect("unchanged healthy admission");
}

struct PriorityFixture {
    budget: SharedBitrateBudget,
    rates: EncoderRates,
    member: BudgetMembership,
    control: EncoderRateControl,
    attention: HashMap<Group, GroupAttention>,
    submitted: BTreeMap<MediaStreamId, (u64, u64, u64)>,
}

impl PriorityFixture {
    fn new(now: Instant) -> Self {
        let budget = SharedBitrateBudget::new(8_000_000).expect("budget");
        let (rates, member, control) = member(&budget);
        for id in [2, 3] {
            rates
                .register(
                    MediaStreamId::new(id),
                    group(id).root,
                    SurfaceLayerId::new(1),
                )
                .expect("stream");
        }
        member
            .update_inventory(
                (1..=3)
                    .map(|id| StreamDemand {
                        stream: MediaStreamId::new(id),
                        group: group(id),
                        pixels: 100,
                    })
                    .collect(),
            )
            .expect("inventory");
        let attention = (1..=3)
            .map(|id| (group(id), GroupAttention::default()))
            .collect();
        member.update_attention(&attention, now).expect("initial");
        Self {
            budget,
            rates,
            member,
            control,
            attention,
            submitted: BTreeMap::new(),
        }
    }

    fn focus(&mut self, id: u64) {
        for (candidate, value) in &mut self.attention {
            value.focused = *candidate == group(id);
        }
    }

    fn publish(&self, now: Instant) {
        self.member
            .update_attention(&self.attention, now)
            .expect("attention");
    }

    fn requests(&self) -> Vec<crate::BitrateRequest> {
        self.control
            .streams()
            .expect("streams")
            .into_iter()
            .map(|status| status.requested)
            .collect()
    }

    // Count actual selected-rate changes through the real dwell actuator, with
    // legal per-stream frame identities, without sleeping or calling a GPU API.
    fn submit(&mut self, now: Instant) -> Vec<u64> {
        for status in self.control.streams().expect("streams") {
            let request = self.rates.select(status.stream, now).expect("selection");
            let value =
                self.submitted
                    .entry(status.stream)
                    .or_insert((request.bits_per_second, 1, 0));
            if value.0 != request.bits_per_second {
                value.0 = request.bits_per_second;
                value.1 += 1;
                value.2 = 0;
            }
            let frame = MediaFrameId::new(status.stream, StreamGeneration::new(value.1), value.2);
            value.2 += 1;
            let work = EncoderRateApplication { request, frame };
            self.rates.submitted(work, now);
            self.rates.finished(work, true);
        }
        self.submitted.values().map(|value| value.1).collect()
    }
}

#[test]
fn interaction_transfers_budget_even_when_old_sum_fits_and_pauses_do_not_churn() {
    let now = Instant::now();
    let mut fixture = PriorityFixture::new(now);
    assert_eq!(fixture.submit(now), [1, 1, 1]);
    let initial = fixture.requests();
    assert!(
        initial
            .iter()
            .map(|value| value.bits_per_second)
            .sum::<u64>()
            < 8_000_000
    );
    fixture.focus(2);
    fixture
        .attention
        .get_mut(&group(2))
        .expect("foot")
        .interaction = Some(now);
    fixture.publish(now);
    let interactive = fixture.requests();
    assert!(interactive[1].bits_per_second > 6_000_000);
    assert!(interactive[0].bits_per_second < initial[0].bits_per_second);
    assert!(interactive[2].bits_per_second < initial[2].bits_per_second);
    assert_eq!(fixture.submit(now), [2, 2, 2]);
    let pause = now + Duration::from_secs(4);
    fixture.budget.refresh_at(pause).expect("pause");
    assert_eq!(fixture.requests(), interactive);
    fixture
        .attention
        .get_mut(&group(2))
        .expect("foot")
        .interaction = Some(pause);
    fixture.publish(pause);
    assert_eq!(
        fixture.requests(),
        interactive,
        "new timestamps do not allocate new revisions"
    );
    assert_eq!(fixture.submit(pause), [2, 2, 2]);
    fixture
        .budget
        .refresh_at(pause + Duration::from_secs(11))
        .expect("expiry");
    assert!(fixture.requests()[1].bits_per_second < interactive[1].bits_per_second);
    assert_eq!(fixture.submit(pause + Duration::from_secs(11)), [3, 3, 3]);
}

#[test]
fn focus_handoff_is_atomic_and_toggle_back_has_no_rate_or_generation_changes() {
    let now = Instant::now();
    let mut fixture = PriorityFixture::new(now);
    fixture.focus(1);
    fixture.publish(now);
    fixture
        .budget
        .refresh_at(now + Duration::from_millis(500))
        .expect("settle A");
    let original = fixture.requests();
    assert!(
        original[0].bits_per_second < 4_000_000,
        "focus alone is modest"
    );
    assert_eq!(fixture.submit(now + Duration::from_millis(500)), [1, 1, 1]);
    fixture.focus(2);
    fixture.publish(now + Duration::from_secs(1));
    fixture.focus(3);
    fixture.publish(now + Duration::from_millis(1100));
    assert_eq!(fixture.requests(), original, "no intermediate allocation");
    fixture
        .budget
        .refresh_at(now + Duration::from_millis(1600))
        .expect("settle C without polling source");
    let handed = fixture.requests();
    assert_eq!(handed[0].revision, original[0].revision + 1);
    assert_eq!(handed[1], original[1]);
    assert_eq!(handed[2].revision, original[2].revision + 1);
    assert_eq!(fixture.submit(now + Duration::from_millis(1600)), [2, 1, 2]);
    for step in 0..10 {
        fixture.focus(if step % 2 == 0 { 1 } else { 3 });
        fixture.publish(now + Duration::from_millis(1700 + step * 100));
    }
    fixture
        .budget
        .refresh_at(now + Duration::from_secs(4))
        .expect("no outstanding candidate");
    assert_eq!(fixture.requests(), handed);
    assert_eq!(fixture.submit(now + Duration::from_secs(4)), [2, 1, 2]);
    fixture.focus(0);
    fixture.publish(now + Duration::from_secs(5));
    assert!(
        fixture.requests()[2].bits_per_second < handed[2].bits_per_second,
        "explicit clear is immediate"
    );
}

#[test]
fn pending_stream_focus_does_not_clear_bonus_and_unmapping_settled_group_does() {
    let now = Instant::now();
    let mut fixture = PriorityFixture::new(now);
    fixture.focus(1);
    fixture.publish(now);
    fixture
        .budget
        .refresh_at(now + Duration::from_secs(1))
        .expect("settled");
    let before = fixture.requests();
    fixture.focus(0);
    fixture.attention.insert(
        group(4),
        GroupAttention {
            focused: true,
            ..Default::default()
        },
    );
    fixture.publish(now + Duration::from_millis(1100));
    assert_eq!(
        fixture.requests(),
        before,
        "new mapped group has no stream yet"
    );
    fixture.attention.remove(&group(1));
    fixture.publish(now + Duration::from_millis(1200));
    assert!(fixture.requests()[0].bits_per_second < before[0].bits_per_second);
}

#[test]
fn another_ports_activity_drives_expiry_of_an_idle_peers_boost() {
    let now = Instant::now();
    let budget = SharedBitrateBudget::new(8_000_000).expect("budget");
    let (_rates1, member1, control1) = member(&budget);
    let (_rates2, member2, control2) = member(&budget);
    member1.update_inventory(demands()).expect("first");
    member2.update_inventory(demands()).expect("second");
    let attention = HashMap::from([(
        group(1),
        GroupAttention {
            interaction: Some(now),
            ..Default::default()
        },
    )]);
    member1.update_attention(&attention, now).expect("boost");
    member2
        .update_attention(&HashMap::new(), now)
        .expect("other port");
    let boosted = control1.streams().expect("first")[0]
        .requested
        .bits_per_second;
    assert!(
        boosted
            > control2.streams().expect("second")[0]
                .requested
                .bits_per_second
    );
    member2
        .update_attention(&HashMap::new(), now + Duration::from_secs(11))
        .expect("idle peer expires without its poll");
    assert!(
        control1.streams().expect("expired")[0]
            .requested
            .bits_per_second
            < boosted
    );
}

#[test]
fn quality_policy_rejects_zero_weights_and_invalid_holds() {
    let defaults = BitrateAllocationPolicy::default();
    for policy in [
        BitrateAllocationPolicy {
            weights: [1, 0, 2, 12],
            ..defaults
        },
        BitrateAllocationPolicy {
            interaction_hold: Duration::ZERO,
            ..defaults
        },
        BitrateAllocationPolicy {
            focus_settle: Duration::ZERO,
            ..defaults
        },
        BitrateAllocationPolicy {
            interaction_hold: Duration::MAX,
            ..defaults
        },
        BitrateAllocationPolicy {
            focus_settle: Duration::from_secs(61),
            ..defaults
        },
    ] {
        assert!(SharedBitrateBudget::with_policy(8_000_000, policy).is_err());
    }
}

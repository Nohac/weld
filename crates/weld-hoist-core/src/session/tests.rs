use super::*;
use futures_lite::future::block_on;
use std::time::Duration;
use weld_access::AccessAuthority;
use weld_client::{ClientId, ClientSourceId, ClientSurfaceId};

fn permissions() -> DevicePermissions {
    DevicePermissions {
        browse: true,
        hoist: true,
        thumbnails: true,
        diagnostics: false,
    }
}
fn app() -> ApplicationInfo {
    ApplicationInfo {
        window: 1,
        surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1),
        title: "Example".into(),
        app_id: "example".into(),
        available: true,
        hoisted_here: false,
    }
}
fn response<T>(mut pending: Pending<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(value) = pending.poll().expect("response") {
            return value;
        }
        assert!(Instant::now() < deadline, "service stalled");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn shared_owner_isolates_sessions_and_revocation_without_transport_state() {
    let service = SessionService::start(|| {}).expect("service");
    let other_binding = service.clone();
    assert_eq!(service.instance(), other_binding.instance());
    assert_ne!(
        service.instance(),
        SessionService::start(|| {})
            .expect("another host lifetime")
            .instance()
    );
    let authority = AccessAuthority::new(|| {});
    let first_access = authority.open("first".into()).expect("permit");
    let second_access = authority.open("second".into()).expect("permit");
    let first = response(
        service
            .join(first_access.clone(), permissions())
            .expect("join"),
    )
    .expect("joined");
    let second = response(
        other_binding
            .join(second_access.clone(), permissions())
            .expect("join"),
    )
    .expect("joined");
    assert_ne!(first, second);
    service.publish(first, vec![app()]).expect("catalogue");
    assert_eq!(
        block_on(service.catalogue(first)).expect("catalogue"),
        vec![app()]
    );
    assert!(
        block_on(other_binding.catalogue(second))
            .expect("catalogue")
            .is_empty()
    );
    let (answer, wait) = reply_channel();
    block_on(service.action(DeviceAction::Hoist {
        session: first,
        window: 1,
        answer,
    }))
    .expect("action");
    block_on(service.catalogue(first)).expect("catalogue");
    let actions = service.take_actions();
    assert_eq!(actions.len(), 1);
    service.leave(&first_access);
    let DeviceAction::Hoist { answer, .. } = actions.into_iter().next().expect("action") else {
        panic!("hoist");
    };
    assert!(
        answer.is_closed(),
        "held effects immediately observe revocation"
    );
    drop(answer);
    assert!(wait.0.recv_blocking().is_err());
    assert!(block_on(service.catalogue(first)).is_err());
    assert!(block_on(service.catalogue(second)).is_ok());
    drop(other_binding);
    drop(service);
    assert!(
        !second_access.is_authorized(),
        "last owner drop retires permits and joins the worker"
    );
}

#[test]
fn preview_policy_is_shared_and_late_completion_checks_current_occupancy() {
    let service = SessionService::start(|| {}).expect("service");
    let authority = AccessAuthority::new(|| {});
    let access = authority.open("viewer".into()).expect("permit");
    let id = response(service.join(access.clone(), permissions()).expect("join")).expect("joined");
    service.publish(id, vec![app()]).expect("catalogue");
    let request = ThumbnailRequest {
        target: ThumbnailTarget {
            window: 1,
            surface: app().surface,
        },
        if_revision: None,
    };
    assert_eq!(
        response(service.thumbnail(id, request).expect("request"))
            .reply
            .status,
        ThumbnailStatus::Pending
    );
    let capture = response(service.take_thumbnail_capture().expect("capture")).expect("ticket");
    let image = ThumbnailImage::new(1, 1, vec![1]).expect("fixture");
    service.publish(id, Vec::new()).expect("removed");
    assert!(!response(
        service
            .complete_thumbnail_capture(capture, Some(image))
            .expect("complete")
    ));
    assert_eq!(
        response(service.thumbnail(id, request).expect("request"))
            .reply
            .status,
        ThumbnailStatus::Unavailable
    );
}

#[test]
fn cancellation_and_capacity_keep_the_owner_bounded() {
    let (actions, receive_actions) = async_channel::bounded(1);
    let mut controller = Controller {
        sessions: BTreeMap::new(),
        next: 0,
        thumbnails: Cache::default(),
        actions,
        notify: Box::new(|| {}),
    };
    let authority = AccessAuthority::new(|| {});
    let access = authority.open("viewer".into()).expect("permit");
    let id = controller
        .join(access.clone(), permissions())
        .expect("join");
    let (answer, pending) = reply_channel();
    drop(pending);
    controller.handle(
        Event::Action(DeviceAction::Release {
            session: id,
            answer,
        }),
        Instant::now(),
    );
    assert!(receive_actions.is_empty());
    let (first, _wait) = reply_channel();
    controller.handle(
        Event::Action(DeviceAction::Release {
            session: id,
            answer: first,
        }),
        Instant::now(),
    );
    let (second, mut wait) = reply_channel();
    controller.handle(
        Event::Action(DeviceAction::Release {
            session: id,
            answer: second,
        }),
        Instant::now(),
    );
    assert_eq!(wait.poll().expect("busy result"), Some(false));
    assert_eq!(receive_actions.len(), 1);
    access.end();
    controller.handle(Event::Wake, Instant::now());
    assert!(controller.sessions.is_empty());
}

#[test]
fn catalogue_subscription_coalesces_changes_and_is_quiet_when_unchanged() {
    let service = SessionService::start(|| {}).expect("service");
    let authority = AccessAuthority::new(|| {});
    let access = authority.open("viewer".into()).expect("permit");
    let id = response(service.join(access.clone(), permissions()).expect("join")).expect("joined");
    let subscription = block_on(service.subscribe(id)).expect("subscription");
    assert_eq!(
        subscription.receiver.try_recv().expect("initial").revision,
        0
    );
    service.publish(id, vec![app()]).expect("publish");
    let mut changed = app();
    changed.title = "Changed".into();
    service.publish(id, vec![changed.clone()]).expect("replace");
    block_on(service.catalogue(id)).expect("catalogue");
    let snapshot = subscription.receiver.try_recv().expect("coalesced update");
    assert_eq!(snapshot.revision, 2);
    assert_eq!(snapshot.applications.as_ref(), &[changed.clone()]);
    assert!(subscription.receiver.is_empty());
    service.publish(id, vec![changed]).expect("unchanged");
    block_on(service.catalogue(id)).expect("catalogue");
    assert!(
        subscription.receiver.is_empty(),
        "identical publication causes no notification"
    );
    service.leave(&access);
    assert!(block_on(service.catalogue(id)).is_err());
    assert!(subscription.receiver.is_closed());
}

#[test]
fn concurrent_service_handles_share_capture_budget_and_cache() {
    let service = SessionService::start(|| {}).expect("service");
    let other = service.clone();
    let authority = AccessAuthority::new(|| {});
    let first_access = authority.open("one".into()).expect("permit");
    let first = response(
        service
            .join(first_access.clone(), permissions())
            .expect("join"),
    )
    .expect("joined");
    let second = response(
        other
            .join(authority.open("two".into()).expect("permit"), permissions())
            .expect("join"),
    )
    .expect("joined");
    service
        .publish(first, vec![app()])
        .expect("first catalogue");
    other
        .publish(second, vec![app()])
        .expect("second catalogue");
    let request = ThumbnailRequest {
        target: ThumbnailTarget {
            window: 1,
            surface: app().surface,
        },
        if_revision: None,
    };
    response(service.thumbnail(first, request).expect("preview"));
    response(other.thumbnail(second, request).expect("preview"));
    let capture = response(service.take_thumbnail_capture().expect("capture")).expect("ticket");
    assert!(response(other.take_thumbnail_capture().expect("capture")).is_none());
    service.leave(&first_access);
    assert!(
        response(
            other
                .complete_thumbnail_capture(
                    capture,
                    Some(ThumbnailImage::new(1, 1, vec![1]).expect("fixture"))
                )
                .expect("complete")
        ),
        "remaining viewer keeps shared work alive"
    );
    assert!(matches!(
        response(other.thumbnail(second, request).expect("preview"))
            .reply
            .status,
        ThumbnailStatus::Image { .. }
    ));
    assert_eq!(
        response(service.thumbnail(first, request).expect("preview"))
            .reply
            .status,
        ThumbnailStatus::Unavailable
    );
}

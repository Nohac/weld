use super::*;
use weld_client::{ClientId, ClientSourceId, ClientSurfaceId};

fn target(window: u64) -> ThumbnailTarget {
    ThumbnailTarget {
        window,
        surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), window),
    }
}
fn request(window: u64) -> ThumbnailRequest {
    ThumbnailRequest {
        target: target(window),
        if_revision: None,
    }
}
fn image() -> ThumbnailImage {
    ThumbnailImage::new(2, 1, vec![1, 2, 3]).expect("image fixture")
}

#[test]
fn requests_coalesce_and_dirty_refresh_preserves_the_cached_revision() {
    let mut cache = Cache::default();
    let now = Instant::now();
    assert_eq!(
        cache.request(request(1), now).reply.status,
        ThumbnailStatus::Pending
    );
    let first = cache.take(now).expect("capture");
    cache.request(request(1), now);
    assert!(cache.take(now).is_none());
    assert!(cache.complete(first, Some(image()), now));
    assert!(
        cache.take(now + REFRESH_INTERVAL).is_none(),
        "coalesced requests require one capture"
    );
    let conditional = ThumbnailRequest {
        if_revision: Some(first.revision),
        ..request(1)
    };
    assert!(matches!(
        cache.request(conditional, now).reply.status,
        ThumbnailStatus::Unchanged {
            freshness: ThumbnailFreshness { stale: false, .. },
            ..
        }
    ));
    cache.entries.get_mut(&target(1)).expect("entry").dirty = true;
    assert!(matches!(
        cache.request(conditional, now).reply.status,
        ThumbnailStatus::Unchanged {
            freshness: ThumbnailFreshness { stale: true, .. },
            ..
        }
    ));
    assert!(
        cache.take(now).is_none(),
        "dirty commits respect the refresh budget"
    );
    let next = cache.take(now + REFRESH_INTERVAL).expect("refresh");
    cache.entries.get_mut(&target(1)).expect("entry").dirty = true;
    assert!(cache.complete(next, Some(image()), now + REFRESH_INTERVAL));
    assert!(
        matches!(
            cache
                .request(conditional, now + REFRESH_INTERVAL)
                .reply
                .status,
            ThumbnailStatus::Image {
                freshness: ThumbnailFreshness { stale: true, .. },
                ..
            }
        ),
        "a commit during capture stays dirty"
    );
}

#[test]
fn replacement_and_timeout_reject_late_pixels_and_storage_stays_bounded() {
    let mut cache = Cache::default();
    let now = Instant::now();
    cache.request(request(1), now);
    let removed = cache.take(now).expect("capture");
    cache.retain(|_| false);
    cache.request(request(1), now);
    let replacement = cache.take(now).expect("replacement");
    assert_ne!(removed.revision, replacement.revision);
    assert!(!cache.complete(removed, Some(image()), now));
    let retry = cache.take(now + CAPTURE_TIMEOUT).expect("timeout retry");
    assert!(!cache.complete(replacement, Some(image()), now + CAPTURE_TIMEOUT));
    assert!(cache.complete(retry, None, now + CAPTURE_TIMEOUT));
    for window in 2..100 {
        cache.request(request(window), now);
        assert!(cache.entries.len() <= CACHE_ENTRIES);
    }
}

#[test]
fn catalogue_access_and_revocation_guard_capture_and_cached_pixels() {
    let notifier = IrohNotifier::new(|| Ok(()));
    let desktop = DesktopSessions::new(notifier.clone(), notifier, weld_media::VideoCodec::Av1);
    let authority = weld_access::AccessAuthority::new(|| {});
    let access = authority.open("test".into()).expect("access");
    let session = SessionId(1);
    desktop.state.lock().expect("state").catalogues.insert(
        session,
        (
            vec![super::super::ApplicationInfo {
                window: 1,
                surface: target(1).surface,
                title: "Test".into(),
                app_id: "test".into(),
                available: true,
                hoisted_here: false,
            }],
            access.clone(),
        ),
    );
    assert_eq!(
        desktop.thumbnail(SessionId(2), request(1)).reply.status,
        ThumbnailStatus::Unavailable
    );
    assert_eq!(
        desktop.thumbnail(session, request(2)).reply.status,
        ThumbnailStatus::Unavailable
    );
    assert!(desktop.take_thumbnail_capture().is_none());
    desktop.thumbnail(session, request(1));
    let capture = desktop.take_thumbnail_capture().expect("capture");
    access.end();
    assert!(!desktop.complete_thumbnail_capture(capture, Some(image())));
    assert_eq!(
        desktop.thumbnail(session, request(1)).reply.status,
        ThumbnailStatus::Unavailable
    );
    assert!(
        desktop
            .state
            .lock()
            .expect("state")
            .thumbnails
            .entries
            .is_empty()
    );
}

#[tokio::test]
async fn wire_rejects_extent_payload_target_and_conditional_mismatches() {
    let freshness = ThumbnailFreshness {
        age_ms: 0,
        stale: false,
    };
    for status in [
        ThumbnailStatus::Image {
            revision: ThumbnailRevision(1),
            freshness,
            image: ThumbnailImageHeader {
                width: 321,
                height: 1,
                png_bytes: 1,
            },
        },
        ThumbnailStatus::Image {
            revision: ThumbnailRevision(1),
            freshness,
            image: ThumbnailImageHeader {
                width: 1,
                height: 1,
                png_bytes: u32::MAX,
            },
        },
        ThumbnailStatus::Unchanged {
            revision: ThumbnailRevision(1),
            freshness,
        },
    ] {
        let mut wire = Vec::new();
        write_record(
            &mut wire,
            &ThumbnailReply {
                target: target(1),
                status,
            },
        )
        .await
        .expect("header");
        assert!(
            read_response(&mut wire.as_slice(), request(1))
                .await
                .is_err()
        );
    }
    let mut wire = Vec::new();
    write_response(
        &mut wire,
        &ThumbnailResponse::status(target(2), ThumbnailStatus::Pending),
    )
    .await
    .expect("reply");
    assert!(
        read_response(&mut wire.as_slice(), request(1))
            .await
            .is_err()
    );
    let response = ThumbnailResponse {
        reply: ThumbnailReply {
            target: target(1),
            status: ThumbnailStatus::Image {
                revision: ThumbnailRevision(1),
                freshness,
                image: image().header(),
            },
        },
        image: Some(image()),
    };
    wire.clear();
    write_response(&mut wire, &response)
        .await
        .expect("image write");
    wire.pop();
    assert!(
        read_response(&mut wire.as_slice(), request(1))
            .await
            .is_err(),
        "truncated payload"
    );
}

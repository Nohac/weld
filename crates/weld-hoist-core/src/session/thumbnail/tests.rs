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

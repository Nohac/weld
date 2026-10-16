use super::*;
use anyhow::Context;
use futures_lite::future::block_on;
use std::collections::VecDeque;
use weld_client::{ClientId, ClientSourceId, ClientSurfaceId};
use weld_hoist_protocol::thumbnail::{
    ThumbnailFreshness, ThumbnailImageHeader, ThumbnailReply, ThumbnailRevision, ThumbnailTarget,
};

#[derive(Default)]
struct MemoryLane(VecDeque<Vec<u8>>);
impl PacketSend for MemoryLane {
    async fn finish(&mut self) -> Result<()> {
        Ok(())
    }
    async fn send(&mut self, packet: &[u8]) -> Result<()> {
        self.0.push_back(packet.to_vec());
        Ok(())
    }
}
impl PacketReceive for MemoryLane {
    async fn receive(&mut self, maximum: usize) -> Result<Vec<u8>> {
        let packet = self.0.pop_front().context("lane empty")?;
        ensure!(packet.len() <= maximum, "packet too large");
        Ok(packet)
    }
}
fn request() -> ThumbnailRequest {
    ThumbnailRequest {
        target: ThumbnailTarget {
            window: 1,
            surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1),
        },
        if_revision: None,
    }
}

#[test]
fn thumbnail_protocol_checks_target_dimensions_payload_and_revision() {
    block_on(async {
        let request = request();
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
            let mut lane = MemoryLane::default();
            send_record(
                &mut lane,
                &ServiceReply::Thumbnail(ThumbnailReply {
                    target: request.target,
                    status,
                }),
            )
            .await
            .expect("header");
            assert!(read_thumbnail(&mut lane, request).await.is_err());
        }
        let mut lane = MemoryLane::default();
        let target = ThumbnailTarget {
            window: 2,
            ..request.target
        };
        write_thumbnail(
            &mut lane,
            &ThumbnailResponse::status(target, ThumbnailStatus::Pending),
        )
        .await
        .expect("reply");
        assert!(read_thumbnail(&mut lane, request).await.is_err());
        let image = ThumbnailImage::new(2, 1, vec![1, 2, 3]).expect("image fixture");
        let response = ThumbnailResponse {
            reply: ThumbnailReply {
                target: request.target,
                status: ThumbnailStatus::Image {
                    revision: ThumbnailRevision(1),
                    freshness,
                    image: image.header(),
                },
            },
            image: Some(image),
        };
        write_thumbnail(&mut lane, &response).await.expect("image");
        assert_eq!(
            read_thumbnail(&mut lane, request)
                .await
                .expect("image reply")
                .image
                .expect("payload")
                .png(),
            &[1, 2, 3]
        );
        write_thumbnail(&mut lane, &response).await.expect("image");
        lane.0.back_mut().expect("payload packet").pop();
        assert!(read_thumbnail(&mut lane, request).await.is_err());
    });
}

use super::*;
use crate::session::{SessionId, SessionService};
use anyhow::Context;
use async_io::Timer;
use futures_lite::future::{block_on, zip};
use weld_access::AccessAuthority;
use weld_client::{ClientId, ClientSourceId, ClientSurfaceId};

struct Writer(Sender<Vec<u8>>);
struct Reader(Receiver<Vec<u8>>);
impl PacketSend for Writer {
    async fn finish(&mut self) -> Result<()> {
        Ok(())
    }
    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.send(bytes.to_vec()).await.context("closed writer")
    }
}
impl PacketReceive for Reader {
    async fn receive(&mut self, maximum: usize) -> Result<Vec<u8>> {
        let bytes = self.0.recv().await.context("closed reader")?;
        ensure!(bytes.len() <= maximum, "packet exceeds limit");
        Ok(bytes)
    }
}
fn pipe() -> (Writer, Reader) {
    let (send, receive) = async_channel::bounded(4);
    (Writer(send), Reader(receive))
}
async fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready() {
        assert!(Instant::now() < deadline, "driver stalled");
        Timer::after(Duration::from_millis(1)).await;
    }
}

#[test]
fn shared_control_driver_delivers_changes_and_authorized_actions_then_cancels() {
    block_on(async {
        let service = SessionService::start(|| {}).expect("service");
        let authority = AccessAuthority::new(|| {});
        let access = authority.open("test".into()).expect("permit");
        let grant = DevicePermissions {
            browse: true,
            hoist: true,
            ..Default::default()
        };
        let id = service
            .join(access.clone(), grant)
            .expect("join")
            .wait()
            .await
            .expect("response")
            .expect("joined");
        let context = ServiceSession {
            service: service.clone(),
            id,
            access: access.clone(),
        };
        let (server_write, client_read) = pipe();
        let (client_write, server_read) = pipe();
        let (client, driver) = DeviceClient::new(|| {});
        assert!(client.is_connected());
        let application = ApplicationInfo {
            window: 5,
            surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1),
            title: "First".into(),
            app_id: "test".into(),
            available: true,
            hoisted_here: false,
        };
        service
            .publish(id, vec![application.clone()])
            .expect("publish");
        let exercising = async {
            until(|| client.applications() == vec![application.clone()]).await;
            let mut changed = application;
            changed.title = "Changed".into();
            service.publish(id, vec![changed.clone()]).expect("publish");
            until(|| client.applications() == vec![changed.clone()]).await;
            client.hoist(5).expect("request");
            until(|| {
                let actions = service.take_actions();
                let received = !actions.is_empty();
                for action in actions {
                    let DeviceAction::Hoist {
                        session,
                        window,
                        answer,
                    } = action
                    else {
                        panic!("hoist");
                    };
                    assert_eq!(session, id);
                    assert_eq!(window, 5);
                    answer.send(false).expect("decline");
                }
                received
            })
            .await;
            until(|| client.take_error().is_some()).await;
            assert!(
                client.is_connected(),
                "an operation rejection keeps the catalogue connection alive"
            );
            drop(client);
        };
        let (_, (client_result, server_result)) = zip(
            exercising,
            zip(
                driver.run(client_write, client_read, None),
                context.serve_control(server_write, server_read, || Some(grant), |_| None, None),
            ),
        )
        .await;
        assert!(client_result.is_ok());
        assert!(
            server_result.is_err(),
            "closed packet lane ends server driver"
        );
        service.leave(&access);
        assert!(service.catalogue(id).await.is_err());
    });
}

#[test]
fn losing_the_control_driver_is_observable_without_a_media_failure() {
    let (client, driver) = DeviceClient::new(|| {});
    assert!(client.is_connected());
    drop(driver);
    assert!(!client.is_connected());
}

#[test]
fn revoked_session_cannot_dispatch_a_queued_command() {
    block_on(async {
        let service = SessionService::start(|| {}).expect("service");
        let authority = AccessAuthority::new(|| {});
        let access = authority.open("test".into()).expect("permit");
        access.end();
        let context = ServiceSession {
            service: service.clone(),
            id: SessionId(1),
            access,
        };
        let (mut sender, reader) = pipe();
        let (writer, _receiver) = pipe();
        send_record(&mut sender, &DeviceRequest::Hoist(5))
            .await
            .expect("queued");
        assert!(
            context
                .serve_control(
                    writer,
                    reader,
                    || Some(DevicePermissions {
                        browse: true,
                        hoist: true,
                        ..Default::default()
                    }),
                    |_| None,
                    None
                )
                .await
                .is_err()
        );
        assert!(service.take_actions().is_empty());
    });
}

//! Real Wayland requests against mock physical-output application callbacks.
use super::*;
use crate::gamma::GammaRamp;
use std::process::{Child, Command, Stdio};
use wayland_protocols_wlr::gamma_control::v1::client::{
    zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1,
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};

#[derive(Default)]
pub(super) struct Probe {
    pub manager: Option<ZwlrGammaControlManagerV1>,
    sizes: HashMap<u32, u32>,
    failed: HashSet<u32>,
}
delegate_noop!(Observer: ignore ZwlrGammaControlManagerV1);
impl Dispatch<ZwlrGammaControlV1, u32> for Observer {
    fn event(
        state: &mut Self,
        _: &ZwlrGammaControlV1,
        event: zwlr_gamma_control_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => {
                state.gamma.sizes.insert(*id, size);
            }
            zwlr_gamma_control_v1::Event::Failed => {
                state.gamma.failed.insert(*id);
            }
            _ => {}
        }
    }
}
fn control(f: &Fixture, output: u32, id: u32) -> ZwlrGammaControlV1 {
    let output = f
        .observer
        .output_objects
        .iter()
        .find(|object| {
            f.observer.outputs.get(&object.id().protocol_id()) == Some(&format!("test-{output}"))
        })
        .expect("output");
    f.observer
        .gamma
        .manager
        .as_ref()
        .expect("gamma global")
        .get_gamma_control(output, &f.queue.handle(), id)
}
fn table(size: u16, offset: u16) -> File {
    let mut file = tempfile::tempfile().expect("table file");
    for value in (0..size)
        .map(|i| offset + i)
        .chain((0..size).map(|i| offset + 100 + i))
        .chain((0..size).map(|i| offset + 200 + i))
    {
        file.write_all(&value.to_ne_bytes()).expect("table data");
    }
    file
}
fn applied(f: &mut Fixture, output: u32) -> Vec<Option<GammaRamp>> {
    let mut applied = Vec::new();
    f.server
        .service_gamma(OutputId::new(output as u64), |ramp| {
            applied.push(ramp.cloned());
            Ok(())
        })
        .expect("mock gamma apply");
    applied
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn gamma_is_not_advertised_by_ordinary_servers() {
    let f = Fixture::new();
    assert!(f.observer.gamma.manager.is_none());
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn gamma_coalesces_per_output_and_restores_on_owner_destruction() {
    let mut f = Fixture::with_protocols(false, true);
    let first = control(&f, 1, 1);
    let other = control(&f, 2, 2);
    let rejected = control(&f, 1, 3);
    f.sync();
    assert_eq!(f.observer.gamma.sizes.get(&1), Some(&4));
    assert_eq!(f.observer.gamma.sizes.get(&2), Some(&8));
    assert!(f.observer.gamma.failed.contains(&3));
    first.set_gamma(table(4, 10).as_fd());
    first.set_gamma(table(4, 20).as_fd());
    other.set_gamma(table(8, 30).as_fd());
    f.sync();
    let expected = GammaRamp::read(&table(4, 20), 4).expect("ramp");
    assert_eq!(applied(&mut f, 1), [Some(expected.clone())]);
    assert!(applied(&mut f, 1).is_empty());
    assert_eq!(applied(&mut f, 2)[0].as_ref().expect("ramp").len(), 8);
    first.set_gamma(table(4, 20).as_fd());
    f.sync();
    assert!(
        applied(&mut f, 1).is_empty(),
        "unchanged LUT does not commit"
    );
    // The manager and failed controller own no active tables.
    f.observer
        .gamma
        .manager
        .as_ref()
        .expect("manager")
        .destroy();
    rejected.destroy();
    f.sync();
    assert!(applied(&mut f, 1).is_empty());
    first.destroy();
    f.sync();
    assert_eq!(applied(&mut f, 1), [None]);
    assert!(applied(&mut f, 2).is_empty());
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn gamma_reapplies_after_resume_and_fails_only_the_affected_output() {
    let mut f = Fixture::with_protocols(false, true);
    let first = control(&f, 1, 1);
    let second = control(&f, 2, 2);
    first.set_gamma(table(4, 5).as_fd());
    second.set_gamma(table(8, 6).as_fd());
    f.sync();
    let expected = applied(&mut f, 1);
    applied(&mut f, 2);
    f.server.reapply_gamma();
    assert_eq!(applied(&mut f, 1), expected);
    assert!(
        f.server
            .service_gamma(OutputId::new(2), |_| anyhow::bail!("mock DRM rejection"))
            .is_err()
    );
    f.sync();
    assert!(!f.observer.gamma.failed.contains(&1));
    assert!(f.observer.gamma.failed.contains(&2));
    let unsupported = control(&f, 2, 3);
    f.sync();
    assert!(f.observer.gamma.failed.contains(&3));
    unsupported.destroy();
    second.destroy();
    f.server.remove_gamma_output(OutputId::new(1));
    f.sync();
    assert!(f.observer.gamma.failed.contains(&1));
    f.server.enable_gamma_control([(OutputId::new(1), 4)]);
    let replacement = control(&f, 1, 4);
    f.sync();
    first.destroy();
    replacement.set_gamma(table(4, 9).as_fd());
    f.sync();
    assert_eq!(applied(&mut f, 1)[0].as_ref().expect("new owner").red[0], 9);
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn gamma_disconnect_restores_original_tables() {
    let mut f = Fixture::with_protocols(false, true);
    let first = control(&f, 1, 1);
    first.set_gamma(table(4, 4).as_fd());
    f.sync();
    applied(&mut f, 1);
    let Fixture {
        mut event_loop,
        mut server,
        connection,
        queue,
        observer,
        ..
    } = f;
    drop((first, connection, queue, observer));
    event_loop
        .dispatch(Some(Duration::from_millis(10)), &mut server)
        .expect("disconnect");
    let mut restored = Vec::new();
    server
        .service_gamma(OutputId::new(1), |ramp| {
            restored.push(ramp.cloned());
            Ok(())
        })
        .expect("restore on disconnect");
    assert_eq!(restored, [None]);
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn gamma_invalid_fd_posts_protocol_error_and_releases_the_output() {
    let mut f = Fixture::with_protocols(false, true);
    let first = control(&f, 1, 1);
    first.set_gamma(table(4, 4).as_fd());
    f.sync();
    applied(&mut f, 1);
    first.set_gamma(table(3, 4).as_fd());
    f.connection.flush().expect("flush invalid request");
    f.event_loop
        .dispatch(Some(Duration::from_millis(10)), &mut f.server)
        .expect("dispatch invalid request");
    f.server.flush_clients();
    let _ = f.queue.prepare_read().expect("read guard").read();
    let error = f
        .connection
        .protocol_error()
        .expect("invalid gamma protocol error");
    assert_eq!(
        error.code,
        zwlr_gamma_control_v1::Error::InvalidGamma as u32
    );
    assert_eq!(error.object_id, first.id().protocol_id());
    assert_eq!(applied(&mut f, 1), [None]);
}

// Always reap the external client, including when a fixture assertion fails.
struct GammaClient(Child);
impl Drop for GammaClient {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires local sockets and gammastep in PATH; uses mock outputs only"]
fn gamma_gammastep_sets_both_outputs_and_disconnect_restores_them() {
    let mut f = Fixture::with_protocols(false, true);
    let mut client = GammaClient(
        Command::new("gammastep")
            .args([
                "-c",
                "/dev/null",
                "-m",
                "wayland",
                "-l",
                "0:0",
                "-t",
                "4500:4500",
                "-r",
            ])
            .env("WAYLAND_DISPLAY", &f.server.socket_name)
            .env_remove("WAYLAND_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("start gammastep"),
    );
    let mut received = HashMap::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while received.len() < 2 && Instant::now() < deadline {
        assert!(
            client.0.try_wait().expect("client status").is_none(),
            "gammastep exited before setting both outputs"
        );
        f.event_loop
            .dispatch(Some(Duration::from_millis(10)), &mut f.server)
            .expect("gammastep requests");
        f.server.flush_clients();
        for id in [1, 2] {
            for ramp in applied(&mut f, id).into_iter().flatten() {
                received.insert(id, ramp);
            }
        }
    }
    assert_eq!(
        received.len(),
        2,
        "gammastep must set both physical outputs"
    );
    for (id, ramp) in received {
        assert_eq!(ramp.len(), if id == 1 { 4 } else { 8 });
        assert!(
            ramp.blue.last() < ramp.red.last(),
            "4500K reduces blue relative to red"
        );
    }
    drop(client);
    let mut restored = HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while restored.len() < 2 && Instant::now() < deadline {
        f.event_loop
            .dispatch(Some(Duration::from_millis(10)), &mut f.server)
            .expect("client disconnect");
        for id in [1, 2] {
            if applied(&mut f, id) == [None] {
                restored.insert(id);
            }
        }
    }
    assert_eq!(
        restored.len(),
        2,
        "disconnect restores both original tables"
    );
}

#[test]
fn gamma_fd_validation_checks_layout_size_and_rejects_pipes() {
    let file = table(4, 5);
    let ramp = GammaRamp::read(&file, 4).expect("ramp");
    assert_eq!(ramp.red, [5, 6, 7, 8]);
    assert_eq!(ramp.green, [105, 106, 107, 108]);
    assert_eq!(ramp.blue, [205, 206, 207, 208]);
    let packed: Vec<_> = ramp
        .drm_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| u16::from_ne_bytes(*bytes))
        .collect();
    assert_eq!(&packed[..8], &[5, 105, 205, 0, 6, 106, 206, 0]);
    assert!(GammaRamp::read(&file, 3).is_err());
    assert!(GammaRamp::read(&file, 0).is_err());
    assert!(GammaRamp::read(&file, crate::gamma::MAX_GAMMA_ENTRIES + 1).is_err());
    file.set_len(1).expect("truncate");
    assert!(GammaRamp::read(&file, 4).is_err());
    let (reader, _writer) = rustix::pipe::pipe().expect("pipe");
    assert!(GammaRamp::read(&File::from(reader), 4).is_err());
}

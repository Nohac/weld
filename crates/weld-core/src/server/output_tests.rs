//! Real protocol regression for event-driven output and scale propagation.

#[path = "cursor_tests.rs"]
mod cursor_tests;
#[path = "layer_tests.rs"]
mod layer_tests;
#[path = "workspace_tests.rs"]
mod workspace_tests;

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Write,
    os::fd::AsFd,
    os::unix::net::UnixStream,
    sync::Arc,
    time::{Duration, Instant},
};

use calloop::{EventLoop, channel};
use rustix::fs::{MemfdFlags, memfd_create};
use smithay::reexports::wayland_server::{Client, Display};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool,
        wl_subcompositor, wl_subsurface, wl_surface,
    },
};
use wayland_protocols::{
    wp::fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1},
    xdg::shell::client::{xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base},
};

use super::{
    ClientState, OutputDescriptor, OutputMetrics, ServerOptions, ServerOutputDefinition,
    ServerState, WaylandClientBridge,
};
use crate::{
    OutputId, OutputScale, dmabuf::DmabufSourceCache, input::KeyboardRepeatMode, surface::SurfaceId,
};
use weld_client::{
    ClientId, ClientPresentationClaim, ClientPresentationUpdate, ClientSourceId, PresentationRate,
};

#[derive(Default)]
struct ObservedSurface {
    outputs: HashSet<u32>,
    fractional_scale: u32,
}

#[derive(Default)]
struct Observer {
    workspaces: workspace_tests::Probe,
    layers: Option<
        wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::ZwlrLayerShellV1,
    >,
    layer_configures: Vec<(u32, u32)>,
    popup_configures: Vec<(i32, i32, i32, i32)>,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    subsurfaces: Option<wl_subcompositor::WlSubcompositor>,
    shell: Option<xdg_wm_base::XdgWmBase>,
    fractional: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    outputs: HashMap<u32, String>,
    output_objects: Vec<wl_output::WlOutput>,
    surfaces: HashMap<u32, ObservedSurface>,
    frames: usize,
    toplevel_configures: Vec<(i32, i32, Vec<u32>)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Observer {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "ext_workspace_manager_v1" => {
                    state.workspaces.manager = Some(registry.bind(name, 1, qh, ()));
                }
                "zwlr_layer_shell_v1" => {
                    state.layers = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(6), qh, ()))
                }
                "wl_subcompositor" => state.subsurfaces = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => state.shell = Some(registry.bind(name, version.min(6), qh, ())),
                "wp_fractional_scale_manager_v1" => {
                    state.fractional = Some(registry.bind(name, 1, qh, ()))
                }
                "wl_output" => {
                    state.workspaces.output_registry = Some((registry.clone(), name));
                    state
                        .output_objects
                        .push(registry.bind::<wl_output::WlOutput, _, _>(name, 4, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Observer {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.outputs.insert(output.id().protocol_id(), name);
        }
    }
}

impl Dispatch<wl_surface::WlSurface, u32> for Observer {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let surface = state.surfaces.entry(*id).or_default();
        match event {
            wl_surface::Event::Enter { output } => {
                surface.outputs.insert(output.id().protocol_id());
            }
            wl_surface::Event::Leave { output } => {
                surface.outputs.remove(&output.id().protocol_id());
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, u32> for Observer {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.surfaces.entry(*id).or_default().fractional_scale = scale;
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Observer {
    fn event(
        _: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
        }
    }
}

delegate_noop!(Observer: ignore wl_callback::WlCallback);
delegate_noop!(Observer: ignore wl_shm::WlShm);
delegate_noop!(Observer: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Observer: ignore wl_buffer::WlBuffer);
delegate_noop!(Observer: ignore wl_compositor::WlCompositor);
delegate_noop!(Observer: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(Observer: ignore wl_subsurface::WlSubsurface);
delegate_noop!(Observer: ignore xdg_wm_base::XdgWmBase);
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Observer {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure {
            width,
            height,
            states,
        } = event
        {
            let states = states
                .chunks_exact(4)
                .map(|bytes| u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                .collect();
            state.toplevel_configures.push((width, height, states));
        }
    }
}
delegate_noop!(Observer: ignore xdg_positioner::XdgPositioner);
impl Dispatch<xdg_popup::XdgPopup, ()> for Observer {
    fn event(
        state: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_popup::Event::Configure {
            x,
            y,
            width,
            height,
        } = event
        {
            state.popup_configures.push((x, y, width, height));
        }
    }
}
delegate_noop!(Observer: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);

impl Dispatch<wl_callback::WlCallback, bool> for Observer {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &bool,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.frames += 1;
    }
}

struct Fixture {
    event_loop: EventLoop<'static, ServerState>,
    server: ServerState,
    client: Client,
    connection: Connection,
    queue: EventQueue<Observer>,
    observer: Observer,
}

impl Fixture {
    fn new() -> Self {
        let event_loop = EventLoop::try_new().expect("event loop");
        let (_, receiver) = channel::channel();
        let display = Display::new().expect("display");
        let mut handle = display.handle();
        let socket = std::env::temp_dir().join(format!("weld-output-test-{}", std::process::id()));
        let server = ServerState::new(
            &event_loop.handle(),
            display,
            receiver,
            WaylandClientBridge::default(),
            ServerOptions {
                started_at: Instant::now(),
                seat_name: "output-test",
                socket_name: Some(socket.to_str().expect("socket path")),
                dmabuf_capabilities: None,
                dmabuf_sources: DmabufSourceCache::unavailable(),
                keyboard_repeat_mode: KeyboardRepeatMode::Client,
                initial_toplevel_size: None,
                outputs: [1, 2]
                    .into_iter()
                    .map(|id| {
                        let mut descriptor = OutputDescriptor::nested();
                        descriptor.name = format!("test-{id}");
                        ServerOutputDefinition {
                            id: OutputId::new(id),
                            primary: id == 1,
                            logical_position: (0, 0),
                            descriptor,
                            metrics: OutputMetrics::new(
                                800,
                                600,
                                OutputScale::new(id as f64).expect("scale"),
                            )
                            .expect("metrics"),
                        }
                    })
                    .collect(),
            },
        )
        .expect("server");
        let (server_socket, client_socket) = UnixStream::pair().expect("socket pair");
        let client = handle
            .insert_client(
                server_socket,
                Arc::new(ClientState::new(ClientId::new(
                    crate::WAYLAND_CLIENT_SOURCE,
                    1,
                ))),
            )
            .expect("client");
        let connection = Connection::from_socket(client_socket).expect("connection");
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut fixture = Self {
            event_loop,
            server,
            client,
            connection,
            queue,
            observer: Observer::default(),
        };
        fixture.sync();
        fixture.sync();
        fixture
    }

    fn sync(&mut self) {
        self.connection.display().sync(&self.queue.handle(), ());
        self.connection.flush().expect("flush requests");
        self.event_loop
            .dispatch(Some(Duration::from_millis(10)), &mut self.server)
            .expect("dispatch");
        self.server.flush_clients();
        self.queue
            .prepare_read()
            .expect("read guard")
            .read()
            .expect("read events");
        self.queue
            .dispatch_pending(&mut self.observer)
            .expect("dispatch client events");
    }

    fn surface(&self, id: u32) -> wl_surface::WlSurface {
        let surface = self
            .observer
            .compositor
            .as_ref()
            .expect("compositor")
            .create_surface(&self.queue.handle(), id);
        self.observer
            .fractional
            .as_ref()
            .expect("fractional scale")
            .get_fractional_scale(&surface, &self.queue.handle(), id);
        surface
    }

    fn toplevel(
        &mut self,
        surface: &wl_surface::WlSurface,
    ) -> (SurfaceId, xdg_surface::XdgSurface) {
        let xdg = self
            .observer
            .shell
            .as_ref()
            .expect("shell")
            .get_xdg_surface(surface, &self.queue.handle(), ());
        xdg.get_toplevel(&self.queue.handle(), ());
        surface.commit();
        self.sync();
        let root = self.client.object_from_protocol_id::<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>(
            &self.server.display_handle, surface.id().protocol_id()).expect("server surface");
        let id = self
            .server
            .toplevels
            .id_for_surface(&root)
            .expect("toplevel id");
        (id, xdg)
    }

    fn buffer(&self) -> wl_buffer::WlBuffer {
        let mut file = File::from(memfd_create("output-test", MemfdFlags::CLOEXEC).expect("memfd"));
        file.write_all(&[0x80; 16]).expect("pixels");
        let pool = self.observer.shm.as_ref().expect("shm").create_pool(
            file.as_fd(),
            16,
            &self.queue.handle(),
            (),
        );
        let buffer = pool.create_buffer(
            0,
            2,
            2,
            8,
            wl_shm::Format::Xrgb8888,
            &self.queue.handle(),
            (),
        );
        pool.destroy();
        buffer
    }

    fn assert_output(&self, id: u32, output: &str, scale_120: u32) {
        let observed = &self.observer.surfaces[&id];
        assert_eq!(observed.fractional_scale, scale_120);
        let names = observed
            .outputs
            .iter()
            .map(|id| self.observer.outputs[id].as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, [output]);
    }
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn fullscreen_size_and_state_arrive_in_one_configure_and_restore_constraints() {
    use crate::surface::Extent;
    let mut f = Fixture::new();
    let surface = f.surface(40);
    let xdg =
        f.observer
            .shell
            .as_ref()
            .expect("shell")
            .get_xdg_surface(&surface, &f.queue.handle(), ());
    let toplevel = xdg.get_toplevel(&f.queue.handle(), ());
    toplevel.set_max_size(400, 300);
    toplevel.set_fullscreen(None);
    surface.commit();
    f.sync();
    let native = f.client.object_from_protocol_id::<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>(&f.server.display_handle, surface.id().protocol_id()).expect("native surface");
    let id = f.server.toplevels.id_for_surface(&native).expect("id");
    f.observer.toplevel_configures.clear();
    for (logical_size, resizing, fullscreen) in [
        (Extent::new(300, 200), true, false),
        (Extent::new(800, 600), true, true),
    ] {
        f.server
            .apply_client_request(weld_client::ClientRequest::Surface(
                weld_client::ClientSurfaceRequest {
                    surface: id,
                    kind: weld_client::ClientSurfaceRequestKind::Configure {
                        layout: Default::default(),
                        logical_size,
                        resizing,
                        fullscreen,
                    },
                },
            ));
    }
    f.server.flush_pending_resizes();
    f.sync();
    assert_eq!(f.observer.toplevel_configures.len(), 1);
    let (width, height, states) = &f.observer.toplevel_configures[0];
    assert_eq!((*width, *height), (800, 600));
    assert!(states.contains(&(xdg_toplevel::State::Fullscreen as u32)));
    assert!(!states.contains(&(xdg_toplevel::State::Resizing as u32)));
    f.observer.toplevel_configures.clear();
    toplevel.unset_fullscreen();
    f.sync();
    f.observer.toplevel_configures.clear();
    f.server
        .configure_toplevel(id, Extent::new(500, 400), false, false, Default::default());
    f.sync();
    let (width, height, states) = f
        .observer
        .toplevel_configures
        .last()
        .expect("restore configure");
    assert_eq!((*width, *height), (400, 300));
    assert!(!states.contains(&(xdg_toplevel::State::Fullscreen as u32)));
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn tiled_configures_override_client_limits_and_restore_them_when_floating() {
    use weld_client::{Extent, ToplevelLayout};
    let mut f = Fixture::new();
    let surface = f.surface(41);
    let xdg =
        f.observer
            .shell
            .as_ref()
            .expect("shell")
            .get_xdg_surface(&surface, &f.queue.handle(), ());
    let toplevel = xdg.get_toplevel(&f.queue.handle(), ());
    toplevel.set_min_size(400, 300);
    toplevel.set_max_size(600, 500);
    surface.commit();
    f.sync();
    let native = f.client.object_from_protocol_id::<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>(&f.server.display_handle, surface.id().protocol_id()).expect("native");
    let id = f.server.toplevels.id_for_surface(&native).expect("id");
    for (layout, requested, expected) in [
        (ToplevelLayout::Tiled, Extent::new(200, 150), (200, 150)),
        (ToplevelLayout::Floating, Extent::new(200, 150), (400, 300)),
        (ToplevelLayout::Tiled, Extent::new(800, 700), (800, 700)),
        (ToplevelLayout::Floating, Extent::new(800, 700), (600, 500)),
    ] {
        f.observer.toplevel_configures.clear();
        f.server
            .configure_toplevel(id, requested, false, false, layout);
        f.sync();
        let (width, height, states) = f.observer.toplevel_configures.last().expect("configure");
        assert_eq!((*width, *height), expected);
        for edge in [
            xdg_toplevel::State::TiledLeft,
            xdg_toplevel::State::TiledRight,
            xdg_toplevel::State::TiledTop,
            xdg_toplevel::State::TiledBottom,
        ] {
            assert_eq!(
                states.contains(&(edge as u32)),
                layout == ToplevelLayout::Tiled
            );
        }
    }
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn output_assignment_follows_attached_subtrees_and_scale_changes() {
    let mut f = Fixture::new();
    let parent = f.surface(1);
    let (id, parent_xdg) = f.toplevel(&parent);
    f.server
        .set_toplevel_outputs(id, &[OutputId::new(2)], Some(OutputId::new(2)));
    f.sync();
    f.assert_output(1, "test-2", 240);

    let child = f.surface(2);
    let grandchild = f.surface(3);
    let sub = f.observer.subsurfaces.clone().expect("subcompositor");
    sub.get_subsurface(&grandchild, &child, &f.queue.handle(), ());
    f.sync();
    f.assert_output(3, "test-1", 120);
    sub.get_subsurface(&child, &parent, &f.queue.handle(), ());
    f.sync();
    for surface in [1, 2, 3] {
        f.assert_output(surface, "test-2", 240);
    }

    let popup = f.surface(4);
    let shell = f.observer.shell.as_ref().expect("shell");
    let positioner = shell.create_positioner(&f.queue.handle(), ());
    positioner.set_size(2, 2);
    positioner.set_anchor_rect(0, 0, 2, 2);
    let popup_xdg = shell.get_xdg_surface(&popup, &f.queue.handle(), ());
    popup_xdg.get_popup(Some(&parent_xdg), &positioner, &f.queue.handle(), ());
    popup.commit();
    f.sync();
    f.assert_output(4, "test-2", 240);

    f.server.set_toplevel_preferred_scale(id, Some(180));
    f.sync();
    assert_eq!(f.observer.surfaces[&3].fractional_scale, 180);
    // No buffer/map transition is needed for a topology or preferred-scale update.
    f.server
        .set_toplevel_outputs(id, &[OutputId::new(1)], Some(OutputId::new(1)));
    f.sync();
    for surface in [1, 2, 3, 4] {
        f.assert_output(surface, "test-1", 180);
    }
    f.server.set_toplevel_preferred_scale(id, None);
    f.sync();
    for surface in [1, 2, 3, 4] {
        f.assert_output(surface, "test-1", 120);
    }
    f.server.update_output_metrics(
        OutputId::new(1),
        OutputMetrics::new(800, 600, OutputScale::new(2.5).expect("scale")).expect("metrics"),
        (0, 0),
    );
    f.sync();
    for surface in [1, 2, 3, 4] {
        f.assert_output(surface, "test-1", 300);
    }
    assert_eq!(f.observer.surfaces[&3].fractional_scale, 300);

    let buffer = f.buffer();
    parent.attach(Some(&buffer), 0, 0);
    parent.frame(&f.queue.handle(), true);
    parent.commit();
    f.sync();
    let now = Instant::now();
    assert_eq!(f.server.independent_callback_timeout(now), None);
    let claimant = ClientSourceId::new(99);
    f.server.apply_presentation_claim(
        claimant,
        ClientPresentationUpdate {
            surface: id,
            claim: ClientPresentationClaim::Active {
                rate: Some(PresentationRate::HZ_60),
            },
        },
    );
    assert_eq!(
        f.server.independent_callback_timeout(now),
        Some(Duration::ZERO)
    );
    f.server.service_independent_callbacks(now);
    f.sync();
    assert_eq!(f.observer.frames, 1);

    parent.attach(None, 0, 0);
    parent.frame(&f.queue.handle(), true);
    parent.commit();
    f.sync();
    let later = now + Duration::from_secs(1);
    // XDG remapping needs a new initial configure before attaching content.
    parent.commit();
    f.sync();
    assert_eq!(f.server.independent_callback_timeout(later), None);
    parent.attach(Some(&buffer), 0, 0);
    parent.commit();
    f.sync();
    assert_eq!(
        f.server.independent_callback_timeout(later),
        Some(Duration::ZERO)
    );
    for surface in [1, 2, 3] {
        f.assert_output(surface, "test-1", 300);
    }
    f.server.apply_presentation_claim(
        claimant,
        ClientPresentationUpdate {
            surface: id,
            claim: ClientPresentationClaim::Paused,
        },
    );
    assert_eq!(f.server.independent_callback_timeout(later), None);
    f.server.service_independent_callbacks(later);
    f.sync();
    assert_eq!(f.observer.frames, 1);
    f.server.apply_presentation_claim(
        claimant,
        ClientPresentationUpdate {
            surface: id,
            claim: ClientPresentationClaim::Release,
        },
    );
    assert!(f.server.take_local_callback_demand());
    let frame = f.server.stage_frame_callbacks();
    f.server.complete_frame_callbacks(frame);
    f.sync();
    assert_eq!(f.observer.frames, 2);
}

//! Clipboard readers discover only their relevant logical seats.

use super::*;
use smithay::wayland::selection::data_device::set_data_device_selection;
use wayland_client::protocol::{wl_callback, wl_registry, wl_seat};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1 as ext_device, ext_data_control_manager_v1 as ext_manager,
    ext_data_control_offer_v1 as ext_offer,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1 as wlr_device, zwlr_data_control_manager_v1 as wlr_manager,
    zwlr_data_control_offer_v1 as wlr_offer,
};
use weld_client::ClientInputController;

#[derive(Default)]
struct ClipboardProbe {
    seats: HashMap<u32, wl_seat::WlSeat>,
    names: HashMap<u32, String>,
    ext: Option<ext_manager::ExtDataControlManagerV1>,
    wlr: Option<wlr_manager::ZwlrDataControlManagerV1>,
    offers: HashMap<wayland_client::backend::ObjectId, Vec<String>>,
    selections: HashMap<(&'static str, u32), Option<wayland_client::backend::ObjectId>>,
}

struct Reader {
    client: Client,
    connection: Connection,
    queue: EventQueue<ClipboardProbe>,
    probe: ClipboardProbe,
}

impl Reader {
    fn new(f: &mut Fixture, id: u64) -> Self {
        let (server, client) = UnixStream::pair().expect("sockets");
        let client_handle = f
            .server
            .display_handle
            .insert_client(
                server,
                Arc::new(ClientState::new(ClientId::new(
                    crate::WAYLAND_CLIENT_SOURCE,
                    id,
                ))),
            )
            .expect("client");
        let connection = Connection::from_socket(client).expect("connection");
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut reader = Self {
            client: client_handle,
            connection,
            queue,
            probe: ClipboardProbe::default(),
        };
        reader.sync(f);
        reader.sync(f);
        reader
    }

    fn sync(&mut self, f: &mut Fixture) {
        self.connection.display().sync(&self.queue.handle(), ());
        self.connection.flush().expect("flush");
        f.sync();
        self.queue
            .prepare_read()
            .expect("read guard")
            .read()
            .expect("read");
        self.queue
            .dispatch_pending(&mut self.probe)
            .expect("dispatch");
    }

    fn read_selections(&mut self, f: &mut Fixture) {
        for (&id, seat) in &self.probe.seats {
            self.probe
                .ext
                .as_ref()
                .expect("ext manager")
                .get_data_device(seat, &self.queue.handle(), id);
            self.probe
                .wlr
                .as_ref()
                .expect("wlr manager")
                .get_data_device(seat, &self.queue.handle(), id);
        }
        self.sync(f);
    }

    fn assert_selection(&self, name: &str, expected: &[&str]) {
        let id = *self
            .probe
            .names
            .iter()
            .find(|(_, value)| *value == name)
            .expect("seat name")
            .0;
        for protocol in ["ext", "wlr"] {
            let selection = self
                .probe
                .selections
                .get(&(protocol, id))
                .expect("selection event");
            let mimes = selection
                .as_ref()
                .and_then(|offer| self.probe.offers.get(offer))
                .map(Vec::as_slice)
                .unwrap_or_default();
            assert_eq!(mimes, expected, "{protocol} selection on {name}");
        }
    }
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn clipboard_readers_keep_desktop_selection_while_remote_seats_are_private() {
    let mut f = Fixture::new();
    let surface = f.surface(2001);
    let (window, _) = f.toplevel(&surface);
    let peer = ClientInputController {
        adapter: ClientSourceId::new(9),
        connection: 1,
    };
    let remote = f.server.remote_input(peer).expect("peer");
    set_data_device_selection(
        &f.server.display_handle,
        &f.server.local_input.native,
        vec!["image/png".into()],
        (),
    );
    set_data_device_selection(
        &f.server.display_handle,
        &remote.native,
        vec!["text/remote".into()],
        (),
    );
    let mut desktop = Reader::new(&mut f, 20);
    f.sync();
    assert_eq!(
        f.observer.selection.seats.len(),
        1,
        "unassigned peer stays internal"
    );
    f.server.route_controller_focus(Some(peer), Some(window));
    for _ in 0..3 {
        f.sync();
    }
    assert_eq!(
        f.observer.selection.seats.len(),
        2,
        "target can bind its native peer seat"
    );
    assert_eq!(remote.publications.borrow().len(), 1);
    f.server.route_controller_focus(Some(peer), Some(window));
    assert_eq!(
        remote.publications.borrow().len(),
        1,
        "publication is idempotent"
    );

    // Rust clipboard readers select an arbitrary advertised seat. Every fresh
    // desktop connection must have exactly one choice, including after routing.
    for id in 21..29 {
        let mut reader = Reader::new(&mut f, id);
        reader.read_selections(&mut f);
        assert_eq!(reader.probe.seats.len(), 1);
        reader.assert_selection("output-test", &["image/png"]);
    }
    desktop.sync(&mut f);
    desktop.read_selections(&mut f);
    assert_eq!(
        desktop.probe.seats.len(),
        1,
        "existing reader also stays desktop-only"
    );
    desktop.assert_selection("output-test", &["image/png"]);

    // An explicitly scoped publication still exposes the named controller's
    // own selection to both clipboard-manager protocols.
    let mut remote_reader = Reader::new(&mut f, 30);
    f.server.advertise_remote_input(peer, &remote_reader.client);
    remote_reader.sync(&mut f);
    remote_reader.sync(&mut f);
    remote_reader.read_selections(&mut f);
    remote_reader.assert_selection("weld-peer-9-1", &["text/remote"]);
    remote_reader.assert_selection("output-test", &["image/png"]);
    drop(remote_reader);
    f.sync();
    f.server.cleanup_input_bindings();
    assert_eq!(
        remote.publications.borrow().len(),
        1,
        "dead client publication retired"
    );

    f.server.retire_remote_seat(peer);
    assert!(remote.publications.borrow().is_empty());
    desktop.sync(&mut f);
    desktop.assert_selection("output-test", &["image/png"]);
}

delegate_noop!(ClipboardProbe: ignore wl_callback::WlCallback);
delegate_noop!(ClipboardProbe: ignore ext_manager::ExtDataControlManagerV1);
delegate_noop!(ClipboardProbe: ignore wlr_manager::ZwlrDataControlManagerV1);

impl Dispatch<wl_registry::WlRegistry, ()> for ClipboardProbe {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_seat" => {
                    state
                        .seats
                        .insert(name, registry.bind(name, version.min(10), qh, name));
                }
                "ext_data_control_manager_v1" => state.ext = Some(registry.bind(name, 1, qh, ())),
                "zwlr_data_control_manager_v1" => {
                    state.wlr = Some(registry.bind(name, version.min(2), qh, ()))
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                state.seats.remove(&name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, u32> for ClipboardProbe {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Name { name } = event {
            state.names.insert(*id, name);
        }
    }
}

macro_rules! clipboard_protocol {
    ($device:ident, $device_type:ident, $offer:ident, $offer_type:ident, $name:literal) => {
        impl Dispatch<$device::$device_type, u32> for ClipboardProbe {
            fn event(state: &mut Self, _: &$device::$device_type, event: $device::Event, seat: &u32, _: &Connection, _: &QueueHandle<Self>) {
                if let $device::Event::Selection { id } = event {
                    state.selections.insert(($name, *seat), id.map(|offer| offer.id()));
                }
            }
            wayland_client::event_created_child!(ClipboardProbe, $device::$device_type, [0 => ($offer::$offer_type, ())]);
        }
        impl Dispatch<$offer::$offer_type, ()> for ClipboardProbe {
            fn event(state: &mut Self, offer: &$offer::$offer_type, event: $offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
                if let $offer::Event::Offer { mime_type } = event {
                    state.offers.entry(offer.id()).or_default().push(mime_type);
                }
            }
        }
    };
}
clipboard_protocol!(
    ext_device,
    ExtDataControlDeviceV1,
    ext_offer,
    ExtDataControlOfferV1,
    "ext"
);
clipboard_protocol!(
    wlr_device,
    ZwlrDataControlDeviceV1,
    wlr_offer,
    ZwlrDataControlOfferV1,
    "wlr"
);

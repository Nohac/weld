use crate::platform::{Bridge, Requests};
use crate::{
    catalogue::{self, Grouping},
    session::{ConnectionRequest as Command, Session, Snapshot},
};
use anyhow::Result;
use dioxus::prelude::*;
use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc, time::Duration};

#[css_module("/assets/shell.css")]
struct Styles;

#[derive(Clone, Routable, PartialEq)]
enum Route {
    #[layout(Root)]
    #[layout(Shell)]
    #[route("/")]
    Hosts {},
    #[route("/pair")]
    Pair {},
    #[route("/host/:id")]
    Applications { id: String },
    #[end_layout]
    #[route("/host/:id/window/:window")]
    Stream { id: String, window: u64 },
}

#[derive(Clone)]
struct Client {
    session: Arc<Session>,
    snapshot: Signal<Snapshot>,
    error: Signal<Option<String>>,
    device_name: String,
    native: Rc<RefCell<Requests>>,
    insets: Signal<[f64; 4]>,
}
impl Client {
    fn send(&mut self, command: impl Into<crate::session::Command>) -> bool {
        match self.session.send(command) {
            Ok(()) => {
                self.error.set(None);
                true
            }
            Err(error) => {
                self.error.set(Some(error.to_string()));
                false
            }
        }
    }
}
pub fn launch(directory: PathBuf, device_name: String) -> Result<()> {
    let session = Arc::new(Session::start(directory)?);
    let config = dioxus_native::Config::new().with_window_attributes(
        dioxus_native::WindowAttributes::default()
            .with_title("Weld Connect")
            .with_surface_size(dioxus_native::LogicalSize::new(960.0, 720.0)),
    );
    dioxus_native::launch_cfg_with_props(
        app,
        AppProps {
            session,
            device_name,
        },
        vec![],
        vec![Box::new(config)],
    );
    Ok(())
}
#[derive(Props, Clone)]
struct AppProps {
    session: Arc<Session>,
    device_name: String,
}
impl PartialEq for AppProps {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.session, &other.session) && self.device_name == other.device_name
    }
}
fn app(props: AppProps) -> Element {
    let mut snapshot = use_signal(Snapshot::default);
    let error = use_signal(|| None);
    let native = use_hook(|| Rc::new(RefCell::new(Requests::default())));
    let insets = use_signal(|| [0.0; 4]);
    use_context_provider(|| Client {
        session: props.session.clone(),
        snapshot,
        error,
        device_name: props.device_name.clone(),
        native,
        insets,
    });
    use_future(move || {
        let session = props.session.clone();
        async move {
            loop {
                if let Ok(view) = session.snapshot.lock()
                    && *snapshot.peek() != *view
                {
                    snapshot.set(view.clone());
                }
                futures_timer::Delay::new(Duration::from_millis(100)).await;
            }
        }
    });
    rsx! { Router::<Route> {} }
}
#[component]
fn Shell() -> Element {
    let client = use_context::<Client>();
    let navigator = use_navigator();
    dioxus_native::use_back_button({
        let mut client = client.clone();
        move || {
            client.send(Command::Disconnect);
            navigator.replace(Route::Hosts {});
        }
    });
    rsx! {
        div { class: Styles::shell, style: "padding: {client.insets.read()[1]}px {client.insets.read()[2]}px {client.insets.read()[3]}px {client.insets.read()[0]}px;",
            header { class: Styles::header,
                div { class: Styles::brand, "weld" }
                div { class: Styles::subtitle, "Your applications, wherever you are." }
            }
            main { class: Styles::content, div { class: Styles::page, Outlet::<Route> {} } }
            footer { class: Styles::status,
                if let Some(error) = (client.error)() { "{error}" } else { "{client.snapshot.read().status}" }
            }
        }
    }
}
#[component]
fn Hosts() -> Element {
    let client = use_context::<Client>();
    let navigator = use_navigator();
    let view = (client.snapshot)();
    rsx! {
        h1 { class: Styles::heading, "Your hosts" }
        p { class: Styles::description, "Connect to a computer to browse its running applications." }
        div { class: Styles::toolbar,
            button { class: "{Styles::button} {Styles::primary}", onclick: move |_| { navigator.push(Route::Pair {}); }, "Pair a device" }
        }
        if view.hosts.is_empty() {
            div { class: Styles::empty, "Bring your first computer into Weld Connect. Create a pairing invitation with weldctl on that computer, then add it here." }
        }
        div { class: Styles::cards,
            for host in view.hosts {
                if let Ok(id) = host.id() {
                    div { key: "{id}", class: Styles::card,
                        div {
                            p { class: Styles::label, "{host.name}" }
                            div { class: Styles::detail, "Paired host" }
                        }
                        button { class: Styles::button, onclick: {
                            let mut client = client.clone();
                            move |_| { if client.send(Command::Connect(id.clone())) { navigator.push(Route::Applications { id: id.clone() }); } }
                        }, "Open" }
                    }
                }
            }
        }
    }
}
#[component]
fn Pair() -> Element {
    let mut client = use_context::<Client>();
    let navigator = use_navigator();
    let mut link = use_signal(String::new);
    let mut name = use_signal(|| client.device_name.clone());
    let view = (client.snapshot)();
    rsx! {
        h1 { class: Styles::heading, "Pair a device" }
        if Bridge::AVAILABLE {
            div { class: Styles::toolbar,
                button { class: "{Styles::button} {Styles::primary}", onclick: { let native = client.native.clone(); move |_| native.borrow_mut().scan = true }, "Scan pairing QR" }
                button { class: Styles::button, onclick: { let native = client.native.clone(); move |_| native.borrow_mut().paste = true }, "Paste pairing link" }
            }
        }
        p { class: Styles::description, "Create an invitation on your computer with weldctl pair. Compare the verification code on both devices before approving." }
        label { r#for: "device-name", "This device's name" }
        input { id: "device-name", class: Styles::input, value: "{name}", maxlength: 128, oninput: move |event| name.set(event.value()) }
        label { r#for: "pairing-link", "Pairing link" }
        input { id: "pairing-link", class: Styles::input, value: "{link}", placeholder: "weld://pair/…", maxlength: 4096, oninput: move |event| link.set(event.value()) }
        if let Some((host, code)) = view.verification {
            div { class: Styles::empty,
                p { "Verify this code on {host}" }
                div { class: Styles::code, "{code}" }
                p { "Approve the matching request on your computer." }
            }
        }
        div { class: Styles::toolbar,
            button { class: "{Styles::button} {Styles::primary}", disabled: view.pairing || link.read().is_empty(), onclick: {
                let mut client = client.clone();
                move |_| { let parsed = link.read().trim().parse(); match parsed {
                    Ok(invitation) => { if client.send(Command::Pair { invitation, name: name.read().trim().to_owned() }) { link.set(String::new()); } }
                    Err(_) => client.error.set(Some("Enter a valid Weld pairing link".into())),
                } }
            }, "Pair" }
            button { class: Styles::button, onclick: move |_| { client.send(Command::Disconnect); navigator.replace(Route::Hosts {}); }, "Back to hosts" }
        }
    }
}
#[component]
fn Applications(id: String) -> Element {
    let mut client = use_context::<Client>();
    let navigator = use_navigator();
    let mut grouping = use_signal(Grouping::default);
    let view = (client.snapshot)();
    let name = view
        .hosts
        .iter()
        .find(|host| host.id().is_ok_and(|value| value == id))
        .map_or("Host", |host| host.name.as_str());
    rsx! {
        h1 { class: Styles::heading, "{name}" }
        p { class: Styles::description, "Running applications" }
        div { class: Styles::toolbar,
            button { class: Styles::button, onclick: move |_| { client.send(Command::Disconnect); navigator.replace(Route::Hosts {}); }, "Hosts" }
            button { class: Styles::button, aria_pressed: grouping() == Grouping::Windows, onclick: move |_| grouping.set(Grouping::Windows), "Windows" }
            button { class: Styles::button, aria_pressed: grouping() == Grouping::Application, onclick: move |_| grouping.set(Grouping::Application), "By application" }
        }
        if view.connected.as_deref() != Some(id.as_str()) {
            div { class: Styles::empty, "{view.status}" }
        } else if view.applications.is_empty() {
            div { class: Styles::empty, "No running applications are available on this host." }
        } else {
            for group in catalogue::groups(&view.applications, grouping()) {
                h2 { class: Styles::group_title, "{group.label}" }
                div { class: Styles::cards,
                    for application in group.windows {
                        div { key: "{application.window}", class: Styles::card,
                            div {
                                p { class: Styles::label, "{application.title}" }
                                div { class: Styles::detail, "{application.app_id}" }
                            }
                            button { class: Styles::button, disabled: !application.available && !application.hoisted_here, onclick: {
                                let mut client = client.clone();
                                let id = id.clone();
                                let window = application.window;
                                move |_| {
                                    if client.send(crate::session::Command::Hoist { identity: id.clone(), window }) { navigator.push(Route::Stream { id: id.clone(), window }); }
                                }
                            }, "Open" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn Stream(id: String, window: u64) -> Element {
    let client = use_context::<Client>();
    let navigator = use_navigator();
    let native_window = dioxus_native::use_window();
    dioxus_native::use_window_event({
        let session = client.session.clone();
        move |event, _| {
            use dioxus_native::winit::event::WindowEvent;
            if let WindowEvent::Focused(focused) = event {
                session
                    .media
                    .active
                    .store(*focused, std::sync::atomic::Ordering::Release);
                session
                    .media
                    .reset
                    .store(true, std::sync::atomic::Ordering::Release);
                session.wake();
            }
        }
    });
    let widget = use_hook({
        let session = client.session.clone();
        move || {
            session
                .media
                .attach(Arc::new(move || native_window.request_redraw()));
            dioxus_native::CustomWidgetAttr::new(crate::video::Video::new(session))
        }
    });
    use_drop({
        let session = client.session.clone();
        let id = id.clone();
        move || {
            let _ = session.send(crate::session::Command::Release {
                identity: id,
                window,
            });
        }
    });
    dioxus_native::use_back_button({
        let id = id.clone();
        move || {
            navigator.replace(Route::Applications { id: id.clone() });
        }
    });
    rsx! {
        div { class: Styles::stream,
            object { class: Styles::video, "data": widget }
            if client.snapshot.read().connected.as_deref() != Some(id.as_str()) {
                div { class: Styles::stream_status, "{client.snapshot.read().status}" }
            }
            if !Bridge::AVAILABLE {
                button { class: Styles::stream_back, onclick: move |_| { navigator.replace(Route::Applications { id: id.clone() }); }, "Back" }
            }
        }
    }
}

#[component]
fn Root() -> Element {
    let client = use_context::<Client>();
    let navigator = use_navigator();
    let router = router();
    let window = dioxus_native::use_window();
    use_future(move || {
        let mut client = client.clone();
        let window = window.clone();
        async move {
            let mut bridge = Bridge::default();
            loop {
                let route = router.current::<Route>();
                let mut requests = client.native.borrow().clone();
                requests.streaming = matches!(route, Route::Stream { .. });
                if let Some(update) = bridge.poll(&mut requests) {
                    let insets = update
                        .insets
                        .map(|value| f64::from(value) / window.scale_factor());
                    if *client.insets.peek() != insets {
                        client.insets.set(insets);
                    }
                    if let Some(link) = update.link {
                        match link.parse() {
                            Ok(invitation) => {
                                if client.send(Command::Pair {
                                    invitation,
                                    name: update.name,
                                }) {
                                    navigator.replace(Route::Pair {});
                                }
                            }
                            Err(_) => client
                                .error
                                .set(Some("Not a valid Weld pairing link".into())),
                        }
                    } else if update.back {
                        match route {
                            Route::Stream { id, .. } => {
                                navigator.replace(Route::Applications { id });
                            }
                            Route::Hosts {} => requests.background = true,
                            _ => {
                                client.send(Command::Disconnect);
                                navigator.replace(Route::Hosts {});
                            }
                        }
                    }
                }
                // Keep native one-shot requests outside the reactive render dependency graph.
                *client.native.borrow_mut() = requests;
                futures_timer::Delay::new(Duration::from_millis(100)).await;
            }
        }
    });
    rsx! { Outlet::<Route> {} }
}

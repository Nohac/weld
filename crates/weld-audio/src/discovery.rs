//! Resolve stream identities through node properties and their owning clients.
use super::{
    CaptureState, CaptureStream, CaptureTarget, Consumer, Failure, belongs_to, monotonic_ns,
};
use anyhow::Result;
use pipewire::{
    core::CoreRc,
    proxy::{Listener as ProxyListener, ProxyT},
    registry::{Listener, RegistryRc},
    spa::utils::dict::DictRef,
    types::ObjectType,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, Mutex, Weak},
    time::Instant,
};

pub(super) struct Discovery {
    _listener: Listener,
    _registry: RegistryRc,
    _watches: Rc<RefCell<HashMap<u32, Watch>>>,
    _state: Rc<RefCell<State>>,
}
struct Watch {
    _listener: Box<dyn ProxyListener>,
    _proxy: Box<dyn ProxyT>,
}
#[derive(Default)]
struct NodeIdentity {
    serial: u64,
    name: String,
    class: String,
    process: Option<u32>,
    client: Option<u32>,
}
struct State {
    core: CoreRc,
    target: CaptureTarget,
    origin: Instant,
    clock_origin_ns: i128,
    consumers: Arc<Mutex<Vec<Weak<Consumer>>>>,
    failure: Failure,
    nodes: HashMap<u32, NodeIdentity>,
    clients: HashMap<u32, u32>,
    captures: HashMap<u32, CaptureStream>,
}
impl Discovery {
    pub fn new(
        core: &CoreRc,
        target: CaptureTarget,
        origin: Instant,
        consumers: Arc<Mutex<Vec<Weak<Consumer>>>>,
        failure: Failure,
    ) -> Result<Self> {
        let registry = core.get_registry_rc()?;
        let state = Rc::new(RefCell::new(State {
            core: core.clone(),
            target,
            origin,
            clock_origin_ns: monotonic_ns() - origin.elapsed().as_nanos() as i128,
            consumers,
            failure,
            nodes: HashMap::new(),
            clients: HashMap::new(),
            captures: HashMap::new(),
        }));
        let watches = Rc::new(RefCell::new(HashMap::new()));
        let bind = registry.clone();
        let added = watches.clone();
        let removed = watches.clone();
        let observe = state.clone();
        let retire = state.clone();
        let listener = registry
            .add_listener_local()
            .global(move |global| {
                if added.borrow().len() >= 1024 {
                    return;
                }
                let id = global.id;
                let observed = observe.clone();
                let result = (|| -> Result<Option<Watch>> {
                    match global.type_ {
                        ObjectType::Client => {
                            let client = bind.bind::<pipewire::client::Client, _>(global)?;
                            let listener = client
                                .add_listener_local()
                                .info(move |info| {
                                    if let Some(props) = info.props() {
                                        let mut state = observed.borrow_mut();
                                        if let Some(pid) = process_id(props) {
                                            state.clients.insert(id, pid);
                                        }
                                        state.reconcile();
                                    }
                                })
                                .register();
                            Ok(Some(Watch {
                                _listener: Box::new(listener),
                                _proxy: Box::new(client),
                            }))
                        }
                        ObjectType::Node => {
                            let Some(props) = global.props else {
                                return Ok(None);
                            };
                            if !matches!(
                                props.get("media.class"),
                                Some("Stream/Output/Audio" | "Audio/Sink")
                            ) {
                                return Ok(None);
                            }
                            observed.borrow_mut().update_node(id, props);
                            let node = bind.bind::<pipewire::node::Node, _>(global)?;
                            let listener = node
                                .add_listener_local()
                                .info(move |info| {
                                    if let Some(props) = info.props() {
                                        observed.borrow_mut().update_node(id, props);
                                    }
                                })
                                .register();
                            Ok(Some(Watch {
                                _listener: Box::new(listener),
                                _proxy: Box::new(node),
                            }))
                        }
                        _ => Ok(None),
                    }
                })();
                match result {
                    Ok(Some(watch)) => {
                        added.borrow_mut().insert(id, watch);
                    }
                    Ok(None) => {}
                    Err(error) => observe.borrow().failure.set(error),
                }
            })
            .global_remove(move |id| {
                removed.borrow_mut().remove(&id);
                let mut state = retire.borrow_mut();
                state.nodes.remove(&id);
                state.clients.remove(&id);
                state.captures.remove(&id);
            })
            .register();
        Ok(Self {
            _listener: listener,
            _registry: registry,
            _watches: watches,
            _state: state,
        })
    }
}
impl State {
    fn update_node(&mut self, id: u32, props: &DictRef) {
        let node = self.nodes.entry(id).or_default();
        if let Some(serial) = props
            .get("object.serial")
            .and_then(|value| value.parse().ok())
        {
            node.serial = serial;
        }
        if let Some(name) = props.get("node.name") {
            node.name = name.into();
        }
        if let Some(class) = props.get("media.class") {
            node.class = class.into();
        }
        node.process = process_id(props).or(node.process);
        node.client = props
            .get("client.id")
            .and_then(|value| value.parse().ok())
            .or(node.client);
        self.reconcile();
    }
    fn reconcile(&mut self) {
        for (id, node) in &self.nodes {
            let selected = match &self.target {
                CaptureTarget::Output(name) => node.class == "Audio/Sink" && node.name == *name,
                CaptureTarget::Process(root) => {
                    node.class == "Stream/Output/Audio"
                        && node
                            .process
                            .or_else(|| self.clients.get(&node.client?).copied())
                            .is_some_and(|pid| belongs_to(pid, *root))
                }
            };
            if !selected
                || node.serial == 0
                || self.captures.contains_key(id)
                || self.captures.len() >= 16
            {
                continue;
            }
            match CaptureStream::new(
                &self.core,
                CaptureState {
                    serial: node.serial,
                    origin: self.origin,
                    clock_origin_ns: self.clock_origin_ns,
                    consumers: self.consumers.clone(),
                    failure: self.failure.clone(),
                },
                matches!(self.target, CaptureTarget::Output(_)),
            ) {
                Ok(stream) => {
                    self.captures.insert(*id, stream);
                }
                Err(error) => self.failure.set(error),
            }
        }
    }
}
fn process_id(props: &DictRef) -> Option<u32> {
    props
        .get("application.process.id")
        .or_else(|| props.get("pipewire.sec.pid"))
        .and_then(|value| value.parse().ok())
}

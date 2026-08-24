//! Caller-driven adapter registry and device-paced input routing.

use std::{
    any::Any,
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
};

use crate::{
    ButtonState, ClientEventQueue, ClientInputEvent, ClientInputTarget, ClientKeyboardRoute,
    ClientPointerRoute, ClientPointerRouteUpdate, ClientRequest, ClientSourceDescriptor,
    ClientSourceId, ClientSurfaceId, InputEventKind, LinuxButtonCode, PointerGesture,
    PointerGestureKind, RawScrollFrame, RawScrollPhase, RawScrollSource, RuntimeInputEvent,
    RuntimeInputEventKind,
};

/// One client source driven by the native runtime.
///
/// Implementations are deliberately caller-driven. They must not start their
/// own application loop or assume an async runtime.
pub trait ClientAdapter {
    fn drain_events(&mut self, events: &mut ClientEventQueue);
    fn apply_request(&mut self, request: ClientRequest);
    fn apply_input(&mut self, event: ClientInputEvent);
    fn apply_command(&mut self, command: ClientAdapterCommandEnvelope);
    fn host_focus_lost(&mut self, time: u32);

    /// Observes one validated event from another local-provenance adapter.
    /// Relocated events are not observed recursively.
    fn observe_event(&mut self, _event: &crate::ClientSurfaceEvent) {}

    /// Publishes input/request route aliases created by observed events.
    fn drain_route_alias_updates(&mut self, _updates: &mut Vec<ClientRouteAliasUpdate>) {}

    /// Publishes already-addressed work received from an external adapter.
    fn drain_effects(&mut self, _effects: &mut Vec<ClientAdapterEffect>) {}

    /// Publishes adapter-owned allocations that can no longer be reused.
    fn drain_retired_buffers(&mut self, _buffers: &mut Vec<crate::ClientBufferId>) {}

    /// Observes retirement from another local-provenance adapter.
    fn observe_retired_buffer(&mut self, _buffer: crate::ClientBufferId) {}
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClientAdapterEffect {
    Request(ClientRequest),
    Input(ClientInputEvent),
}

impl ClientAdapterEffect {
    pub const fn target_source(&self) -> Option<ClientSourceId> {
        match self {
            Self::Request(request) => request.source(),
            Self::Input(event) => Some(event.target.surface().source()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientRuntimeEffectError {
    pub adapter: ClientSourceId,
    pub target: Option<ClientSourceId>,
}

impl fmt::Display for ClientRuntimeEffectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "client adapter {} targeted unavailable source {:?}",
            self.adapter.raw(),
            self.target.map(ClientSourceId::raw)
        )
    }
}

impl std::error::Error for ClientRuntimeEffectError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientRouteAliasUpdate {
    pub destination: ClientSurfaceId,
    pub source: Option<ClientSurfaceId>,
}

/// Source-addressed local command whose payload is understood only by its adapter.
///
/// This is a process-local extension seam, not a serialized protocol message.
pub struct ClientAdapterCommandEnvelope {
    source: ClientSourceId,
    payload: Box<dyn Any + Send + Sync>,
}

impl ClientAdapterCommandEnvelope {
    pub fn new<T>(source: ClientSourceId, payload: T) -> Self
    where
        T: Any + Send + Sync,
    {
        Self {
            source,
            payload: Box::new(payload),
        }
    }

    pub const fn source(&self) -> ClientSourceId {
        self.source
    }

    pub fn downcast<T: Any + Send + Sync>(self) -> Result<Box<T>, Self> {
        match self.payload.downcast() {
            Ok(payload) => Ok(payload),
            Err(payload) => Err(Self {
                source: self.source,
                payload,
            }),
        }
    }
}

impl fmt::Debug for ClientAdapterCommandEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientAdapterCommandEnvelope")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// Runtime-owned half of an adapter registration.
pub struct ClientRuntimeAdapter {
    pub descriptor: ClientSourceDescriptor,
    pub driver: Box<dyn ClientAdapter>,
}

impl ClientRuntimeAdapter {
    pub fn new(descriptor: ClientSourceDescriptor, driver: impl ClientAdapter + 'static) -> Self {
        Self {
            descriptor,
            driver: Box::new(driver),
        }
    }
}

/// Pre-run registration split between the native runtime and application ingress.
pub struct ClientAdapterRegistration {
    runtime: ClientRuntimeAdapter,
    importer: Box<dyn Any>,
}

impl ClientAdapterRegistration {
    pub fn new<I>(
        descriptor: ClientSourceDescriptor,
        driver: impl ClientAdapter + 'static,
        importer: I,
    ) -> Self
    where
        I: Any,
    {
        Self {
            runtime: ClientRuntimeAdapter::new(descriptor, driver),
            importer: Box::new(importer),
        }
    }

    pub fn into_parts(self) -> ClientAdapterRegistrationParts {
        let descriptor = self.runtime.descriptor;
        ClientAdapterRegistrationParts {
            runtime: self.runtime,
            importer: ClientImporterRegistration {
                descriptor,
                importer: self.importer,
            },
        }
    }
}

pub struct ClientAdapterRegistrationParts {
    pub runtime: ClientRuntimeAdapter,
    pub importer: ClientImporterRegistration,
}

pub struct ClientImporterRegistration {
    pub descriptor: ClientSourceDescriptor,
    pub importer: Box<dyn Any>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientRuntimeRegistrationError {
    pub source: ClientSourceId,
}

impl fmt::Display for ClientRuntimeRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "client source {} is already registered",
            self.source.raw()
        )
    }
}

impl std::error::Error for ClientRuntimeRegistrationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientRuntimeEventError {
    pub registered_source: ClientSourceId,
    pub event_surface: ClientSurfaceId,
}

impl fmt::Display for ClientRuntimeEventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "client source {} published an event for source {}",
            self.registered_source.raw(),
            self.event_surface.source().raw()
        )
    }
}

impl std::error::Error for ClientRuntimeEventError {}

#[derive(Default)]
enum PointerCapture {
    #[default]
    Idle,
    Active {
        route: Option<ClientPointerRoute>,
    },
}

#[derive(Clone, Copy)]
struct GestureCapture {
    kind: PointerGestureKind,
    route: Option<ClientPointerRoute>,
}

#[derive(Clone, Copy)]
struct FingerScrollCapture {
    route: Option<ClientPointerRoute>,
    horizontal_active: bool,
    vertical_active: bool,
}

impl FingerScrollCapture {
    fn new(route: Option<ClientPointerRoute>, axis: RawScrollFrame) -> Self {
        let mut capture = Self {
            route,
            horizontal_active: false,
            vertical_active: false,
        };
        capture.observe(axis);
        capture
    }

    fn observe(&mut self, axis: RawScrollFrame) {
        self.horizontal_active |= axis.horizontal != 0.0;
        self.vertical_active |= axis.vertical != 0.0;
        if axis.horizontal_stop {
            self.horizontal_active = false;
        }
        if axis.vertical_stop {
            self.vertical_active = false;
        }
    }
}

/// Result of routing one unconsumed event through the client runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientInputDispatchResult {
    Delivered,
    NoRoute,
    UnknownAdapter,
    AliasCycle,
    UnsupportedButton,
}

impl ClientInputDispatchResult {
    pub const fn is_delivered(self) -> bool {
        matches!(self, Self::Delivered)
    }
}

const BUTTON_WORDS: usize = 12;

#[derive(Default)]
struct PressedButtons([u64; BUTTON_WORDS]);

impl PressedButtons {
    fn supports(button: LinuxButtonCode) -> bool {
        button.0 as usize / (u64::BITS as usize) < BUTTON_WORDS
    }

    fn insert(&mut self, button: LinuxButtonCode) {
        let bit = button.0 as usize;
        if let Some(word) = self.0.get_mut(bit / u64::BITS as usize) {
            *word |= 1_u64 << (bit % u64::BITS as usize);
        }
    }

    fn remove(&mut self, button: LinuxButtonCode) {
        let bit = button.0 as usize;
        if let Some(word) = self.0.get_mut(bit / u64::BITS as usize) {
            *word &= !(1_u64 << (bit % u64::BITS as usize));
        }
    }

    fn is_empty(&self) -> bool {
        self.0.iter().all(|word| *word == 0)
    }

    fn clear(&mut self) {
        self.0.fill(0);
    }
}

/// Registry above all client adapters in one native backend loop.
///
/// [`Self::dispatch_unconsumed_input`] must be called only after application
/// shortcut and virtual-terminal filters have declined to consume an event.
/// The dispatch step itself does not access Bevy or enqueue input for a later
/// application update.
#[derive(Default)]
pub struct ClientRuntime {
    adapters: BTreeMap<ClientSourceId, ClientRuntimeAdapter>,
    scratch_events: ClientEventQueue,
    generated_events: ClientEventQueue,
    scratch_destroyed: Vec<ClientSurfaceId>,
    scratch_alias_updates: Vec<ClientRouteAliasUpdate>,
    sourced_alias_updates: Vec<(ClientSourceId, ClientRouteAliasUpdate)>,
    observed_adapters: Vec<ClientSourceId>,
    scratch_effects: Vec<ClientAdapterEffect>,
    pending_effects: Vec<(ClientSourceId, ClientAdapterEffect)>,
    scratch_retired_buffers: Vec<crate::ClientBufferId>,
    retired_buffers: Vec<crate::ClientBufferId>,
    aliases: HashMap<ClientSurfaceId, ClientSurfaceId>,
    retired_aliases: HashSet<ClientSurfaceId>,
    pointer_route: Option<ClientPointerRoute>,
    pending_pointer_route: Option<Option<ClientPointerRoute>>,
    keyboard_route: Option<ClientKeyboardRoute>,
    pointer_capture: PointerCapture,
    gesture_capture: Option<GestureCapture>,
    finger_scroll_capture: Option<FingerScrollCapture>,
    pressed_buttons: PressedButtons,
    pointer_position: Option<crate::InputPosition>,
}

impl ClientRuntime {
    pub fn register(
        &mut self,
        adapter: ClientRuntimeAdapter,
    ) -> Result<(), ClientRuntimeRegistrationError> {
        let source = adapter.descriptor.id;
        if self.adapters.contains_key(&source) {
            return Err(ClientRuntimeRegistrationError { source });
        }
        self.adapters.insert(source, adapter);
        Ok(())
    }

    pub fn descriptor(&self, source: ClientSourceId) -> Option<ClientSourceDescriptor> {
        self.adapters.get(&source).map(|adapter| adapter.descriptor)
    }

    pub fn set_route_alias(&mut self, destination: ClientSurfaceId, source: ClientSurfaceId) {
        if destination != source {
            self.aliases.insert(destination, source);
            self.retired_aliases.remove(&destination);
        }
    }

    pub fn remove_route_alias(&mut self, destination: ClientSurfaceId) {
        if self.aliases.remove(&destination).is_some() {
            self.retired_aliases.insert(destination);
        }
    }

    pub fn set_pointer_route(&mut self, route: Option<ClientPointerRoute>) {
        if matches!(self.pointer_capture, PointerCapture::Active { .. }) {
            self.pending_pointer_route = Some(route);
        } else {
            self.pointer_route = route;
        }
    }

    /// Publishes application pointer policy and settles adapter focus immediately.
    pub fn publish_pointer_route(
        &mut self,
        update: ClientPointerRouteUpdate,
    ) -> ClientInputDispatchResult {
        let ClientPointerRouteUpdate {
            route,
            position,
            time,
        } = update;
        match route {
            Some(route) => {
                let capturing = matches!(self.pointer_capture, PointerCapture::Active { .. });
                let previous = match self.captured_or_current_pointer_route() {
                    Ok(route) => route,
                    Err(error) => return error,
                };
                let next = match self.resolved_pointer_route(Some(route)) {
                    Ok(route) => route,
                    Err(error) => return error,
                };
                if !capturing
                    && previous.zip(next).is_some_and(|(previous, next)| {
                        previous.surface.source() != next.surface.source()
                    })
                {
                    let _ = self.dispatch_pointer_to(
                        previous,
                        InputEventKind::PointerLeft { position },
                        Some(position),
                        time,
                    );
                }
                self.set_pointer_route(Some(route));
                self.dispatch_unconsumed_input(RuntimeInputEvent::new(
                    RuntimeInputEventKind::Input(InputEventKind::PointerMotion { position }),
                    time,
                ))
            }
            None => self.dispatch_unconsumed_input(RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(InputEventKind::PointerLeft { position }),
                time,
            )),
        }
    }

    pub fn set_keyboard_route(&mut self, route: Option<ClientKeyboardRoute>) {
        self.keyboard_route = route;
    }

    pub fn drain_events(
        &mut self,
        events: &mut ClientEventQueue,
        invalid: &mut Vec<ClientRuntimeEventError>,
    ) {
        // Retirement rejects aliases removed during this drain. Destroyed-event
        // route cleanup below is the durable protection after the next drain.
        self.retired_aliases.clear();
        for (source, adapter) in &mut self.adapters {
            adapter.driver.drain_events(&mut self.scratch_events);
            while let Some(event) = self.scratch_events.pop_front() {
                if event.surface.source() == *source {
                    self.generated_events.push(event);
                } else {
                    invalid.push(ClientRuntimeEventError {
                        registered_source: *source,
                        event_surface: event.surface,
                    });
                }
            }
        }
        while let Some(event) = self.generated_events.pop_front() {
            let event_source = event.surface.source();
            let relayable = self.adapters.get(&event_source).is_some_and(|adapter| {
                adapter.descriptor.provenance == crate::ClientProvenance::Local
            });
            if relayable {
                for (source, adapter) in &mut self.adapters {
                    if *source != event_source {
                        adapter.driver.observe_event(&event);
                        if !self.observed_adapters.contains(source) {
                            self.observed_adapters.push(*source);
                        }
                    }
                }
            }
            if matches!(&event.kind, crate::ClientSurfaceEventKind::Destroyed) {
                self.scratch_destroyed.push(event.surface);
            }
            events.push(event);
        }
        for index in 0..self.scratch_destroyed.len() {
            let surface = self.scratch_destroyed[index];
            self.forget_surface(surface);
        }
        self.scratch_destroyed.clear();

        for (source, adapter) in &mut self.adapters {
            adapter
                .driver
                .drain_route_alias_updates(&mut self.scratch_alias_updates);
            for update in self.scratch_alias_updates.drain(..) {
                self.sourced_alias_updates.push((*source, update));
            }
        }
        for index in 0..self.sourced_alias_updates.len() {
            let (source, update) = self.sourced_alias_updates[index];
            if update.destination.source() != source {
                continue;
            }
            match update.source {
                Some(target) if self.adapters.contains_key(&target.source()) => {
                    self.set_route_alias(update.destination, target);
                }
                Some(_) => {}
                None => self.remove_route_alias(update.destination),
            }
        }
        self.sourced_alias_updates.clear();

        for index in 0..self.observed_adapters.len() {
            let source = self.observed_adapters[index];
            let Some(adapter) = self.adapters.get_mut(&source) else {
                continue;
            };
            adapter.driver.drain_events(&mut self.scratch_events);
            while let Some(event) = self.scratch_events.pop_front() {
                if event.surface.source() == source {
                    if matches!(&event.kind, crate::ClientSurfaceEventKind::Destroyed) {
                        self.scratch_destroyed.push(event.surface);
                    }
                    events.push(event);
                } else {
                    invalid.push(ClientRuntimeEventError {
                        registered_source: source,
                        event_surface: event.surface,
                    });
                }
            }
        }
        self.observed_adapters.clear();
        for index in 0..self.scratch_destroyed.len() {
            let surface = self.scratch_destroyed[index];
            self.forget_surface(surface);
        }
        self.scratch_destroyed.clear();

        for (source, adapter) in &mut self.adapters {
            adapter.driver.drain_effects(&mut self.scratch_effects);
            self.pending_effects.extend(
                self.scratch_effects
                    .drain(..)
                    .map(|effect| (*source, effect)),
            );
        }

        for (source, adapter) in &mut self.adapters {
            adapter
                .driver
                .drain_retired_buffers(&mut self.scratch_retired_buffers);
            self.retired_buffers.extend(
                self.scratch_retired_buffers
                    .drain(..)
                    .filter(|buffer| buffer.source() == *source),
            );
        }
        for buffer in self.retired_buffers.drain(..) {
            for (source, adapter) in &mut self.adapters {
                if *source != buffer.source() {
                    adapter.driver.observe_retired_buffer(buffer);
                }
            }
        }
    }

    /// Applies external adapter work immediately after one event drain.
    pub fn apply_pending_effects(&mut self, invalid: &mut Vec<ClientRuntimeEffectError>) {
        let effects = std::mem::take(&mut self.pending_effects);
        for (adapter, effect) in effects {
            let target = effect.target_source();
            let applied = match effect {
                ClientAdapterEffect::Request(request) => self.apply_request(request),
                ClientAdapterEffect::Input(event) => self.apply_targeted_input(event),
            };
            if !applied {
                invalid.push(ClientRuntimeEffectError { adapter, target });
            }
        }
    }

    fn apply_targeted_input(&mut self, event: ClientInputEvent) -> bool {
        let source = event.target.surface().source();
        let Some(adapter) = self.adapters.get_mut(&source) else {
            return false;
        };
        adapter.driver.apply_input(event);
        true
    }

    pub fn apply_request(&mut self, request: ClientRequest) -> bool {
        if matches!(&request, ClientRequest::ClearFocus) {
            let Some(route) = self.keyboard_route else {
                return true;
            };
            let source = route.surface.source();
            let Some(adapter) = self.adapters.get_mut(&source) else {
                return false;
            };
            self.keyboard_route = None;
            adapter
                .driver
                .apply_request(ClientRequest::Focus(crate::ClientFocusRequest {
                    source,
                    surface: None,
                }));
            return true;
        }
        let Some(request) = self.resolve_request(request) else {
            return false;
        };
        let Some(source) = request.source() else {
            return false;
        };
        if !self.adapters.contains_key(&source) {
            return false;
        }
        if let ClientRequest::Focus(focus) = request {
            if let Some(previous) = self.keyboard_route
                && previous.surface.source() != source
                && let Some(adapter) = self.adapters.get_mut(&previous.surface.source())
            {
                adapter
                    .driver
                    .apply_request(ClientRequest::Focus(crate::ClientFocusRequest {
                        source: previous.surface.source(),
                        surface: None,
                    }));
            }
            let Some(adapter) = self.adapters.get_mut(&source) else {
                return false;
            };
            adapter.driver.apply_request(ClientRequest::Focus(focus));
            if focus.surface.is_some()
                || self
                    .keyboard_route
                    .is_some_and(|route| route.surface.source() == source)
            {
                self.keyboard_route = focus.surface.map(|surface| ClientKeyboardRoute { surface });
            }
        } else {
            let Some(adapter) = self.adapters.get_mut(&source) else {
                return false;
            };
            adapter.driver.apply_request(request);
        }
        true
    }

    pub fn apply_command(&mut self, command: ClientAdapterCommandEnvelope) -> bool {
        let Some(adapter) = self.adapters.get_mut(&command.source()) else {
            return false;
        };
        adapter.driver.apply_command(command);
        true
    }

    /// Delivers one input event that application policy did not consume.
    pub fn dispatch_unconsumed_input(
        &mut self,
        event: RuntimeInputEvent,
    ) -> ClientInputDispatchResult {
        let RuntimeInputEvent { event, time } = event;
        match event {
            RuntimeInputEventKind::Input(InputEventKind::PointerMotion { position }) => {
                self.pointer_position = Some(position);
                self.dispatch_pointer(
                    InputEventKind::PointerMotion { position },
                    Some(position),
                    time,
                )
            }
            RuntimeInputEventKind::Input(InputEventKind::PointerLeft { position }) => {
                self.pointer_position = Some(position);
                let result = self.dispatch_pointer(
                    InputEventKind::PointerLeft { position },
                    Some(position),
                    time,
                );
                if matches!(self.pointer_capture, PointerCapture::Active { .. }) {
                    self.pending_pointer_route = Some(None);
                } else {
                    self.pointer_route = None;
                }
                result
            }
            RuntimeInputEventKind::Input(InputEventKind::PointerButton {
                position,
                button,
                state,
            }) => {
                if let Some(position) = position {
                    self.pointer_position = Some(position);
                }
                let position = position.or(self.pointer_position);
                if !PressedButtons::supports(button) {
                    return ClientInputDispatchResult::UnsupportedButton;
                }
                if state == ButtonState::Pressed && self.pressed_buttons.is_empty() {
                    let route = match self.resolved_pointer_route(self.pointer_route) {
                        Ok(route) => route,
                        Err(error) => return error,
                    };
                    self.pointer_capture = PointerCapture::Active { route };
                }
                let route = match self.captured_or_current_pointer_route() {
                    Ok(route) => route,
                    Err(error) => return error,
                };
                let result = self.dispatch_pointer_to(
                    route,
                    InputEventKind::PointerButton {
                        position,
                        button,
                        state,
                    },
                    position,
                    time,
                );
                match state {
                    ButtonState::Pressed => self.pressed_buttons.insert(button),
                    ButtonState::Released => self.pressed_buttons.remove(button),
                }
                if self.pressed_buttons.is_empty()
                    && matches!(self.pointer_capture, PointerCapture::Active { .. })
                {
                    self.pointer_capture = PointerCapture::Idle;
                    if let Some(route) = self.pending_pointer_route.take() {
                        self.pointer_route = route;
                    }
                }
                result
            }
            RuntimeInputEventKind::Input(InputEventKind::PointerAxis { position, axis }) => {
                if let Some(position) = position {
                    self.pointer_position = Some(position);
                }
                let position = position.or(self.pointer_position);
                self.dispatch_axis(position, axis, time)
            }
            RuntimeInputEventKind::Input(InputEventKind::PointerGesture { gesture }) => {
                self.dispatch_gesture(gesture, time)
            }
            RuntimeInputEventKind::Input(InputEventKind::Keyboard { keycode, state }) => {
                let route = match self.resolved_keyboard_route(self.keyboard_route) {
                    Ok(Some(route)) => route,
                    Ok(None) => return ClientInputDispatchResult::NoRoute,
                    Err(error) => return error,
                };
                self.dispatch_to(
                    route.surface.source(),
                    ClientInputEvent {
                        target: ClientInputTarget::Keyboard {
                            surface: route.surface,
                        },
                        host_position: None,
                        event: InputEventKind::Keyboard { keycode, state },
                        time,
                    },
                )
            }
            RuntimeInputEventKind::HostFocusLost => self.host_focus_lost(time),
        }
    }

    pub fn host_focus_lost(&mut self, time: u32) -> ClientInputDispatchResult {
        let delivered = !self.adapters.is_empty();
        for adapter in self.adapters.values_mut() {
            adapter.driver.host_focus_lost(time);
        }
        // Host input sources synthesize gesture and finger-scroll cancellations
        // before this event, while their pinned routes are still available.
        self.pointer_route = None;
        self.pending_pointer_route = None;
        self.keyboard_route = None;
        self.pointer_capture = PointerCapture::Idle;
        self.gesture_capture = None;
        self.finger_scroll_capture = None;
        self.pressed_buttons.clear();
        self.pointer_position = None;
        if delivered {
            ClientInputDispatchResult::Delivered
        } else {
            ClientInputDispatchResult::NoRoute
        }
    }

    fn dispatch_pointer(
        &mut self,
        event: InputEventKind,
        position: Option<crate::InputPosition>,
        time: u32,
    ) -> ClientInputDispatchResult {
        let route = match self.captured_or_current_pointer_route() {
            Ok(route) => route,
            Err(error) => return error,
        };
        self.dispatch_pointer_to(route, event, position, time)
    }

    fn dispatch_pointer_to(
        &mut self,
        route: Option<ClientPointerRoute>,
        mut event: InputEventKind,
        position: Option<crate::InputPosition>,
        time: u32,
    ) -> ClientInputDispatchResult {
        let Some(route) = route else {
            return ClientInputDispatchResult::NoRoute;
        };
        let local = position.map(|position| route.transform.transform(position));
        match &mut event {
            InputEventKind::PointerMotion { position }
            | InputEventKind::PointerLeft { position } => {
                if let Some(local) = local {
                    *position = local;
                }
            }
            InputEventKind::PointerButton { position, .. }
            | InputEventKind::PointerAxis { position, .. } => {
                *position = local;
            }
            InputEventKind::PointerGesture { .. } | InputEventKind::Keyboard { .. } => {}
        }
        self.dispatch_to(
            route.surface.source(),
            ClientInputEvent {
                target: ClientInputTarget::Pointer {
                    surface: route.surface,
                    layer: route.layer,
                },
                host_position: position,
                event,
                time,
            },
        )
    }

    fn dispatch_axis(
        &mut self,
        position: Option<crate::InputPosition>,
        axis: RawScrollFrame,
        time: u32,
    ) -> ClientInputDispatchResult {
        if axis.source != RawScrollSource::Finger {
            return self.dispatch_pointer(
                InputEventKind::PointerAxis { position, axis },
                position,
                time,
            );
        }
        match axis.phase {
            RawScrollPhase::Started => {
                if let Some(capture) = self.finger_scroll_capture.take()
                    && (capture.horizontal_active || capture.vertical_active)
                {
                    let _ = self.dispatch_pointer_to(
                        capture.route,
                        InputEventKind::PointerAxis {
                            position,
                            axis: RawScrollFrame::cancelled_finger(
                                capture.horizontal_active,
                                capture.vertical_active,
                            ),
                        },
                        position,
                        time,
                    );
                }
                let route = match self.captured_or_current_pointer_route() {
                    Ok(route) => route,
                    Err(error) => return error,
                };
                self.finger_scroll_capture = Some(FingerScrollCapture::new(route, axis));
                self.dispatch_pointer_to(
                    route,
                    InputEventKind::PointerAxis { position, axis },
                    position,
                    time,
                )
            }
            RawScrollPhase::Moved => {
                let Some(mut capture) = self.finger_scroll_capture else {
                    return ClientInputDispatchResult::NoRoute;
                };
                capture.observe(axis);
                self.finger_scroll_capture = Some(capture);
                self.dispatch_pointer_to(
                    capture.route,
                    InputEventKind::PointerAxis { position, axis },
                    position,
                    time,
                )
            }
            RawScrollPhase::Ended | RawScrollPhase::Cancelled => {
                let Some(capture) = self.finger_scroll_capture.take() else {
                    return ClientInputDispatchResult::NoRoute;
                };
                self.dispatch_pointer_to(
                    capture.route,
                    InputEventKind::PointerAxis { position, axis },
                    position,
                    time,
                )
            }
        }
    }

    fn dispatch_gesture(
        &mut self,
        gesture: PointerGesture,
        time: u32,
    ) -> ClientInputDispatchResult {
        if gesture.is_begin() {
            if let Some(capture) = self.gesture_capture.take() {
                let _ = self.dispatch_pointer_to(
                    capture.route,
                    InputEventKind::PointerGesture {
                        gesture: capture.kind.cancelled(),
                    },
                    None,
                    time,
                );
            }
            let route = match self.captured_or_current_pointer_route() {
                Ok(route) => route,
                Err(error) => return error,
            };
            self.gesture_capture = Some(GestureCapture {
                kind: gesture.kind(),
                route,
            });
            return self.dispatch_pointer_to(
                route,
                InputEventKind::PointerGesture { gesture },
                None,
                time,
            );
        }
        // Input sources repair stale begins and drop unpaired updates or ends;
        // this layer only preserves the route of the validated sequence.
        let Some(capture) = self.gesture_capture else {
            return ClientInputDispatchResult::NoRoute;
        };
        let result = self.dispatch_pointer_to(
            capture.route,
            InputEventKind::PointerGesture { gesture },
            None,
            time,
        );
        if gesture.is_end() {
            self.gesture_capture = None;
        }
        result
    }

    fn dispatch_to(
        &mut self,
        source: ClientSourceId,
        event: ClientInputEvent,
    ) -> ClientInputDispatchResult {
        let Some(adapter) = self.adapters.get_mut(&source) else {
            return ClientInputDispatchResult::UnknownAdapter;
        };
        adapter.driver.apply_input(event);
        ClientInputDispatchResult::Delivered
    }

    fn captured_or_current_pointer_route(
        &self,
    ) -> Result<Option<ClientPointerRoute>, ClientInputDispatchResult> {
        match self.pointer_capture {
            PointerCapture::Idle => self.resolved_pointer_route(self.pointer_route),
            PointerCapture::Active { route } => Ok(route),
        }
    }

    fn resolved_pointer_route(
        &self,
        route: Option<ClientPointerRoute>,
    ) -> Result<Option<ClientPointerRoute>, ClientInputDispatchResult> {
        let Some(mut route) = route else {
            return Ok(None);
        };
        route.surface = self.resolve_alias(route.surface)?;
        Ok(Some(route))
    }

    fn resolved_keyboard_route(
        &self,
        route: Option<ClientKeyboardRoute>,
    ) -> Result<Option<ClientKeyboardRoute>, ClientInputDispatchResult> {
        let Some(mut route) = route else {
            return Ok(None);
        };
        route.surface = self.resolve_alias(route.surface)?;
        Ok(Some(route))
    }

    fn resolve_request(&self, request: ClientRequest) -> Option<ClientRequest> {
        match request {
            ClientRequest::Surface(mut request) => {
                request.surface = self.resolve_alias(request.surface).ok()?;
                Some(ClientRequest::Surface(request))
            }
            ClientRequest::Focus(mut request) => {
                if let Some(surface) = request.surface {
                    if surface.source() != request.source {
                        return None;
                    }
                    let surface = self.resolve_alias(surface).ok()?;
                    request.source = surface.source();
                    request.surface = Some(surface);
                }
                Some(ClientRequest::Focus(request))
            }
            ClientRequest::ClearFocus => Some(ClientRequest::ClearFocus),
        }
    }

    fn resolve_alias(
        &self,
        surface: ClientSurfaceId,
    ) -> Result<ClientSurfaceId, ClientInputDispatchResult> {
        if self.retired_aliases.contains(&surface) {
            return Err(ClientInputDispatchResult::NoRoute);
        }
        let mut current = surface;
        for _ in 0..=self.aliases.len() {
            let Some(next) = self.aliases.get(&current).copied() else {
                return Ok(current);
            };
            current = next;
        }
        Err(ClientInputDispatchResult::AliasCycle)
    }

    fn forget_surface(&mut self, surface: ClientSurfaceId) {
        let pointer_destroyed = self.route_resolves_to(self.pointer_route, surface);
        let pending_destroyed = self
            .pending_pointer_route
            .is_some_and(|route| self.route_resolves_to(route, surface));
        let keyboard_destroyed = self.keyboard_route.is_some_and(|route| {
            route.surface == surface || self.resolve_alias(route.surface) == Ok(surface)
        });
        let capture_destroyed = matches!(
            self.pointer_capture,
            PointerCapture::Active { route } if self.route_resolves_to(route, surface)
        );
        let gesture_destroyed = self
            .gesture_capture
            .is_some_and(|capture| self.route_resolves_to(capture.route, surface));
        let finger_scroll_destroyed = self
            .finger_scroll_capture
            .is_some_and(|capture| self.route_resolves_to(capture.route, surface));
        let aliases = &mut self.aliases;
        let retired = &mut self.retired_aliases;
        aliases.retain(|destination, source| {
            let retain = *destination != surface && *source != surface;
            if !retain {
                retired.insert(*destination);
            }
            retain
        });
        if pointer_destroyed {
            self.pointer_route = None;
        }
        if pending_destroyed {
            self.pending_pointer_route = Some(None);
        }
        if keyboard_destroyed {
            self.keyboard_route = None;
        }
        if capture_destroyed {
            self.pointer_capture = PointerCapture::Idle;
            self.pressed_buttons.clear();
            if let Some(route) = self.pending_pointer_route.take() {
                self.pointer_route = route;
            }
        }
        if gesture_destroyed {
            self.gesture_capture = None;
        }
        if finger_scroll_destroyed {
            self.finger_scroll_capture = None;
        }
    }

    fn route_resolves_to(
        &self,
        route: Option<ClientPointerRoute>,
        surface: ClientSurfaceId,
    ) -> bool {
        route.is_some_and(|route| {
            route.surface == surface || self.resolve_alias(route.surface) == Ok(surface)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;
    use crate::{
        ClientId, ClientProvenance, ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceRole,
        InputPosition, InputTransform, LinuxKeycode, SurfaceLayerId, ToplevelState,
        WindowDecoration,
    };

    #[derive(Default)]
    struct AdapterRecord {
        inputs: Vec<ClientInputEvent>,
        focus_lost: Vec<u32>,
        requests: Vec<ClientRequest>,
        commands: usize,
    }

    struct RecordingAdapter(Rc<RefCell<AdapterRecord>>);

    struct EffectAdapter(Option<ClientAdapterEffect>);

    impl ClientAdapter for EffectAdapter {
        fn drain_events(&mut self, _events: &mut ClientEventQueue) {}
        fn apply_request(&mut self, _request: ClientRequest) {}
        fn apply_input(&mut self, _event: ClientInputEvent) {}
        fn apply_command(&mut self, _command: ClientAdapterCommandEnvelope) {}
        fn host_focus_lost(&mut self, _time: u32) {}

        fn drain_effects(&mut self, effects: &mut Vec<ClientAdapterEffect>) {
            effects.extend(self.0.take());
        }
    }

    impl ClientAdapter for RecordingAdapter {
        fn drain_events(&mut self, _events: &mut ClientEventQueue) {}

        fn apply_request(&mut self, request: ClientRequest) {
            self.0.borrow_mut().requests.push(request);
        }

        fn apply_input(&mut self, event: ClientInputEvent) {
            self.0.borrow_mut().inputs.push(event);
        }

        fn apply_command(&mut self, _command: ClientAdapterCommandEnvelope) {
            self.0.borrow_mut().commands += 1;
        }

        fn host_focus_lost(&mut self, time: u32) {
            self.0.borrow_mut().focus_lost.push(time);
        }
    }

    fn surface(source: u64, client: u64, local: u64) -> ClientSurfaceId {
        ClientSurfaceId::new(ClientId::new(ClientSourceId::new(source), client), local)
    }

    fn register(runtime: &mut ClientRuntime, source: u64) -> Rc<RefCell<AdapterRecord>> {
        let record = Rc::new(RefCell::new(AdapterRecord::default()));
        runtime
            .register(ClientRuntimeAdapter::new(
                ClientSourceDescriptor::new(ClientSourceId::new(source), ClientProvenance::Local),
                RecordingAdapter(record.clone()),
            ))
            .expect("unique test source");
        record
    }

    #[test]
    fn source_namespaces_keep_equal_local_ids_distinct() {
        assert_ne!(surface(1, 4, 9), surface(2, 4, 9));
    }

    #[test]
    fn adapter_effects_apply_targeted_input_without_runtime_capture() {
        let mut runtime = ClientRuntime::default();
        let target = register(&mut runtime, 2);
        let target_surface = surface(2, 1, 1);
        runtime
            .register(ClientRuntimeAdapter::new(
                ClientSourceDescriptor::new(ClientSourceId::new(1), ClientProvenance::Relocated),
                EffectAdapter(Some(ClientAdapterEffect::Input(ClientInputEvent {
                    target: ClientInputTarget::Keyboard {
                        surface: target_surface,
                    },
                    host_position: None,
                    event: InputEventKind::Keyboard {
                        keycode: LinuxKeycode(4),
                        state: ButtonState::Pressed,
                    },
                    time: 7,
                }))),
            ))
            .expect("unique effect source");
        let mut events = ClientEventQueue::default();
        let mut invalid_events = Vec::new();
        let mut invalid_effects = Vec::new();

        runtime.drain_events(&mut events, &mut invalid_events);
        runtime.apply_pending_effects(&mut invalid_effects);

        assert!(invalid_events.is_empty());
        assert!(invalid_effects.is_empty());
        assert_eq!(target.borrow().inputs.len(), 1);
        assert_eq!(target.borrow().inputs[0].target.surface(), target_surface);
    }

    #[test]
    fn focus_switch_clears_the_old_adapter_and_unknown_focus_keeps_the_route() {
        let mut runtime = ClientRuntime::default();
        let first = register(&mut runtime, 1);
        let second = register(&mut runtime, 2);
        let first_surface = surface(1, 1, 1);
        let second_surface = surface(2, 1, 1);

        assert!(
            !runtime.apply_request(ClientRequest::Focus(crate::ClientFocusRequest {
                source: ClientSourceId::new(1),
                surface: Some(second_surface),
            }))
        );
        assert!(first.borrow().requests.is_empty());

        assert!(
            runtime.apply_request(ClientRequest::Focus(crate::ClientFocusRequest {
                source: ClientSourceId::new(1),
                surface: Some(first_surface),
            }))
        );
        assert!(
            !runtime.apply_request(ClientRequest::Focus(crate::ClientFocusRequest {
                source: ClientSourceId::new(3),
                surface: Some(surface(3, 1, 1)),
            }))
        );
        assert_eq!(
            runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(InputEventKind::Keyboard {
                    keycode: LinuxKeycode(1),
                    state: ButtonState::Pressed,
                }),
                1,
            )),
            ClientInputDispatchResult::Delivered
        );
        assert_eq!(first.borrow().inputs.len(), 1);

        assert!(
            runtime.apply_request(ClientRequest::Focus(crate::ClientFocusRequest {
                source: ClientSourceId::new(2),
                surface: Some(second_surface),
            }))
        );
        assert_eq!(
            first.borrow().requests.last(),
            Some(&ClientRequest::Focus(crate::ClientFocusRequest {
                source: ClientSourceId::new(1),
                surface: None,
            }))
        );
        assert_eq!(
            second.borrow().requests.last(),
            Some(&ClientRequest::Focus(crate::ClientFocusRequest {
                source: ClientSourceId::new(2),
                surface: Some(second_surface),
            }))
        );
        assert!(runtime.apply_request(ClientRequest::ClearFocus));
        assert_eq!(
            second.borrow().requests.last(),
            Some(&ClientRequest::Focus(crate::ClientFocusRequest {
                source: ClientSourceId::new(2),
                surface: None,
            }))
        );
    }

    #[test]
    fn pointer_route_switch_notifies_the_previous_adapter_before_the_next() {
        let mut runtime = ClientRuntime::default();
        let first = register(&mut runtime, 1);
        let second = register(&mut runtime, 2);
        let position = InputPosition::new(4.0, 5.0);
        let route = |surface| ClientPointerRoute {
            surface,
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        };

        runtime.publish_pointer_route(ClientPointerRouteUpdate {
            route: Some(route(surface(1, 1, 1))),
            position,
            time: 1,
        });
        runtime.publish_pointer_route(ClientPointerRouteUpdate {
            route: Some(route(surface(2, 1, 1))),
            position,
            time: 2,
        });

        assert!(matches!(
            first.borrow().inputs.last().map(|input| &input.event),
            Some(InputEventKind::PointerLeft { .. })
        ));
        assert!(matches!(
            second.borrow().inputs.last().map(|input| &input.event),
            Some(InputEventKind::PointerMotion { .. })
        ));
    }

    #[test]
    fn active_pointer_capture_defers_cross_source_handoff_without_sending_leave() {
        let mut runtime = ClientRuntime::default();
        let first = register(&mut runtime, 1);
        let second = register(&mut runtime, 2);
        let position = InputPosition::new(4.0, 5.0);
        let route = |surface| ClientPointerRoute {
            surface,
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        };
        runtime.set_pointer_route(Some(route(surface(1, 1, 1))));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerButton {
                position: Some(position),
                button: LinuxButtonCode(1),
                state: ButtonState::Pressed,
            }),
            1,
        ));

        runtime.publish_pointer_route(ClientPointerRouteUpdate {
            route: Some(route(surface(2, 1, 1))),
            position,
            time: 2,
        });
        assert!(
            first
                .borrow()
                .inputs
                .iter()
                .all(|input| !matches!(input.event, InputEventKind::PointerLeft { .. }))
        );
        assert!(second.borrow().inputs.is_empty());

        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerButton {
                position: Some(position),
                button: LinuxButtonCode(1),
                state: ButtonState::Released,
            }),
            3,
        ));
        assert_eq!(
            runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(InputEventKind::PointerMotion { position }),
                4,
            )),
            ClientInputDispatchResult::Delivered
        );
        assert_eq!(second.borrow().inputs.len(), 1);
    }

    struct OneEventAdapter(Option<ClientSurfaceEvent>);

    impl ClientAdapter for OneEventAdapter {
        fn drain_events(&mut self, events: &mut ClientEventQueue) {
            if let Some(event) = self.0.take() {
                events.push(event);
            }
        }

        fn apply_request(&mut self, _request: ClientRequest) {}
        fn apply_input(&mut self, _event: ClientInputEvent) {}
        fn apply_command(&mut self, _command: ClientAdapterCommandEnvelope) {}
        fn host_focus_lost(&mut self, _time: u32) {}
    }

    #[test]
    fn destroyed_surface_is_removed_from_retained_input_routes() {
        let source = ClientSourceId::new(1);
        let destroyed = surface(1, 1, 1);
        let mut runtime = ClientRuntime::default();
        runtime
            .register(ClientRuntimeAdapter::new(
                ClientSourceDescriptor::new(source, ClientProvenance::Local),
                OneEventAdapter(Some(ClientSurfaceEvent {
                    surface: destroyed,
                    kind: ClientSurfaceEventKind::Destroyed,
                })),
            ))
            .expect("unique source");
        runtime.set_pointer_route(Some(ClientPointerRoute {
            surface: destroyed,
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        }));
        runtime.set_keyboard_route(Some(ClientKeyboardRoute { surface: destroyed }));
        let mut events = ClientEventQueue::default();
        let mut invalid = Vec::new();

        runtime.drain_events(&mut events, &mut invalid);

        assert!(invalid.is_empty());
        assert!(matches!(
            events.pop_front(),
            Some(ClientSurfaceEvent {
                kind: ClientSurfaceEventKind::Destroyed,
                ..
            })
        ));
        assert_eq!(
            runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(InputEventKind::PointerMotion {
                    position: InputPosition::new(1.0, 1.0),
                }),
                1,
            )),
            ClientInputDispatchResult::NoRoute
        );
        assert_eq!(
            runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(InputEventKind::Keyboard {
                    keycode: LinuxKeycode(1),
                    state: ButtonState::Pressed,
                }),
                2,
            )),
            ClientInputDispatchResult::NoRoute
        );
    }

    #[test]
    fn pointer_capture_stays_on_its_initial_route_until_final_release() {
        let mut runtime = ClientRuntime::default();
        let first_record = register(&mut runtime, 1);
        let second_record = register(&mut runtime, 2);
        let first = surface(1, 1, 1);
        let second = surface(2, 1, 1);
        runtime.set_pointer_route(Some(ClientPointerRoute {
            surface: first,
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        }));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerButton {
                position: Some(InputPosition::new(10.0, 20.0)),
                button: LinuxButtonCode(0x110),
                state: ButtonState::Pressed,
            }),
            1,
        ));
        runtime.set_pointer_route(Some(ClientPointerRoute {
            surface: second,
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        }));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerMotion {
                position: InputPosition::new(30.0, 40.0),
            }),
            2,
        ));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerButton {
                position: Some(InputPosition::new(30.0, 40.0)),
                button: LinuxButtonCode(0x110),
                state: ButtonState::Released,
            }),
            3,
        ));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerMotion {
                position: InputPosition::new(50.0, 60.0),
            }),
            4,
        ));

        assert_eq!(first_record.borrow().inputs.len(), 3);
        assert_eq!(second_record.borrow().inputs.len(), 1);
    }

    #[test]
    fn pointer_leave_clears_idle_route_but_defers_clear_during_capture() {
        let mut runtime = ClientRuntime::default();
        let record = register(&mut runtime, 1);
        let pointer = ClientPointerRoute {
            surface: surface(1, 1, 1),
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        };
        runtime.set_pointer_route(Some(pointer));
        assert_eq!(
            runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(InputEventKind::PointerLeft {
                    position: InputPosition::new(1.0, 2.0),
                }),
                1,
            )),
            ClientInputDispatchResult::Delivered
        );
        assert_eq!(
            runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(InputEventKind::PointerMotion {
                    position: InputPosition::new(2.0, 3.0),
                }),
                2,
            )),
            ClientInputDispatchResult::NoRoute
        );

        runtime.set_pointer_route(Some(pointer));
        let button = LinuxButtonCode(0x110);
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerButton {
                position: None,
                button,
                state: ButtonState::Pressed,
            }),
            3,
        ));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerLeft {
                position: InputPosition::new(3.0, 4.0),
            }),
            4,
        ));
        assert!(
            runtime
                .dispatch_unconsumed_input(RuntimeInputEvent::new(
                    RuntimeInputEventKind::Input(InputEventKind::PointerMotion {
                        position: InputPosition::new(4.0, 5.0),
                    }),
                    5,
                ))
                .is_delivered()
        );
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerButton {
                position: None,
                button,
                state: ButtonState::Released,
            }),
            6,
        ));
        assert_eq!(
            runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(InputEventKind::PointerMotion {
                    position: InputPosition::new(5.0, 6.0),
                }),
                7,
            )),
            ClientInputDispatchResult::NoRoute
        );
        assert_eq!(record.borrow().inputs.len(), 5);
    }

    #[test]
    fn gesture_and_finger_scroll_keep_their_begin_route() {
        let mut runtime = ClientRuntime::default();
        let first = register(&mut runtime, 1);
        let second = register(&mut runtime, 2);
        let first_route = ClientPointerRoute {
            surface: surface(1, 1, 1),
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        };
        let second_route = ClientPointerRoute {
            surface: surface(2, 1, 1),
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        };
        runtime.set_pointer_route(Some(first_route));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerGesture {
                gesture: PointerGesture::Swipe(crate::TouchpadSwipe::Begin { fingers: 3 }),
            }),
            1,
        ));
        runtime.set_pointer_route(Some(second_route));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerGesture {
                gesture: PointerGesture::Swipe(crate::TouchpadSwipe::Update {
                    delta: crate::InputDelta::new(1.0, 2.0),
                }),
            }),
            2,
        ));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerGesture {
                gesture: PointerGesture::Swipe(crate::TouchpadSwipe::End { cancelled: false }),
            }),
            3,
        ));

        let finger = |phase| RawScrollFrame {
            source: RawScrollSource::Finger,
            phase,
            horizontal: 0.0,
            vertical: 1.0,
            horizontal_v120: None,
            vertical_v120: None,
            horizontal_stop: false,
            vertical_stop: false,
        };
        runtime.set_pointer_route(Some(first_route));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerAxis {
                position: None,
                axis: finger(RawScrollPhase::Started),
            }),
            4,
        ));
        runtime.set_pointer_route(Some(second_route));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerAxis {
                position: None,
                axis: finger(RawScrollPhase::Moved),
            }),
            5,
        ));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerAxis {
                position: None,
                axis: finger(RawScrollPhase::Ended),
            }),
            6,
        ));

        assert_eq!(first.borrow().inputs.len(), 6);
        assert!(second.borrow().inputs.is_empty());
    }

    #[test]
    fn stale_sequence_restart_cancels_the_old_route_with_exact_axes() {
        let mut runtime = ClientRuntime::default();
        let first = register(&mut runtime, 1);
        let second = register(&mut runtime, 2);
        let first_route = ClientPointerRoute {
            surface: surface(1, 1, 1),
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        };
        let second_route = ClientPointerRoute {
            surface: surface(2, 1, 1),
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        };
        runtime.set_pointer_route(Some(first_route));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerGesture {
                gesture: PointerGesture::Swipe(crate::TouchpadSwipe::Begin { fingers: 3 }),
            }),
            1,
        ));
        runtime.set_pointer_route(Some(second_route));
        assert!(
            runtime
                .dispatch_unconsumed_input(RuntimeInputEvent::new(
                    RuntimeInputEventKind::Input(InputEventKind::PointerGesture {
                        gesture: PointerGesture::Pinch(crate::TouchpadPinch::Begin { fingers: 2 }),
                    }),
                    2,
                ))
                .is_delivered()
        );

        let finger = |phase, horizontal, vertical| RawScrollFrame {
            source: RawScrollSource::Finger,
            phase,
            horizontal,
            vertical,
            horizontal_v120: None,
            vertical_v120: None,
            horizontal_stop: false,
            vertical_stop: false,
        };
        runtime.set_pointer_route(Some(first_route));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerAxis {
                position: None,
                axis: finger(RawScrollPhase::Started, 1.0, 0.0),
            }),
            3,
        ));
        runtime.set_pointer_route(Some(second_route));
        assert!(
            runtime
                .dispatch_unconsumed_input(RuntimeInputEvent::new(
                    RuntimeInputEventKind::Input(InputEventKind::PointerAxis {
                        position: None,
                        axis: finger(RawScrollPhase::Started, 0.0, 1.0),
                    }),
                    4,
                ))
                .is_delivered()
        );

        let first = first.borrow();
        assert!(matches!(
            first.inputs[1].event,
            InputEventKind::PointerGesture {
                gesture: PointerGesture::Swipe(crate::TouchpadSwipe::End { cancelled: true })
            }
        ));
        let InputEventKind::PointerAxis { axis, .. } = first.inputs[3].event else {
            panic!("expected synthesized finger cancellation");
        };
        assert_eq!(axis.phase, RawScrollPhase::Cancelled);
        assert!(axis.horizontal_stop);
        assert!(!axis.vertical_stop);
        assert_eq!(second.borrow().inputs.len(), 2);
    }

    #[test]
    fn pointer_leave_preserves_sequences_and_focus_loss_clears_them() {
        let mut runtime = ClientRuntime::default();
        let record = register(&mut runtime, 1);
        runtime.set_pointer_route(Some(ClientPointerRoute {
            surface: surface(1, 1, 1),
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        }));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerGesture {
                gesture: PointerGesture::Swipe(crate::TouchpadSwipe::Begin { fingers: 3 }),
            }),
            1,
        ));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerAxis {
                position: None,
                axis: RawScrollFrame {
                    source: RawScrollSource::Finger,
                    phase: RawScrollPhase::Started,
                    horizontal: 0.0,
                    vertical: 1.0,
                    horizontal_v120: None,
                    vertical_v120: None,
                    horizontal_stop: false,
                    vertical_stop: false,
                },
            }),
            2,
        ));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerLeft {
                position: InputPosition::new(2.0, 3.0),
            }),
            3,
        ));
        assert!(
            runtime
                .dispatch_unconsumed_input(RuntimeInputEvent::new(
                    RuntimeInputEventKind::Input(InputEventKind::PointerGesture {
                        gesture: PointerGesture::Swipe(crate::TouchpadSwipe::Update {
                            delta: crate::InputDelta::new(1.0, 1.0),
                        }),
                    }),
                    4,
                ))
                .is_delivered()
        );
        assert!(
            runtime
                .dispatch_unconsumed_input(RuntimeInputEvent::new(
                    RuntimeInputEventKind::Input(InputEventKind::PointerAxis {
                        position: None,
                        axis: RawScrollFrame {
                            source: RawScrollSource::Finger,
                            phase: RawScrollPhase::Moved,
                            horizontal: 0.0,
                            vertical: 1.0,
                            horizontal_v120: None,
                            vertical_v120: None,
                            horizontal_stop: false,
                            vertical_stop: false,
                        },
                    }),
                    5,
                ))
                .is_delivered()
        );
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::HostFocusLost,
            6,
        ));
        assert_eq!(
            runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(InputEventKind::PointerGesture {
                    gesture: PointerGesture::Swipe(crate::TouchpadSwipe::Update {
                        delta: crate::InputDelta::new(1.0, 1.0),
                    }),
                }),
                7,
            )),
            ClientInputDispatchResult::NoRoute
        );
        assert_eq!(record.borrow().focus_lost, vec![6]);
    }

    #[test]
    fn route_alias_rewrites_pointer_and_keyboard_targets() {
        let mut runtime = ClientRuntime::default();
        let local_record = register(&mut runtime, 1);
        let relocated_record = register(&mut runtime, 2);
        let local = surface(1, 1, 1);
        let relocated = surface(2, 1, 1);
        runtime.set_route_alias(relocated, local);
        runtime.set_pointer_route(Some(ClientPointerRoute {
            surface: relocated,
            layer: SurfaceLayerId::new(4),
            transform: InputTransform::IDENTITY,
        }));
        runtime.set_keyboard_route(Some(ClientKeyboardRoute { surface: relocated }));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerMotion {
                position: InputPosition::new(1.0, 2.0),
            }),
            1,
        ));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::Keyboard {
                keycode: LinuxKeycode(30),
                state: ButtonState::Pressed,
            }),
            2,
        ));

        assert_eq!(local_record.borrow().inputs.len(), 2);
        assert!(relocated_record.borrow().inputs.is_empty());
        assert!(matches!(
            local_record.borrow().inputs[1].target,
            ClientInputTarget::Keyboard { surface } if surface == local
        ));
    }

    #[test]
    fn pointer_hover_and_keyboard_focus_route_independently() {
        let mut runtime = ClientRuntime::default();
        let pointer_record = register(&mut runtime, 1);
        let keyboard_record = register(&mut runtime, 2);
        runtime.set_pointer_route(Some(ClientPointerRoute {
            surface: surface(1, 1, 1),
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        }));
        runtime.set_keyboard_route(Some(ClientKeyboardRoute {
            surface: surface(2, 1, 1),
        }));

        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::PointerMotion {
                position: InputPosition::new(4.0, 5.0),
            }),
            1,
        ));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::Keyboard {
                keycode: LinuxKeycode(30),
                state: ButtonState::Pressed,
            }),
            2,
        ));

        assert_eq!(pointer_record.borrow().inputs.len(), 1);
        assert_eq!(keyboard_record.borrow().inputs.len(), 1);
        assert!(matches!(
            pointer_record.borrow().inputs[0].target,
            ClientInputTarget::Pointer { .. }
        ));
        assert!(matches!(
            keyboard_record.borrow().inputs[0].target,
            ClientInputTarget::Keyboard { .. }
        ));
    }

    #[test]
    fn host_focus_loss_reaches_every_adapter_and_clears_routes() {
        let mut runtime = ClientRuntime::default();
        let first = register(&mut runtime, 1);
        let second = register(&mut runtime, 2);
        runtime.set_keyboard_route(Some(ClientKeyboardRoute {
            surface: surface(1, 1, 1),
        }));

        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::HostFocusLost,
            42,
        ));
        runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
            RuntimeInputEventKind::Input(InputEventKind::Keyboard {
                keycode: LinuxKeycode(30),
                state: ButtonState::Released,
            }),
            43,
        ));

        assert_eq!(first.borrow().focus_lost, vec![42]);
        assert_eq!(second.borrow().focus_lost, vec![42]);
        assert!(first.borrow().inputs.is_empty());
    }

    #[test]
    fn input_dispatch_distinguishes_normal_absence_from_route_failures() {
        let mut runtime = ClientRuntime::default();
        let _ = register(&mut runtime, 1);
        let motion = |time| {
            RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(InputEventKind::PointerMotion {
                    position: InputPosition::new(1.0, 2.0),
                }),
                time,
            )
        };
        assert_eq!(
            runtime.dispatch_unconsumed_input(motion(1)),
            ClientInputDispatchResult::NoRoute
        );

        runtime.set_pointer_route(Some(ClientPointerRoute {
            surface: surface(9, 1, 1),
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        }));
        assert_eq!(
            runtime.dispatch_unconsumed_input(motion(2)),
            ClientInputDispatchResult::UnknownAdapter
        );

        let first = surface(1, 1, 1);
        let second = surface(1, 1, 2);
        runtime.set_route_alias(first, second);
        runtime.set_route_alias(second, first);
        runtime.set_pointer_route(Some(ClientPointerRoute {
            surface: first,
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        }));
        assert_eq!(
            runtime.dispatch_unconsumed_input(motion(3)),
            ClientInputDispatchResult::AliasCycle
        );
        runtime.remove_route_alias(first);
        runtime.remove_route_alias(second);

        assert_eq!(
            runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(InputEventKind::PointerButton {
                    position: None,
                    button: LinuxButtonCode(0x300),
                    state: ButtonState::Pressed,
                }),
                4,
            )),
            ClientInputDispatchResult::UnsupportedButton
        );
    }

    struct ForeignEventAdapter {
        surface: ClientSurfaceId,
    }

    impl ClientAdapter for ForeignEventAdapter {
        fn drain_events(&mut self, events: &mut ClientEventQueue) {
            events.push(ClientSurfaceEvent {
                surface: self.surface,
                kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                    parent: None,
                    decoration: WindowDecoration::ClientSide,
                })),
            });
        }

        fn apply_request(&mut self, _request: ClientRequest) {}
        fn apply_input(&mut self, _event: ClientInputEvent) {}
        fn apply_command(&mut self, _command: ClientAdapterCommandEnvelope) {}
        fn host_focus_lost(&mut self, _time: u32) {}
    }

    #[test]
    fn runtime_rejects_events_outside_the_registered_namespace() {
        let registered = ClientSourceId::new(1);
        let foreign = surface(2, 1, 1);
        let mut runtime = ClientRuntime::default();
        runtime
            .register(ClientRuntimeAdapter::new(
                ClientSourceDescriptor::new(registered, ClientProvenance::Local),
                ForeignEventAdapter { surface: foreign },
            ))
            .expect("unique test source");
        let mut events = ClientEventQueue::default();
        let mut invalid = Vec::new();

        runtime.drain_events(&mut events, &mut invalid);

        assert!(events.is_empty());
        assert_eq!(
            invalid,
            vec![ClientRuntimeEventError {
                registered_source: registered,
                event_surface: foreign,
            }]
        );
    }

    #[test]
    fn duplicate_registration_and_unknown_egress_are_reported() {
        let source = ClientSourceId::new(1);
        let mut runtime = ClientRuntime::default();
        let _ = register(&mut runtime, source.raw());
        let duplicate_record = Rc::new(RefCell::new(AdapterRecord::default()));
        let duplicate = runtime.register(ClientRuntimeAdapter::new(
            ClientSourceDescriptor::new(source, ClientProvenance::Local),
            RecordingAdapter(duplicate_record),
        ));
        assert_eq!(
            duplicate.expect_err("duplicate source must fail"),
            ClientRuntimeRegistrationError { source }
        );

        let unknown = ClientSourceId::new(99);
        assert!(
            !runtime.apply_request(ClientRequest::Focus(crate::ClientFocusRequest {
                source: unknown,
                surface: None,
            }))
        );
        assert!(!runtime.apply_command(ClientAdapterCommandEnvelope::new(unknown, 7_u32)));
    }

    #[test]
    fn registration_keeps_the_importer_descriptor_paired() {
        let descriptor =
            ClientSourceDescriptor::new(ClientSourceId::new(8), ClientProvenance::Relocated);
        let record = Rc::new(RefCell::new(AdapterRecord::default()));
        let parts = ClientAdapterRegistration::new(
            descriptor,
            RecordingAdapter(record),
            String::from("importer"),
        )
        .into_parts();

        assert_eq!(parts.runtime.descriptor, descriptor);
        assert_eq!(parts.importer.descriptor, descriptor);
        let importer = parts
            .importer
            .importer
            .downcast::<String>()
            .expect("registered importer type");
        assert_eq!(*importer, "importer");
    }
}

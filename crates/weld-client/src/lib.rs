//! Runtime-independent client, surface, buffer, and input contracts.
//!
//! Client adapters translate their native protocol into this model. The model
//! deliberately contains no Smithay, Bevy, renderer, codec, or transport
//! types, allowing local Wayland clients and imported hoist clients to enter
//! Weld through the same lifecycle.

mod adapter;
mod buffer;
mod cursor;
mod geometry;
mod id;
mod input;
mod input_geometry;
mod mailbox;
mod metadata;
mod pending;
mod pending_order;
mod presentation;
mod presenter;
mod surface;
mod touch;
#[cfg(feature = "serde")]
mod wire;

pub use adapter::{
    ClientAdapter, ClientAdapterCommandEnvelope, ClientAdapterEffect, ClientAdapterRegistration,
    ClientAdapterRegistrationParts, ClientImporterRegistration, ClientInputDispatchResult,
    ClientRouteAliasUpdate, ClientRuntime, ClientRuntimeAdapter, ClientRuntimeEffectError,
    ClientRuntimeEventError, ClientRuntimeRegistrationError,
};
pub use buffer::{
    ClientBufferId, ClientBufferLease, ClientBufferLeaseSourceMismatch, ClientBufferMetadata,
    ClientBufferUseId,
};
pub use cursor::{
    ClientCursor, ClientCursorImage, ClientCursorUpdate, CursorIcon, CursorImageError,
};
pub use geometry::{Extent, LogicalPoint, LogicalSize};
pub use id::{
    ClientId, ClientOutputId, ClientProvenance, ClientSourceDescriptor, ClientSourceId,
    ClientSurfaceId, ControlOnlyClientImporter, PassthroughClientImporter, SurfaceLayerId,
};
pub use input::{
    ButtonState, ClientInputController, ClientInputEvent, ClientInputTarget, ClientKeyboardRoute,
    ClientPointerRoute, ClientPointerRouteUpdate, InputDelta, InputEventKind, InputPosition,
    InputTransform, KeyboardKeyState, LinuxButtonCode, LinuxKeycode, MAX_TOUCH_CONTACTS,
    PointerGesture, PointerGestureKind, RawScrollFrame, RawScrollPhase, RawScrollSource,
    RelativeMotion, RuntimeInputEvent, RuntimeInputEventKind, TouchEvent, TouchId, TouchpadHold,
    TouchpadPinch, TouchpadSwipe,
};
pub use input_geometry::SurfaceInputGeometry;
pub use mailbox::{PresentationMailbox, PresentationQueueStats};
pub use metadata::{ClientSurfaceMetadata, MAX_SURFACE_LABEL_BYTES, SurfaceMetadataError};
pub use pending::{PendingClientEvents, PendingEventUpdate};
pub use presentation::{
    ClientPresentationClaim, ClientPresentationUpdate, PresentationGroupId, PresentationRate,
    PresentationRole, SurfaceBitratePreference,
};
pub use presenter::{ClientPresentationInbox, PresentationDemand};
pub use surface::{
    ClientCommitRevision, ClientEventQueue, ClientEventSink, ClientFocusRequest, ClientRequest,
    ClientSurfaceCommit, ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceRequest,
    ClientSurfaceRequestKind, ClientSurfaceRole, ClientSurfaceState, DesktopLayer,
    LayerKeyboardInteractivity, LayerSurfaceState, PopupState, SurfaceAlphaMode,
    SurfaceBufferChange, SurfaceBufferUpdate, SurfaceContentView, SurfaceInputPlacement,
    SurfaceInputRect, SurfaceLayerPlacement, SurfaceWindowGeometry, ToplevelHints,
    ToplevelInteractionRequestKind, ToplevelKind, ToplevelLayout, ToplevelState,
    ToplevelStateRequestKind, WindowDecoration, WindowResizeEdge,
};
#[cfg(feature = "serde")]
pub use wire::{
    WireClientInputEvent, WireClientSurfaceCommit, WireClientSurfaceEvent,
    WireClientSurfaceEventKind, WireSurfaceBufferChange, WireSurfaceBufferUpdate,
};

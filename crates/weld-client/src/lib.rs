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
mod presentation;
mod surface;
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
    ButtonState, ClientInputEvent, ClientInputTarget, ClientKeyboardRoute, ClientPointerRoute,
    ClientPointerRouteUpdate, InputDelta, InputEventKind, InputPosition, InputTransform,
    KeyboardKeyState, LinuxButtonCode, LinuxKeycode, PointerGesture, PointerGestureKind,
    RawScrollFrame, RawScrollPhase, RawScrollSource, RuntimeInputEvent, RuntimeInputEventKind,
    TouchpadHold, TouchpadPinch, TouchpadSwipe,
};
pub use input_geometry::SurfaceInputGeometry;
pub use presentation::{ClientPresentationClaim, ClientPresentationUpdate, PresentationRate};
pub use surface::{
    ClientCommitRevision, ClientEventQueue, ClientFocusRequest, ClientRequest, ClientSurfaceCommit,
    ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceRequest, ClientSurfaceRequestKind,
    ClientSurfaceRole, PopupState, SurfaceAlphaMode, SurfaceBufferChange, SurfaceBufferUpdate,
    SurfaceContentView, SurfaceInputPlacement, SurfaceInputRect, SurfaceLayerPlacement,
    SurfaceWindowGeometry, ToplevelInteractionRequestKind, ToplevelState, WindowDecoration,
    WindowResizeEdge,
};
#[cfg(feature = "serde")]
pub use wire::{
    WireClientInputEvent, WireClientSurfaceCommit, WireClientSurfaceEvent,
    WireClientSurfaceEventKind, WireSurfaceBufferChange, WireSurfaceBufferUpdate,
};

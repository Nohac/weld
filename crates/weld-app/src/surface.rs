//! Bevy-facing application-surface state and client-content rendering.
//!
//! Smithay feeds this module owned lifecycle events and pixel data. The durable
//! [`ClientSurface`] entities contain protocol-neutral state only. Presentation
//! plugins claim that entity separately and render its content through
//! [`SurfaceNode`]; its root and overlay rendering materials remain internal.

use std::collections::{HashMap, HashSet, VecDeque};

use bevy::{
    app::{App, Plugin, PreUpdate},
    asset::{Asset, AssetId, Assets, Handle, RenderAssetUsages, load_internal_asset, uuid_handle},
    ecs::{
        component::Component,
        entity::Entity,
        hierarchy::{ChildOf, Children},
        message::Message,
        query::{With, Without},
        resource::Resource,
        schedule::{IntoScheduleConfigs, SystemSet},
        system::{Query, ResMut, SystemParam},
        world::World,
    },
    image::Image,
    math::{UVec2, Vec2, Vec4},
    picking::{Pickable, PickingSystems},
    prelude::px,
    reflect::TypePath,
    render::{
        RenderApp,
        render_resource::{AsBindGroup, Extent3d, ShaderType, TextureDimension, TextureFormat},
    },
    shader::{Shader, ShaderRef},
    ui::{Display, LayoutConfig, Node, PositionType, Val},
    ui_render::{
        UiMaterialPlugin, prelude::UiMaterial, stack_z_offsets, ui_material::MaterialNode,
    },
};
use tracing::warn;
use weld_core::dmabuf::ImportId;
pub use weld_core::surface::{SurfaceId, SurfaceLayerId};

use crate::composition::composition_advance_requested;

#[path = "surface/binding.rs"]
mod binding;
pub(crate) use binding::publish_surface_bindings;

const SURFACE_MATERIAL_SHADER: Handle<Shader> =
    uuid_handle!("f69ff2a0-2dc4-4a34-8f64-bf77475398a1");

#[derive(Clone, Copy, Debug, PartialEq, ShaderType)]
struct SurfaceMaterialParameters {
    source_rect: Vec4,
    buffer_size: Vec2,
    flags: UVec2,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SurfaceParameterKey([u32; 8]);

impl From<SurfaceMaterialParameters> for SurfaceParameterKey {
    fn from(parameters: SurfaceMaterialParameters) -> Self {
        Self([
            parameters.source_rect.x.to_bits(),
            parameters.source_rect.y.to_bits(),
            parameters.source_rect.z.to_bits(),
            parameters.source_rect.w.to_bits(),
            parameters.buffer_size.x.to_bits(),
            parameters.buffer_size.y.to_bits(),
            parameters.flags.x,
            parameters.flags.y,
        ])
    }
}

#[derive(Asset, AsBindGroup, Clone, Debug, PartialEq, TypePath)]
struct SurfaceUiMaterial {
    #[texture(0, filterable = false)]
    image: Handle<Image>,
    #[uniform(1)]
    parameters: SurfaceMaterialParameters,
}

impl UiMaterial for SurfaceUiMaterial {
    fn fragment_shader() -> ShaderRef {
        SURFACE_MATERIAL_SHADER.into()
    }

    fn stack_z_offset() -> f32 {
        stack_z_offsets::IMAGE
    }
}

/// Semantic application-window state exposed to compositor plugins.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientToplevel {
    pub surface: SurfaceId,
}

/// Generic identity shared by every buffer-bearing client surface role.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientSurface {
    pub surface: SurfaceId,
}

/// Protocol-owned popup placement relative to its owning window geometry.
///
/// Unlike [`ClientToplevel`], this role has no shell-owned placement, decoration,
/// dragging, or resizing policy. Presentation plugins consume the committed
/// position and stacking rank without rewriting them.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct ClientPopup {
    pub owner: SurfaceId,
    pub position: Vec2,
    pub stack_index: i32,
}

/// Which side owns the visible frame and titlebar for an application window.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowDecoration {
    #[default]
    ClientSide,
    ServerSide,
}

/// A window whose client owns its frame, titlebar, and resize handles.
#[derive(Component, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClientDecorated;

/// A window whose compositor owns its frame and titlebar.
#[derive(Component, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ServerDecorated;

/// Protocol-neutral state available while an application surface is mapped.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct MappedSurface {
    /// Window-geometry extent exposed to shell layout, excluding client-side
    /// shadows and other invisible root-buffer margins.
    pub logical_size: Vec2,
    /// Offset from the window-geometry anchor to the full surface bounds.
    pub visual_offset: Vec2,
    /// Full surface extent, including client-owned visual overflow.
    pub visual_size: Vec2,
    pub opaque: bool,
}

impl MappedSurface {
    /// Whether the client renders pixels outside its declared window geometry.
    pub fn has_visual_overflow(self) -> bool {
        self.visual_offset != Vec2::ZERO || self.visual_size != self.logical_size
    }
}

/// Which logical bounds a [`SurfaceNode`] presents.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SurfaceView {
    /// Present the complete client surface, including CSD shadow margins.
    FullSurface,
    /// Present only the declared xdg window geometry.
    #[default]
    WindowGeometry,
}

/// A client surface that composes as an ordinary Bevy UI primitive.
///
/// Plugins should decorate or arrange this component's entity rather than
/// depending on its internal material and ignored overlay children.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[require(MaterialNode<SurfaceUiMaterial>, Node)]
pub struct SurfaceNode {
    pub surface: SurfaceId,
    pub view: SurfaceView,
}

/// Protocol-neutral request emitted by ECS policy for the host to apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceAction {
    Close {
        surface: SurfaceId,
    },
    Focus {
        surface: Option<SurfaceId>,
    },
    Resize {
        surface: SurfaceId,
        logical_size: bevy::math::UVec2,
    },
}

/// Edge or corner selected by a client for an interactive resize.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToplevelResizeEdge {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    BottomLeft,
    TopRight,
    BottomRight,
}

impl ToplevelResizeEdge {
    pub const fn has_left(self) -> bool {
        matches!(self, Self::Left | Self::TopLeft | Self::BottomLeft)
    }

    pub const fn has_right(self) -> bool {
        matches!(self, Self::Right | Self::TopRight | Self::BottomRight)
    }

    pub const fn has_top(self) -> bool {
        matches!(self, Self::Top | Self::TopLeft | Self::TopRight)
    }

    pub const fn has_bottom(self) -> bool {
        matches!(self, Self::Bottom | Self::BottomLeft | Self::BottomRight)
    }
}

/// Validated client request for compositor-owned pointer interaction.
#[derive(Clone, Copy, Debug, Eq, Message, PartialEq)]
pub struct ToplevelInteractionRequest {
    pub surface: SurfaceId,
    pub kind: ToplevelInteractionRequestKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToplevelInteractionRequestKind {
    Move,
    Resize { edges: ToplevelResizeEdge },
    End,
}

#[derive(Resource, Default)]
pub struct SurfaceActionQueue(VecDeque<SurfaceAction>);

impl SurfaceActionQueue {
    pub fn push(&mut self, action: SurfaceAction) {
        self.0.push_back(action);
    }
}

/// Internal render backing for one mapped client surface tree.
#[derive(Component, Clone, Debug, PartialEq)]
struct SurfaceContent {
    root: SurfaceLayerContent,
    window_geometry: SurfaceWindowGeometry,
    overlays: Vec<SurfaceLayerContent>,
    inputs: Vec<SurfaceInputPlacement>,
}

#[derive(Clone, Debug, PartialEq)]
struct SurfaceLayerContent {
    layer: SurfaceLayerId,
    image: Handle<Image>,
    view: SurfaceContentView,
    position: Vec2,
    pixel_size: (u32, u32),
}

/// ECS-facing content for a surface layer. Host buffer types stop before this boundary.
#[derive(Debug)]
#[doc(hidden)]
pub enum SurfaceBufferContent {
    Retained,
    Pixels(Vec<u8>),
    RenderImage(SurfaceRenderImage),
}

/// Sampling metadata for an externally owned image resolved before ECS ingress.
#[derive(Clone, Debug, PartialEq)]
#[doc(hidden)]
pub struct SurfaceRenderImage {
    pub import: ImportId,
    pub image: Handle<Image>,
    pub encoding: SurfaceImageEncoding,
    pub y_inverted: bool,
    pub promoted: bool,
}

/// Color representation consumed by the private surface material.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum SurfaceImageEncoding {
    Unbound,
    LinearStraight,
    EncodedPremultiplied,
    EncodedOpaque,
}

/// Updated metadata and optional content for one tree layer.
#[derive(Debug)]
#[doc(hidden)]
pub struct SurfaceBufferUpdate {
    pub layer: SurfaceLayerId,
    pub width: u32,
    pub height: u32,
    pub content: SurfaceBufferContent,
    pub opaque: bool,
}

/// Placement of one imported layer in root-surface logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
#[doc(hidden)]
pub struct SurfaceLayerPlacement {
    pub layer: SurfaceLayerId,
    pub position: Vec2,
    pub view: SurfaceContentView,
}

/// Effective xdg window geometry within the full root surface.
#[derive(Clone, Copy, Debug, PartialEq)]
#[doc(hidden)]
pub struct SurfaceWindowGeometry {
    pub origin: Vec2,
    pub view: SurfaceContentView,
}

/// One effective rectangular part of a Wayland surface's input region.
#[derive(Clone, Copy, Debug, PartialEq)]
#[doc(hidden)]
pub struct SurfaceInputRect {
    pub position: Vec2,
    pub size: Vec2,
}

/// Input regions for one exact root or subsurface layer.
#[derive(Clone, Debug, PartialEq)]
#[doc(hidden)]
pub struct SurfaceInputPlacement {
    pub layer: SurfaceLayerId,
    pub position: Vec2,
    pub regions: Vec<SurfaceInputRect>,
}

/// Complete visible tree state plus pixel deltas copied at the Smithay boundary.
#[derive(Debug)]
#[doc(hidden)]
pub struct SurfaceTreeSnapshot {
    pub client_mapped: bool,
    pub root: Option<SurfaceLayerPlacement>,
    pub window_geometry: Option<SurfaceWindowGeometry>,
    pub overlays: Vec<SurfaceLayerPlacement>,
    pub inputs: Vec<SurfaceInputPlacement>,
    pub buffers: Vec<SurfaceBufferUpdate>,
}

impl SurfaceTreeSnapshot {
    fn carry_pending_content_from(&mut self, previous: &mut Self) {
        let retained = self
            .buffers
            .iter()
            .map(|buffer| buffer.layer)
            .collect::<HashSet<_>>();
        let mut pending = previous
            .buffers
            .iter_mut()
            .filter(|buffer| retained.contains(&buffer.layer))
            .filter_map(|buffer| {
                let content =
                    std::mem::replace(&mut buffer.content, SurfaceBufferContent::Retained);
                (!matches!(content, SurfaceBufferContent::Retained))
                    .then_some((buffer.layer, content))
            })
            .collect::<HashMap<_, _>>();
        for buffer in &mut self.buffers {
            if matches!(buffer.content, SurfaceBufferContent::Retained)
                && let Some(content) = pending.remove(&buffer.layer)
            {
                buffer.content = content;
            }
        }
    }
}

/// The part of a client buffer displayed by a surface and its logical extent.
///
/// Source coordinates are physical image pixels. Destination coordinates are
/// Wayland surface-logical pixels and therefore also drive Bevy layout and
/// client-local pointer coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
#[doc(hidden)]
pub struct SurfaceContentView {
    pub source_x: f32,
    pub source_y: f32,
    pub source_width: f32,
    pub source_height: f32,
    pub logical_width: f32,
    pub logical_height: f32,
}

/// Owned input translated from the Smithay host into compositor ECS state.
#[derive(Debug)]
#[doc(hidden)]
pub struct HostSurfaceEvent {
    pub surface: SurfaceId,
    pub kind: HostSurfaceEventKind,
}

#[derive(Debug)]
#[doc(hidden)]
pub enum HostSurfaceEventKind {
    Created { decoration: WindowDecoration },
    TreeSnapshot(SurfaceTreeSnapshot),
    DecorationChanged { decoration: WindowDecoration },
    PopupConfigured(ClientPopup),
    WindowInteraction(ToplevelInteractionRequestKind),
    Destroyed,
}

#[doc(hidden)]
pub struct SurfacePlugin;

/// Stable ordering points around surface ingress and fallback presentation.
///
/// A specialized presentation plugin may run after [`Self::Ingress`] and
/// before [`Self::FallbackPresentation`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, SystemSet)]
pub enum SurfaceSystems {
    Ingress,
    FallbackPresentation,
}

impl Plugin for SurfacePlugin {
    fn build(&self, app: &mut App) {
        load_internal_asset!(
            app,
            SURFACE_MATERIAL_SHADER,
            "surface_material.wgsl",
            Shader::from_wgsl
        );
        app.add_plugins(UiMaterialPlugin::<SurfaceUiMaterial>::default());
        app.init_resource::<SurfaceEventQueue>()
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<SurfaceRegistry>()
            .init_resource::<SurfaceCommitRevisions>()
            .init_resource::<MaterialSelectorRegistry>()
            .add_message::<ToplevelInteractionRequest>()
            .configure_sets(
                PreUpdate,
                (
                    SurfaceSystems::Ingress,
                    SurfaceSystems::FallbackPresentation,
                )
                    .chain()
                    .before(PickingSystems::Backend),
            )
            // Asset change collection and UI measurement happen later in the frame.
            .add_systems(
                PreUpdate,
                apply_host_surface_events
                    .run_if(composition_advance_requested)
                    .in_set(SurfaceSystems::Ingress),
            )
            .add_systems(
                PreUpdate,
                sync_surface_nodes
                    .run_if(composition_advance_requested)
                    .after(SurfaceSystems::FallbackPresentation)
                    .before(PickingSystems::Backend),
            );
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            binding::configure_render_app(render_app);
        }
    }
}

#[derive(Resource, Default)]
pub(crate) struct SurfaceEventQueue(VecDeque<HostSurfaceEvent>);

impl SurfaceEventQueue {
    pub(crate) fn push(&mut self, event: HostSurfaceEvent) {
        let HostSurfaceEvent { surface, kind } = event;
        let kind = match kind {
            HostSurfaceEventKind::TreeSnapshot(mut snapshot) => {
                if let Some(HostSurfaceEvent {
                    surface: previous_surface,
                    kind: HostSurfaceEventKind::TreeSnapshot(previous),
                }) = self.0.back_mut()
                    && surface == *previous_surface
                {
                    snapshot.carry_pending_content_from(previous);
                    *previous = snapshot;
                    return;
                }
                HostSurfaceEventKind::TreeSnapshot(snapshot)
            }
            kind => kind,
        };
        self.0.push_back(HostSurfaceEvent { surface, kind });
    }
}

#[derive(Resource, Default)]
struct SurfaceRegistry {
    entries: HashMap<SurfaceId, SurfaceEntry>,
    pending_snapshots: HashMap<SurfaceId, SurfaceTreeSnapshot>,
}

#[derive(Resource, Default)]
struct MaterialSelectorRegistry(HashMap<AssetId<SurfaceUiMaterial>, MaterialSelector>);

#[derive(Clone, Copy)]
struct MaterialSelector {
    surface: SurfaceId,
    layer: SurfaceLayerId,
    parameters: SurfaceParameterKey,
}

struct SurfaceEntry {
    entity: Entity,
    buffers: HashMap<SurfaceLayerId, SurfaceBufferAsset>,
    frame_ready: bool,
}

/// Monotonic client-commit sequence, kept outside surface components.
#[derive(Resource, Default)]
pub struct SurfaceCommitRevisions(HashMap<SurfaceId, u64>);

impl SurfaceCommitRevisions {
    pub fn revision(&self, surface: SurfaceId) -> u64 {
        self.0.get(&surface).copied().unwrap_or_default()
    }
}

struct SurfaceBufferAsset {
    /// Stable material-facing selector for this surface layer.
    image: Handle<Image>,
    pixel_size: (u32, u32),
    opaque: bool,
    encoding: SurfaceImageEncoding,
    displayed_dmabuf: Option<SurfaceRenderImage>,
    displayed_pixel_size: Option<(u32, u32)>,
    pending_dmabuf: Option<SurfaceRenderImage>,
    generation: u64,
}

#[derive(Component, Default, PartialEq)]
struct SurfaceOverlayNodes(Vec<(SurfaceLayerId, Entity)>);

#[derive(Component)]
struct SurfaceOverlayNode;

#[derive(Component, Clone, Copy, Debug, PartialEq)]
#[doc(hidden)]
pub struct SurfaceInputNode {
    pub surface: SurfaceId,
    pub layer: SurfaceLayerId,
    pub local_origin: Vec2,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SurfaceInputNodeKey {
    layer: SurfaceLayerId,
    region: usize,
}

#[derive(Component, Default, PartialEq)]
struct SurfaceInputNodes(Vec<(SurfaceInputNodeKey, Entity)>);

#[doc(hidden)]
pub fn enqueue_surface_event(world: &mut World, event: HostSurfaceEvent) {
    let Some(mut events) = world.get_resource_mut::<SurfaceEventQueue>() else {
        warn!("discarded a surface event because the compositor ingress is unavailable");
        return;
    };
    events.push(event);
}

#[doc(hidden)]
pub fn take_surface_actions(world: &mut World) -> Vec<SurfaceAction> {
    world
        .get_resource_mut::<SurfaceActionQueue>()
        .map(|mut actions| actions.0.drain(..).collect())
        .unwrap_or_default()
}

pub(crate) fn has_surface_frame(world: &World) -> bool {
    world
        .get_resource::<SurfaceRegistry>()
        .is_some_and(|registry| registry.entries.values().any(|entry| entry.frame_ready))
}

pub(crate) fn referenced_dmabuf_ids(world: &World) -> HashSet<ImportId> {
    let Some(registry) = world.get_resource::<SurfaceRegistry>() else {
        return HashSet::new();
    };
    registry
        .entries
        .values()
        .flat_map(|entry| entry.buffers.values())
        .flat_map(|buffer| {
            buffer
                .displayed_dmabuf
                .iter()
                .chain(buffer.pending_dmabuf.iter())
                .map(|image| image.import)
        })
        .chain(
            registry
                .pending_snapshots
                .values()
                .flat_map(|snapshot| snapshot.buffers.iter())
                .filter_map(|buffer| match &buffer.content {
                    SurfaceBufferContent::RenderImage(image) => Some(image.import),
                    SurfaceBufferContent::Retained | SurfaceBufferContent::Pixels(_) => None,
                }),
        )
        .collect()
}

pub(crate) fn promote_dmabuf_sources(world: &mut World, promoted: &[ImportId]) {
    let promoted = promoted.iter().copied().collect::<HashSet<_>>();
    let Some(mut registry) = world.get_resource_mut::<SurfaceRegistry>() else {
        return;
    };
    for buffer in registry
        .entries
        .values_mut()
        .flat_map(|entry| entry.buffers.values_mut())
    {
        if buffer
            .pending_dmabuf
            .as_ref()
            .is_some_and(|image| promoted.contains(&image.import))
        {
            buffer.displayed_dmabuf = buffer.pending_dmabuf.take();
            buffer.displayed_pixel_size = Some(buffer.pixel_size);
        }
    }
    for image in registry
        .pending_snapshots
        .values_mut()
        .flat_map(|snapshot| snapshot.buffers.iter_mut())
        .filter_map(|buffer| match &mut buffer.content {
            SurfaceBufferContent::RenderImage(image) => Some(image),
            SurfaceBufferContent::Retained | SurfaceBufferContent::Pixels(_) => None,
        })
    {
        if promoted.contains(&image.import) {
            image.promoted = true;
        }
    }
}

pub(crate) fn reject_dmabuf_sources(world: &mut World, rejected: &HashSet<ImportId>) {
    let Some(mut registry) = world.get_resource_mut::<SurfaceRegistry>() else {
        return;
    };
    for buffer in registry
        .entries
        .values_mut()
        .flat_map(|entry| entry.buffers.values_mut())
    {
        if buffer
            .pending_dmabuf
            .as_ref()
            .is_some_and(|image| rejected.contains(&image.import))
        {
            buffer.pending_dmabuf = None;
            if buffer.displayed_pixel_size != Some(buffer.pixel_size) {
                buffer.displayed_dmabuf = None;
                buffer.displayed_pixel_size = None;
            }
        }
    }
    for buffer in registry
        .pending_snapshots
        .values_mut()
        .flat_map(|snapshot| snapshot.buffers.iter_mut())
    {
        let rejected_image = matches!(
            &buffer.content,
            SurfaceBufferContent::RenderImage(image)
                if rejected.contains(&image.import)
        );
        if rejected_image {
            buffer.content = SurfaceBufferContent::Retained;
        }
    }
}

fn apply_host_surface_events(world: &mut World) {
    let events = world
        .get_resource_mut::<SurfaceEventQueue>()
        .map(|mut events| std::mem::take(&mut events.0))
        .unwrap_or_default();
    let mut registry = world
        .remove_resource::<SurfaceRegistry>()
        .unwrap_or_default();

    for HostSurfaceEvent { surface, kind } in events {
        match kind {
            HostSurfaceEventKind::Created { decoration } => {
                if let Some(entity) =
                    ensure_window_entity(world, &mut registry, surface, decoration)
                {
                    set_decoration_marker(world, entity, decoration);
                    apply_pending_snapshot(world, &mut registry, surface);
                }
            }
            HostSurfaceEventKind::TreeSnapshot(snapshot) => {
                if registry.entries.contains_key(&surface) {
                    apply_surface_tree_snapshot(world, &mut registry, surface, snapshot);
                } else {
                    queue_pending_snapshot(&mut registry, surface, snapshot);
                }
            }
            HostSurfaceEventKind::DecorationChanged { decoration } => {
                set_window_decoration(world, &mut registry, surface, decoration);
            }
            HostSurfaceEventKind::PopupConfigured(popup) => {
                if ensure_popup_entity(world, &mut registry, surface, popup).is_some() {
                    apply_pending_snapshot(world, &mut registry, surface);
                }
            }
            HostSurfaceEventKind::WindowInteraction(kind) => {
                world.write_message(ToplevelInteractionRequest { surface, kind });
            }
            HostSurfaceEventKind::Destroyed => {
                destroy_surface(world, &mut registry, surface);
            }
        }
    }

    world.insert_resource(registry);
}

fn ensure_window_entity(
    world: &mut World,
    registry: &mut SurfaceRegistry,
    surface: SurfaceId,
    decoration: WindowDecoration,
) -> Option<Entity> {
    if let Some(entry) = registry.entries.get(&surface)
        && world.get_entity(entry.entity).is_ok()
    {
        if world.get::<ClientPopup>(entry.entity).is_some() {
            warn!(
                ?surface,
                "ignored an application-window role conflicting with a popup"
            );
            return None;
        }
        let Ok(mut entity) = world.get_entity_mut(entry.entity) else {
            return None;
        };
        entity.insert((ClientSurface { surface }, ClientToplevel { surface }));
        return Some(entry.entity);
    }
    registry.entries.remove(&surface);

    let entity = world
        .spawn((ClientSurface { surface }, ClientToplevel { surface }))
        .id();
    set_decoration_marker(world, entity, decoration);
    registry.entries.insert(
        surface,
        SurfaceEntry {
            entity,
            buffers: HashMap::new(),
            frame_ready: false,
        },
    );
    Some(entity)
}

fn ensure_popup_entity(
    world: &mut World,
    registry: &mut SurfaceRegistry,
    surface: SurfaceId,
    popup: ClientPopup,
) -> Option<Entity> {
    if let Some(entry) = registry.entries.get(&surface)
        && world.get_entity(entry.entity).is_ok()
    {
        if world.get::<ClientToplevel>(entry.entity).is_some() {
            warn!(
                ?surface,
                "ignored a popup role conflicting with an application window"
            );
            return None;
        }
        let Ok(mut entity) = world.get_entity_mut(entry.entity) else {
            return None;
        };
        entity.insert((ClientSurface { surface }, popup));
        return Some(entry.entity);
    }
    registry.entries.remove(&surface);

    let entity = world.spawn((ClientSurface { surface }, popup)).id();
    registry.entries.insert(
        surface,
        SurfaceEntry {
            entity,
            buffers: HashMap::new(),
            frame_ready: false,
        },
    );
    Some(entity)
}

fn queue_pending_snapshot(
    registry: &mut SurfaceRegistry,
    surface: SurfaceId,
    mut snapshot: SurfaceTreeSnapshot,
) {
    if let Some(previous) = registry.pending_snapshots.get_mut(&surface) {
        snapshot.carry_pending_content_from(previous);
        *previous = snapshot;
    } else {
        registry.pending_snapshots.insert(surface, snapshot);
    }
}

fn apply_pending_snapshot(world: &mut World, registry: &mut SurfaceRegistry, surface: SurfaceId) {
    if let Some(snapshot) = registry.pending_snapshots.remove(&surface) {
        apply_surface_tree_snapshot(world, registry, surface, snapshot);
    }
}

fn set_window_decoration(
    world: &mut World,
    registry: &mut SurfaceRegistry,
    surface: SurfaceId,
    decoration: WindowDecoration,
) {
    let Some(entry) = registry.entries.get(&surface) else {
        warn!(
            ?surface,
            "ignored a decoration update for an unknown surface"
        );
        return;
    };
    let entity = entry.entity;
    if world.get::<ClientToplevel>(entity).is_none() {
        warn!(
            ?surface,
            "ignored a window decoration update for a non-window surface"
        );
        return;
    }
    set_decoration_marker(world, entity, decoration);
}

fn set_decoration_marker(world: &mut World, entity: Entity, decoration: WindowDecoration) {
    let Ok(mut entity) = world.get_entity_mut(entity) else {
        return;
    };
    match decoration {
        WindowDecoration::ClientSide => {
            entity.remove::<ServerDecorated>();
            entity.insert(ClientDecorated);
        }
        WindowDecoration::ServerSide => {
            entity.remove::<ClientDecorated>();
            entity.insert(ServerDecorated);
        }
    }
}

fn apply_surface_tree_snapshot(
    world: &mut World,
    registry: &mut SurfaceRegistry,
    surface: SurfaceId,
    mut snapshot: SurfaceTreeSnapshot,
) {
    if !validate_snapshot(&snapshot) {
        warn!(?surface, "discarded an invalid surface tree snapshot");
        return;
    }
    let Some(entity) = registry.entries.get(&surface).map(|entry| entry.entity) else {
        queue_pending_snapshot(registry, surface, snapshot);
        return;
    };
    if let Some(mut revisions) = world.get_resource_mut::<SurfaceCommitRevisions>() {
        let revision = revisions.0.entry(surface).or_default();
        *revision = revision.saturating_add(1);
    }

    let retained = snapshot
        .buffers
        .iter()
        .map(|buffer| buffer.layer)
        .collect::<HashSet<_>>();
    let Some(entry) = registry.entries.get_mut(&surface) else {
        return;
    };
    let removed = entry
        .buffers
        .keys()
        .copied()
        .filter(|layer| !retained.contains(layer))
        .collect::<Vec<_>>();
    if let Some(mut images) = world.get_resource_mut::<Assets<Image>>() {
        for layer in removed {
            if let Some(asset) = entry.buffers.remove(&layer) {
                images.remove(asset.image.id());
            }
        }
        for buffer in &mut snapshot.buffers {
            let pixel_size = (buffer.width, buffer.height);
            let extent = Extent3d {
                width: buffer.width,
                height: buffer.height,
                depth_or_array_layers: 1,
            };
            let content = std::mem::replace(&mut buffer.content, SurfaceBufferContent::Retained);
            if let SurfaceBufferContent::Pixels(mut pixels) = content {
                let generation = entry
                    .buffers
                    .get(&buffer.layer)
                    .map_or(1, |asset| asset.generation.saturating_add(1));
                if !buffer.opaque {
                    unpremultiply_bgra(&mut pixels);
                }
                let image = if let Some(previous) = entry.buffers.get(&buffer.layer)
                    && let Some(mut image) = images.get_mut(&previous.image)
                    && image.asset_usage.contains(RenderAssetUsages::RENDER_WORLD)
                {
                    image.texture_descriptor.size = extent;
                    image.data = Some(pixels);
                    previous.image.clone()
                } else {
                    let image = images.add(surface_image(extent, pixels));
                    if let Some(previous) = entry.buffers.get(&buffer.layer) {
                        images.remove(previous.image.id());
                    }
                    image
                };
                entry.buffers.insert(
                    buffer.layer,
                    SurfaceBufferAsset {
                        image,
                        pixel_size,
                        opaque: buffer.opaque,
                        encoding: SurfaceImageEncoding::LinearStraight,
                        displayed_dmabuf: None,
                        displayed_pixel_size: None,
                        pending_dmabuf: None,
                        generation,
                    },
                );
            } else if let SurfaceBufferContent::RenderImage(render_image) = content {
                if images.get(&render_image.image).is_none() {
                    warn!(?surface, ?buffer.layer, "discarded an unknown rendered surface image");
                    continue;
                }
                let selector = entry.buffers.get(&buffer.layer).map_or_else(
                    || images.add(transparent_surface_image()),
                    |previous| previous.image.clone(),
                );
                let (mut displayed_dmabuf, mut displayed_pixel_size, generation) = entry
                    .buffers
                    .get(&buffer.layer)
                    .map_or((None, None, 0), |previous| {
                        (
                            previous.displayed_dmabuf.clone(),
                            previous.displayed_pixel_size,
                            previous.generation,
                        )
                    });
                let encoding = render_image.encoding;
                let pending_dmabuf = if render_image.promoted {
                    displayed_pixel_size = Some(pixel_size);
                    displayed_dmabuf = Some(render_image);
                    None
                } else {
                    Some(render_image)
                };
                entry.buffers.insert(
                    buffer.layer,
                    SurfaceBufferAsset {
                        image: selector,
                        pixel_size,
                        opaque: buffer.opaque,
                        encoding,
                        displayed_dmabuf,
                        displayed_pixel_size,
                        pending_dmabuf,
                        generation,
                    },
                );
            } else if let Some(asset) = entry.buffers.get_mut(&buffer.layer) {
                asset.pixel_size = pixel_size;
                asset.opaque = buffer.opaque;
            }
        }
    } else {
        warn!(
            ?surface,
            "discarded surface pixels because Bevy image assets are unavailable"
        );
        return;
    }

    let content = snapshot
        .client_mapped
        .then_some(())
        .and(snapshot.root.zip(snapshot.window_geometry))
        .and_then(|(root, window_geometry)| {
            let root_asset = entry.buffers.get(&root.layer)?;
            (validate_view(root.view, root_asset.pixel_size.0, root_asset.pixel_size.1)
                && validate_view(
                    window_geometry.view,
                    root_asset.pixel_size.0,
                    root_asset.pixel_size.1,
                ))
            .then(|| {
                let overlays = snapshot
                    .overlays
                    .iter()
                    .filter_map(|placement| {
                        let asset = entry.buffers.get(&placement.layer)?;
                        validate_view(placement.view, asset.pixel_size.0, asset.pixel_size.1).then(
                            || SurfaceLayerContent {
                                layer: placement.layer,
                                image: asset.image.clone(),
                                view: placement.view,
                                position: placement.position,
                                pixel_size: asset.pixel_size,
                            },
                        )
                    })
                    .collect();
                (
                    SurfaceContent {
                        root: SurfaceLayerContent {
                            layer: root.layer,
                            image: root_asset.image.clone(),
                            view: root.view,
                            position: Vec2::ZERO,
                            pixel_size: root_asset.pixel_size,
                        },
                        window_geometry,
                        overlays,
                        inputs: snapshot.inputs,
                    },
                    root_asset.opaque,
                )
            })
        });

    let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
        registry.entries.remove(&surface);
        warn!(
            ?surface,
            "discarded a surface snapshot because its ECS entity disappeared"
        );
        return;
    };
    if let Some((content, opaque)) = content {
        let logical_size = Vec2::new(
            content.window_geometry.view.logical_width,
            content.window_geometry.view.logical_height,
        );
        let visual_offset = -content.window_geometry.origin;
        let visual_size = Vec2::new(
            content.root.view.logical_width,
            content.root.view.logical_height,
        );
        let mapped = MappedSurface {
            logical_size,
            visual_offset,
            visual_size,
            opaque,
        };
        let content_changed = entity_mut.get::<SurfaceContent>() != Some(&content);
        let mapping_changed = entity_mut.get::<MappedSurface>() != Some(&mapped);
        if content_changed {
            entity_mut.insert(content);
        }
        if mapping_changed {
            entity_mut.insert(mapped);
        }
        entry.frame_ready = true;
        if content_changed || mapping_changed {
            tracing::trace!(
                target: crate::PROFILE_TARGET,
                surface = surface.raw(),
                logical_width = logical_size.x,
                logical_height = logical_size.y,
                visual_offset_x = visual_offset.x,
                visual_offset_y = visual_offset.y,
                visual_width = visual_size.x,
                visual_height = visual_size.y,
                "mapped structural surface state in ECS"
            );
        }
    } else {
        let was_mapped =
            entity_mut.contains::<SurfaceContent>() || entity_mut.contains::<MappedSurface>();
        if was_mapped {
            entity_mut.remove::<(SurfaceContent, MappedSurface)>();
        }
        entry.frame_ready = false;
        if was_mapped {
            tracing::trace!(
                target: crate::PROFILE_TARGET,
                surface = surface.raw(),
                "unmapped structural surface state in ECS"
            );
        }
    }
}

fn destroy_surface(world: &mut World, registry: &mut SurfaceRegistry, surface: SurfaceId) {
    registry.pending_snapshots.remove(&surface);
    if let Some(mut revisions) = world.get_resource_mut::<SurfaceCommitRevisions>() {
        revisions.0.remove(&surface);
    }
    let Some(entry) = registry.entries.remove(&surface) else {
        return;
    };
    if let Some(mut images) = world.get_resource_mut::<Assets<Image>>() {
        for asset in entry.buffers.into_values() {
            images.remove(asset.image.id());
        }
    }
    if let Ok(entity) = world.get_entity_mut(entry.entity) {
        entity.despawn();
    }
}

type SurfaceNodeQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        Entity,
        &'static SurfaceNode,
        &'static mut MaterialNode<SurfaceUiMaterial>,
        &'static mut Node,
        Option<&'static SurfaceOverlayNodes>,
        Option<&'static SurfaceInputNodes>,
        Option<&'static Children>,
    ),
    (Without<SurfaceOverlayNode>, Without<SurfaceInputNode>),
>;
type SurfaceOverlayNodeQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        &'static mut MaterialNode<SurfaceUiMaterial>,
        &'static mut Node,
    ),
    (With<SurfaceOverlayNode>, Without<SurfaceInputNode>),
>;
type SurfaceInputNodeQuery<'world, 'state> = Query<
    'world,
    'state,
    (&'static mut SurfaceInputNode, &'static mut Node),
    (With<SurfaceInputNode>, Without<SurfaceOverlayNode>),
>;

#[derive(SystemParam)]
struct SyncSurfaceNodesParams<'w, 's> {
    commands: bevy::ecs::system::Commands<'w, 's>,
    materials: ResMut<'w, Assets<SurfaceUiMaterial>>,
    surfaces: Query<'w, 's, (&'static ClientSurface, &'static SurfaceContent)>,
    nodes: SurfaceNodeQuery<'w, 's>,
    overlay_nodes: SurfaceOverlayNodeQuery<'w, 's>,
    input_nodes: SurfaceInputNodeQuery<'w, 's>,
    registry: bevy::ecs::system::Res<'w, SurfaceRegistry>,
    selector_registry: ResMut<'w, MaterialSelectorRegistry>,
}

fn sync_surface_nodes(params: SyncSurfaceNodesParams) {
    let SyncSurfaceNodesParams {
        mut commands,
        mut materials,
        surfaces,
        mut nodes,
        mut overlay_nodes,
        mut input_nodes,
        registry,
        mut selector_registry,
    } = params;
    for (
        entity,
        surface_node,
        mut material_node,
        mut node,
        existing_overlays,
        existing_inputs,
        existing_children,
    ) in &mut nodes
    {
        let content = surfaces.iter().find_map(|(surface, content)| {
            (surface.surface == surface_node.surface).then_some(content)
        });
        let Some(content) = content else {
            if node.display != Display::None {
                node.display = Display::None;
                node.width = Val::Auto;
                node.height = Val::Auto;
            }
            clear_surface_material(&mut materials, &mut selector_registry, &mut material_node);
            if let Some(existing) = existing_overlays {
                for (_, overlay) in &existing.0 {
                    if let Ok((mut overlay_material, _)) = overlay_nodes.get_mut(*overlay) {
                        clear_surface_material(
                            &mut materials,
                            &mut selector_registry,
                            &mut overlay_material,
                        );
                    }
                    commands.entity(*overlay).despawn();
                }
                commands.entity(entity).remove::<SurfaceOverlayNodes>();
            }
            if let Some(existing) = existing_inputs {
                for (_, input) in &existing.0 {
                    commands.entity(*input).despawn();
                }
                commands.entity(entity).remove::<SurfaceInputNodes>();
            }
            continue;
        };

        let (root_view, coordinate_origin) = match surface_node.view {
            SurfaceView::FullSurface => (content.root.view, Vec2::ZERO),
            SurfaceView::WindowGeometry => {
                (content.window_geometry.view, content.window_geometry.origin)
            }
        };
        let Some(entry) = registry.entries.get(&surface_node.surface) else {
            continue;
        };
        let Some(root_buffer) = entry.buffers.get(&content.root.layer) else {
            continue;
        };
        let expected_material = surface_material(&content.root, root_view, root_buffer);
        update_surface_material(
            &mut materials,
            &mut selector_registry,
            &mut material_node,
            expected_material,
            surface_node.surface,
            content.root.layer,
        );
        let logical_width = px(root_view.logical_width);
        let logical_height = px(root_view.logical_height);
        if node.display != Display::Flex
            || node.width != logical_width
            || node.height != logical_height
        {
            node.display = Display::Flex;
            node.width = logical_width;
            node.height = logical_height;
            tracing::trace!(
                target: crate::PROFILE_TARGET,
                surface = surface_node.surface.raw(),
                width = root_view.logical_width,
                height = root_view.logical_height,
                "made surface content node visible"
            );
        }

        let mut reusable = existing_overlays
            .map(|overlays| overlays.0.iter().copied().collect::<HashMap<_, _>>())
            .unwrap_or_default();
        let mut ordered = Vec::with_capacity(content.overlays.len());
        let mut tracked = Vec::with_capacity(content.overlays.len());
        for overlay in &content.overlays {
            let Some(overlay_buffer) = entry.buffers.get(&overlay.layer) else {
                continue;
            };
            let expected_material = surface_material(overlay, overlay.view, overlay_buffer);
            let expected_node = overlay_node(overlay, coordinate_origin);
            let overlay_entity = if let Some(overlay_entity) = reusable.remove(&overlay.layer) {
                if let Ok((mut material_node, mut node)) = overlay_nodes.get_mut(overlay_entity) {
                    update_surface_material(
                        &mut materials,
                        &mut selector_registry,
                        &mut material_node,
                        expected_material,
                        surface_node.surface,
                        overlay.layer,
                    );
                    if *node != expected_node {
                        *node = expected_node;
                    }
                }
                overlay_entity
            } else {
                let material = materials.add(expected_material.clone());
                selector_registry.0.insert(
                    material.id(),
                    MaterialSelector {
                        surface: surface_node.surface,
                        layer: overlay.layer,
                        parameters: expected_material.parameters.into(),
                    },
                );
                commands
                    .spawn((
                        SurfaceOverlayNode,
                        Pickable::IGNORE,
                        MaterialNode(material),
                        expected_node,
                        ChildOf(entity),
                    ))
                    .id()
            };
            ordered.push(overlay_entity);
            tracked.push((overlay.layer, overlay_entity));
        }
        for overlay in reusable.into_values() {
            if let Ok((mut material_node, _)) = overlay_nodes.get_mut(overlay) {
                clear_surface_material(&mut materials, &mut selector_registry, &mut material_node);
            }
            commands.entity(overlay).despawn();
        }

        let mut reusable_inputs = existing_inputs
            .map(|inputs| inputs.0.iter().copied().collect::<HashMap<_, _>>())
            .unwrap_or_default();
        let input_capacity = content
            .inputs
            .iter()
            .map(|input| input.regions.len())
            .sum::<usize>();
        let mut tracked_inputs = Vec::with_capacity(input_capacity);
        for input in &content.inputs {
            for (region, rectangle) in input.regions.iter().enumerate() {
                let key = SurfaceInputNodeKey {
                    layer: input.layer,
                    region,
                };
                let expected_target = SurfaceInputNode {
                    surface: surface_node.surface,
                    layer: input.layer,
                    local_origin: rectangle.position,
                };
                let expected_node = input_node(input.position - coordinate_origin, *rectangle);
                let input_entity = if let Some(input_entity) = reusable_inputs.remove(&key) {
                    if let Ok((mut target, mut node)) = input_nodes.get_mut(input_entity) {
                        if *target != expected_target {
                            *target = expected_target;
                        }
                        if *node != expected_node {
                            *node = expected_node;
                        }
                    }
                    input_entity
                } else {
                    commands
                        .spawn((
                            expected_target,
                            Pickable::default(),
                            LayoutConfig { use_rounding: true },
                            expected_node,
                            ChildOf(entity),
                        ))
                        .id()
                };
                ordered.push(input_entity);
                tracked_inputs.push((key, input_entity));
            }
        }
        for input in reusable_inputs.into_values() {
            commands.entity(input).despawn();
        }
        if existing_children
            .is_none_or(|children| !children.iter().copied().eq(ordered.iter().copied()))
        {
            commands.entity(entity).replace_children(&ordered);
        }
        if existing_overlays.map(|nodes| &nodes.0) != Some(&tracked) {
            commands.entity(entity).insert(SurfaceOverlayNodes(tracked));
        }
        if existing_inputs.map(|nodes| &nodes.0) != Some(&tracked_inputs) {
            commands
                .entity(entity)
                .insert(SurfaceInputNodes(tracked_inputs));
        }
    }
}

fn input_node(layer_position: Vec2, rectangle: SurfaceInputRect) -> Node {
    Node {
        position_type: PositionType::Absolute,
        left: px(layer_position.x + rectangle.position.x),
        top: px(layer_position.y + rectangle.position.y),
        width: px(rectangle.size.x),
        height: px(rectangle.size.y),
        ..Default::default()
    }
}

fn overlay_node(layer: &SurfaceLayerContent, coordinate_origin: Vec2) -> Node {
    Node {
        position_type: PositionType::Absolute,
        left: px(layer.position.x - coordinate_origin.x),
        top: px(layer.position.y - coordinate_origin.y),
        width: px(layer.view.logical_width),
        height: px(layer.view.logical_height),
        ..Default::default()
    }
}

fn validate_snapshot(snapshot: &SurfaceTreeSnapshot) -> bool {
    let mut layers = HashSet::new();
    let valid_buffers = snapshot.buffers.iter().all(|buffer| {
        layers.insert(buffer.layer)
            && buffer.width > 0
            && buffer.height > 0
            && match &buffer.content {
                SurfaceBufferContent::Pixels(pixels) => {
                    buffer
                        .width
                        .checked_mul(buffer.height)
                        .and_then(|count| count.checked_mul(4))
                        .and_then(|bytes| usize::try_from(bytes).ok())
                        == Some(pixels.len())
                }
                SurfaceBufferContent::Retained | SurfaceBufferContent::RenderImage(_) => true,
            }
    });
    let valid_geometry = match (snapshot.root, snapshot.window_geometry) {
        (Some(_), Some(geometry)) => geometry.origin.is_finite(),
        (None, None) => true,
        _ => false,
    };
    valid_buffers
        && valid_geometry
        && snapshot.inputs.iter().all(|input| {
            layers.contains(&input.layer)
                && input.position.is_finite()
                && input.regions.iter().all(|region| {
                    region.position.is_finite()
                        && region.size.is_finite()
                        && region.size.x > 0.0
                        && region.size.y > 0.0
                })
        })
}

fn validate_view(view: SurfaceContentView, width: u32, height: u32) -> bool {
    let values = [
        view.source_x,
        view.source_y,
        view.source_width,
        view.source_height,
        view.logical_width,
        view.logical_height,
    ];
    values.into_iter().all(f32::is_finite)
        && view.source_x >= 0.0
        && view.source_y >= 0.0
        && view.source_width > 0.0
        && view.source_height > 0.0
        && view.logical_width > 0.0
        && view.logical_height > 0.0
        && view.source_x + view.source_width <= width as f32
        && view.source_y + view.source_height <= height as f32
}

fn surface_material(
    layer: &SurfaceLayerContent,
    view: SurfaceContentView,
    buffer: &SurfaceBufferAsset,
) -> SurfaceUiMaterial {
    let (source_encoding, y_inverted) = if let Some(displayed) = &buffer.displayed_dmabuf {
        (displayed.encoding, displayed.y_inverted)
    } else if buffer.encoding == SurfaceImageEncoding::LinearStraight {
        (SurfaceImageEncoding::LinearStraight, false)
    } else {
        (SurfaceImageEncoding::Unbound, false)
    };
    let encoding = encoding_flag(source_encoding);
    SurfaceUiMaterial {
        image: layer.image.clone(),
        parameters: SurfaceMaterialParameters {
            source_rect: Vec4::new(
                view.source_x,
                view.source_y,
                view.source_width,
                view.source_height,
            ),
            buffer_size: Vec2::new(layer.pixel_size.0 as f32, layer.pixel_size.1 as f32),
            flags: UVec2::new(encoding, u32::from(y_inverted)),
        },
    }
}

const fn encoding_flag(encoding: SurfaceImageEncoding) -> u32 {
    match encoding {
        SurfaceImageEncoding::Unbound => 3,
        SurfaceImageEncoding::LinearStraight => 0,
        SurfaceImageEncoding::EncodedPremultiplied => 1,
        SurfaceImageEncoding::EncodedOpaque => 2,
    }
}

fn update_surface_material(
    materials: &mut Assets<SurfaceUiMaterial>,
    selectors: &mut MaterialSelectorRegistry,
    node: &mut MaterialNode<SurfaceUiMaterial>,
    expected: SurfaceUiMaterial,
    surface: SurfaceId,
    layer: SurfaceLayerId,
) {
    let parameters = expected.parameters.into();
    if let Some(mut material) = materials.get_mut(&node.0) {
        if *material != expected {
            *material = expected;
        }
    } else {
        selectors.0.remove(&node.0.id());
        node.0 = materials.add(expected);
    }
    selectors.0.insert(
        node.0.id(),
        MaterialSelector {
            surface,
            layer,
            parameters,
        },
    );
}

fn clear_surface_material(
    materials: &mut Assets<SurfaceUiMaterial>,
    selectors: &mut MaterialSelectorRegistry,
    node: &mut MaterialNode<SurfaceUiMaterial>,
) {
    if node.0 != Handle::default() {
        selectors.0.remove(&node.0.id());
        materials.remove(node.0.id());
        node.0 = Handle::default();
    }
}

fn surface_image(extent: Extent3d, pixels: Vec<u8>) -> Image {
    Image::new(
        extent,
        TextureDimension::D2,
        pixels,
        TextureFormat::Bgra8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

fn transparent_surface_image() -> Image {
    surface_image(
        Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        vec![0, 0, 0, 0],
    )
}

/// Converts encoded premultiplied BGRA channels to straight alpha in place.
///
/// Bevy UI uses straight-alpha blending. Dividing before the sRGB sampler
/// decode preserves the current linear premultiplied result, at the cost of
/// unavoidable quantization for very small alpha values.
fn unpremultiply_bgra(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
            continue;
        }
        for channel in &mut pixel[..3] {
            let straight = (u32::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = straight.min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy::{app::App, asset::AssetApp, ecs::change_detection::DetectChanges};

    use crate::composition::{CompositionPlugin, set_composition_advance};

    use super::*;

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins((
            bevy::app::TaskPoolPlugin::default(),
            bevy::asset::AssetPlugin::default(),
        ));
        app.init_asset::<Shader>()
            .insert_resource(Assets::<Image>::default())
            .add_plugins((CompositionPlugin, SurfacePlugin));
        app
    }

    fn full_view(width: f32, height: f32) -> SurfaceContentView {
        SurfaceContentView {
            source_x: 0.0,
            source_y: 0.0,
            source_width: width,
            source_height: height,
            logical_width: width,
            logical_height: height,
        }
    }

    fn placement(layer: u64, position: Vec2) -> SurfaceLayerPlacement {
        SurfaceLayerPlacement {
            layer: SurfaceLayerId::new(layer),
            position,
            view: full_view(1.0, 1.0),
        }
    }

    fn buffer(layer: u64, pixel: Option<[u8; 4]>) -> SurfaceBufferUpdate {
        SurfaceBufferUpdate {
            layer: SurfaceLayerId::new(layer),
            width: 1,
            height: 1,
            content: pixel
                .map(Vec::from)
                .map(SurfaceBufferContent::Pixels)
                .unwrap_or(SurfaceBufferContent::Retained),
            opaque: true,
        }
    }

    fn root_snapshot(pixel: Option<[u8; 4]>) -> SurfaceTreeSnapshot {
        SurfaceTreeSnapshot {
            client_mapped: true,
            root: Some(placement(1, Vec2::ZERO)),
            window_geometry: Some(SurfaceWindowGeometry {
                origin: Vec2::ZERO,
                view: full_view(1.0, 1.0),
            }),
            overlays: Vec::new(),
            inputs: Vec::new(),
            buffers: vec![buffer(1, pixel)],
        }
    }

    fn snapshot_event(surface: SurfaceId, snapshot: SurfaceTreeSnapshot) -> HostSurfaceEvent {
        HostSurfaceEvent {
            surface,
            kind: HostSurfaceEventKind::TreeSnapshot(snapshot),
        }
    }

    fn register_window(app: &mut App, surface: SurfaceId) {
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::Created {
                    decoration: WindowDecoration::ClientSide,
                },
            },
        );
    }

    #[test]
    fn root_updates_reuse_the_bevy_image() {
        let mut app = test_app();
        let surface = SurfaceId::new(7);
        register_window(&mut app, surface);
        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([3, 2, 1, 255]))),
        );
        app.update();
        let first_handle = {
            let mut query = app.world_mut().query::<&SurfaceContent>();
            query
                .single(app.world())
                .expect("root image should exist")
                .root
                .image
                .clone()
        };

        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([7, 6, 5, 255]))),
        );
        app.update();
        let mut query = app.world_mut().query::<&SurfaceContent>();
        let content = query
            .single(app.world())
            .expect("updated root should exist");
        assert_eq!(content.root.image, first_handle);
    }

    #[test]
    fn a_snapshot_waits_for_its_popup_role_without_becoming_a_window() {
        let mut app = test_app();
        let surface = SurfaceId::new(8);
        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([3, 2, 1, 255]))),
        );
        app.update();

        let mut windows = app.world_mut().query::<&ClientToplevel>();
        assert_eq!(windows.iter(app.world()).count(), 0);

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::PopupConfigured(ClientPopup {
                    owner: SurfaceId::new(1),
                    position: Vec2::new(10.0, 20.0),
                    stack_index: 1,
                }),
            },
        );
        app.update();

        let mut popups = app
            .world_mut()
            .query::<(&ClientPopup, &MappedSurface, Option<&ClientDecorated>)>();
        let (popup, mapped, decoration) = popups
            .single(app.world())
            .expect("the queued popup snapshot should map after role registration");
        assert_eq!(popup.owner, SurfaceId::new(1));
        assert_eq!(mapped.logical_size, Vec2::ONE);
        assert!(decoration.is_none());
    }

    #[test]
    fn protocol_unmap_retains_copied_buffers_for_remapping() {
        let mut app = test_app();
        let surface = SurfaceId::new(11);
        register_window(&mut app, surface);
        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([3, 2, 1, 255]))),
        );
        app.update();
        let first_handle = {
            let mut query = app.world_mut().query::<&SurfaceContent>();
            query
                .single(app.world())
                .expect("mapped root should exist")
                .root
                .image
                .clone()
        };

        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(
                surface,
                SurfaceTreeSnapshot {
                    client_mapped: false,
                    root: None,
                    window_geometry: None,
                    overlays: Vec::new(),
                    inputs: Vec::new(),
                    buffers: vec![buffer(1, None)],
                },
            ),
        );
        app.update();
        assert!(!has_surface_frame(app.world()));
        assert!(
            app.world()
                .resource::<Assets<Image>>()
                .get(&first_handle)
                .is_some()
        );

        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(None)),
        );
        app.update();
        let mut query = app.world_mut().query::<&SurfaceContent>();
        let remapped = query.single(app.world()).expect("root should remap");
        assert_eq!(remapped.root.image, first_handle);
    }

    #[test]
    fn coalescing_preserves_unseen_pixels_and_drops_removed_layers() {
        let surface = SurfaceId::new(1);
        let mut events = SurfaceEventQueue::default();
        let mut first = root_snapshot(Some([1, 1, 1, 255]));
        first.buffers.push(buffer(2, Some([2, 2, 2, 255])));
        first.overlays.push(placement(2, Vec2::ZERO));
        events.push(snapshot_event(surface, first));

        let mut next = root_snapshot(None);
        next.buffers.push(buffer(3, Some([3, 3, 3, 255])));
        next.overlays.push(placement(3, Vec2::ZERO));
        events.push(snapshot_event(surface, next));

        let Some(HostSurfaceEvent {
            kind: HostSurfaceEventKind::TreeSnapshot(snapshot),
            ..
        }) = events.0.front()
        else {
            panic!("adjacent snapshots should merge");
        };
        assert_eq!(events.0.len(), 1);
        assert_eq!(snapshot.buffers.len(), 2);
        assert_eq!(
            match &snapshot.buffers[0].content {
                SurfaceBufferContent::Pixels(pixels) => Some(pixels.as_slice()),
                _ => None,
            },
            Some([1, 1, 1, 255].as_slice())
        );
        assert_eq!(snapshot.buffers[1].layer, SurfaceLayerId::new(3));
    }

    #[test]
    fn coalescing_preserves_an_unseen_imported_render_image() {
        let surface = SurfaceId::new(1);
        let image = SurfaceRenderImage {
            import: ImportId::for_test(7),
            image: Handle::default(),
            encoding: SurfaceImageEncoding::EncodedPremultiplied,
            y_inverted: true,
            promoted: false,
        };
        let mut first = root_snapshot(None);
        first.buffers[0].content = SurfaceBufferContent::RenderImage(image.clone());
        let mut events = SurfaceEventQueue::default();
        events.push(snapshot_event(surface, first));
        events.push(snapshot_event(surface, root_snapshot(None)));

        let Some(HostSurfaceEvent {
            kind: HostSurfaceEventKind::TreeSnapshot(snapshot),
            ..
        }) = events.0.front()
        else {
            panic!("adjacent snapshots should merge");
        };
        let SurfaceBufferContent::RenderImage(carried) = &snapshot.buffers[0].content else {
            panic!("the pending imported image should survive a retained update");
        };
        assert_eq!(carried, &image);
    }

    #[test]
    fn snapshot_validation_accepts_imported_images_but_checks_copied_pixel_lengths() {
        let mut imported = root_snapshot(None);
        imported.buffers[0].content = SurfaceBufferContent::RenderImage(SurfaceRenderImage {
            import: ImportId::for_test(8),
            image: Handle::default(),
            encoding: SurfaceImageEncoding::EncodedOpaque,
            y_inverted: false,
            promoted: false,
        });
        assert!(validate_snapshot(&imported));

        let mut malformed_copy = root_snapshot(None);
        malformed_copy.buffers[0].content = SurfaceBufferContent::Pixels(vec![0; 3]);
        assert!(!validate_snapshot(&malformed_copy));
    }

    #[test]
    fn a_pre_role_snapshot_keeps_its_dma_import_referenced() {
        let mut app = test_app();
        let surface = SurfaceId::new(31);
        let import = ImportId::for_test(11);
        let mut snapshot = root_snapshot(None);
        snapshot.buffers[0].content = SurfaceBufferContent::RenderImage(SurfaceRenderImage {
            import,
            image: Handle::default(),
            encoding: SurfaceImageEncoding::EncodedOpaque,
            y_inverted: false,
            promoted: false,
        });
        enqueue_surface_event(app.world_mut(), snapshot_event(surface, snapshot));
        app.update();

        assert_eq!(referenced_dmabuf_ids(app.world()), HashSet::from([import]));
    }

    #[test]
    fn a_content_only_commit_advances_commit_state_without_changing_surface_components() {
        let mut app = test_app();
        let surface = SurfaceId::new(32);
        register_window(&mut app, surface);
        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([1, 2, 3, 255]))),
        );
        app.update();
        let entity = app
            .world_mut()
            .query_filtered::<Entity, With<ClientSurface>>()
            .single(app.world())
            .expect("surface should exist");
        let content_tick = app
            .world()
            .entity(entity)
            .get_ref::<SurfaceContent>()
            .expect("surface content should exist")
            .last_changed();
        let mapped_tick = app
            .world()
            .entity(entity)
            .get_ref::<MappedSurface>()
            .expect("mapped state should exist")
            .last_changed();
        let revision = app
            .world()
            .resource::<SurfaceCommitRevisions>()
            .revision(surface);

        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([4, 5, 6, 255]))),
        );
        app.update();

        assert_eq!(
            app.world()
                .entity(entity)
                .get_ref::<SurfaceContent>()
                .expect("surface content should remain")
                .last_changed(),
            content_tick
        );
        assert_eq!(
            app.world()
                .entity(entity)
                .get_ref::<MappedSurface>()
                .expect("mapped state should remain")
                .last_changed(),
            mapped_tick
        );
        assert_eq!(
            app.world()
                .resource::<SurfaceCommitRevisions>()
                .revision(surface),
            revision + 1
        );
    }

    #[test]
    fn input_only_advances_keep_latest_tree_pixels_queued() {
        let mut app = test_app();
        let surface = SurfaceId::new(5);
        set_composition_advance(app.world_mut(), false);
        register_window(&mut app, surface);
        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([1, 2, 3, 255]))),
        );
        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([4, 5, 6, 255]))),
        );
        app.update();
        app.update();
        let mut mapped = app.world_mut().query::<&MappedSurface>();
        assert_eq!(mapped.iter(app.world()).count(), 0);

        set_composition_advance(app.world_mut(), true);
        app.update();
        let mut content = app.world_mut().query::<&SurfaceContent>();
        let content = content
            .single(app.world())
            .expect("queued root should compose");
        let image = app
            .world()
            .resource::<Assets<Image>>()
            .get(&content.root.image)
            .expect("root asset should exist");
        assert_eq!(image.data.as_deref(), Some([4, 5, 6, 255].as_slice()));
    }

    #[test]
    fn overlays_reuse_entities_and_release_removed_assets() {
        let mut app = test_app();
        let surface = SurfaceId::new(29);
        register_window(&mut app, surface);
        app.world_mut().spawn((
            SurfaceNode {
                surface,
                view: SurfaceView::WindowGeometry,
            },
            Node::default(),
        ));
        let mut snapshot = root_snapshot(Some([1, 1, 1, 255]));
        snapshot.buffers.push(buffer(2, Some([2, 2, 2, 255])));
        snapshot.overlays.push(placement(2, Vec2::new(4.0, 5.0)));
        enqueue_surface_event(app.world_mut(), snapshot_event(surface, snapshot));
        app.update();

        let first_overlay = {
            let mut query = app
                .world_mut()
                .query_filtered::<Entity, With<SurfaceOverlayNode>>();
            query.single(app.world()).expect("overlay should exist")
        };
        let overlay_image = app
            .world()
            .get::<MaterialNode<SurfaceUiMaterial>>(first_overlay)
            .and_then(|node| {
                app.world()
                    .resource::<Assets<SurfaceUiMaterial>>()
                    .get(&node.0)
            })
            .expect("overlay should own a surface material")
            .image
            .clone();

        let mut moved = root_snapshot(None);
        moved.buffers.push(buffer(2, None));
        moved.overlays.push(placement(2, Vec2::new(8.0, 9.0)));
        enqueue_surface_event(app.world_mut(), snapshot_event(surface, moved));
        app.update();
        let mut query = app
            .world_mut()
            .query_filtered::<Entity, With<SurfaceOverlayNode>>();
        assert_eq!(
            query.single(app.world()).expect("overlay should be reused"),
            first_overlay
        );

        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(None)),
        );
        app.update();
        let mut query = app
            .world_mut()
            .query_filtered::<Entity, With<SurfaceOverlayNode>>();
        assert_eq!(query.iter(app.world()).count(), 0);
        assert!(
            app.world()
                .resource::<Assets<Image>>()
                .get(&overlay_image)
                .is_none()
        );
    }

    #[test]
    fn converts_premultiplied_bgra_to_straight_alpha() {
        let mut pixels = [25, 50, 75, 128, 9, 8, 7, 0, 3, 2, 1, 255];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels, [50, 100, 149, 128, 0, 0, 0, 0, 3, 2, 1, 255]);
    }
}

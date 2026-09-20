//! Godot projection of the existing receiver's surface hierarchy. A view owns
//! presentation objects, never its own transport, decoder pool or input seat.
use super::WeldVideoPlayer;
use super::canvas::Layout;
use super::decoration::{Clip, Decoration, Shape};
use super::overlap::{self, Canvas, Plane};
use super::placement::Placement;
use super::stacking::{Layer, Stack};
use super::stereo::ViewLayout;
use super::xr::controls::{Bar, Part};
use crate::{
    playback::{
        Controller,
        session::{Pane, Session},
    },
    presentation::XrPreferences,
};
use anyhow::Result;
use godot::{
    classes::{
        Control, ExternalTexture, IRefCounted, MeshInstance3D, Os, Shader, ShaderMaterial,
        SubViewport,
    },
    prelude::*,
};
use std::collections::BTreeMap;

#[derive(GodotClass)]
#[class(base=RefCounted, no_init)]
pub struct WeldSurface {
    pane: Pane,
    pub(super) player: Gd<WeldVideoPlayer>,
    _texture: Gd<ExternalTexture>,
    material: Gd<ShaderMaterial>,
    right_material: Option<Gd<ShaderMaterial>>,
    pub(super) control: Option<Gd<Control>>,
    right_control: Option<Gd<Control>>,
    outputs: Vec<Gd<Control>>,
    pub(super) panel: Option<Gd<MeshInstance3D>>,
    decoration: Option<Decoration>,
    bar: Option<Bar>,
    placement: Placement,
    presentation_order: i32,
    input_order: i32,
    canvas_layout: Option<Layout>,
    overlap: Vec<overlap::Pass>,
}

#[godot_api]
impl IRefCounted for WeldSurface {}

#[godot_api]
impl WeldSurface {
    #[func]
    fn title(&self) -> GString {
        self.pane.metadata.title().into()
    }
    #[func]
    fn app_id(&self) -> GString {
        self.pane.metadata.app_id().into()
    }
    #[func]
    fn panel_slot(&self) -> i32 {
        self.pane.panel_slot().map_or(-1, |slot| slot as i32)
    }
    /// Adapter-scoped Wayland client identity, not an inferred parent or app title.
    #[func]
    fn application_key(&self) -> GString {
        format!(
            "{}:{}",
            self.pane.client.source().raw(),
            self.pane.client.local()
        )
        .as_str()
        .into()
    }
    #[func]
    fn surface_id(&self) -> i64 {
        self.pane.id as i64
    }
    #[func]
    fn window_id(&self) -> i64 {
        self.pane.window as i64
    }
    #[func]
    fn parent_id(&self) -> i64 {
        self.pane.parent as i64
    }
    #[func]
    fn kind(&self) -> i32 {
        self.pane.kind
    }
    #[func]
    fn stack_index(&self) -> i32 {
        self.pane.stack
    }
    #[func]
    fn logical_position(&self) -> Vector2 {
        let x = if self.is_stereo() {
            self.pane.position[0] * 0.5
        } else {
            self.pane.position[0]
        };
        Vector2::new(x, self.pane.position[1])
    }
    #[func]
    fn logical_size(&self) -> Vector2 {
        if let Some(controller) = self
            .player
            .bind()
            .controller
            .as_ref()
            .filter(|controller| controller.has_frame())
        {
            let size = controller.logical_size();
            return Vector2::new(size[0] as f32, size[1] as f32);
        }
        let x = if self.is_stereo() {
            self.pane.size[0] * 0.5
        } else {
            self.pane.size[0]
        };
        Vector2::new(x, self.pane.size[1])
    }
    #[func]
    pub(super) fn is_mapped(&self) -> bool {
        self.pane.visible
            && self
                .player
                .bind()
                .controller
                .as_ref()
                .is_some_and(Controller::has_frame)
    }
    #[func]
    fn video_material(&self) -> Gd<ShaderMaterial> {
        self.material.clone()
    }
    /// Select packed full-width stereo before mounting the panel. Layout
    /// changes never create a second decoder or independently select a frame.
    #[func]
    fn enable_side_by_side(&mut self) -> bool {
        if self.right_material.is_some() {
            return true;
        }
        if self.panel.is_some() || self.pane.kind == 2 {
            return false;
        }
        let Some(shader) = self.material.get_shader() else {
            return false;
        };
        let mut right = ShaderMaterial::new_gd();
        right.set_shader(&shader);
        let mut player = self.player.bind_mut();
        let Some(controller) = player.controller.as_mut() else {
            return false;
        };
        if let Err(error) = controller.attach_stereo_material(right.clone().upcast()) {
            godot_error!("Could not attach stereo view: {error:#}");
            return false;
        }
        let layout = ViewLayout::SideBySide;
        self.material
            .set_shader_parameter("eye_view", &layout.eye_view(false).to_variant());
        right.set_shader_parameter("eye_view", &layout.eye_view(true).to_variant());
        player.view_layout = layout;
        self.right_material = Some(right);
        true
    }
    #[func]
    fn right_eye_material(&self) -> Option<Gd<ShaderMaterial>> {
        self.right_material.clone()
    }
    #[func]
    fn is_stereo(&self) -> bool {
        self.right_material.is_some()
    }
    #[func]
    fn video_player(&self) -> Gd<WeldVideoPlayer> {
        self.player.clone()
    }
    #[func]
    fn bind_control(&mut self, control: Gd<Control>) {
        self.control = Some(control);
    }
    #[func]
    fn bind_panel(&mut self, panel: Gd<MeshInstance3D>, right_control: Option<Gd<Control>>) {
        self.panel = Some(panel);
        self.right_control = right_control;
    }
    #[func]
    fn bind_outputs(&mut self, left: Gd<Control>, right: Gd<Control>) {
        self.outputs = vec![left, right];
    }
    #[func]
    fn style_panel(&mut self, physical_size: Vector2, owner: Option<Gd<WeldSurface>>) {
        if self.pane.kind == 3 {
            if let Some(mut owner) = owner {
                self.style_content(physical_size, &mut owner.bind_mut());
            }
            return;
        }
        let Some(shape) = Shape::new(physical_size) else {
            return;
        };
        self.apply_clip(Clip::full(shape));
    }
    /// Lay out the complete native canvas while retaining a content-only hit plane.
    #[func]
    fn layout_canvas(&mut self, physical: Vector2, raster: Vector2i) -> Vector2 {
        let Some(layout) = Layout::new(physical, raster, self.pane.kind != 3) else {
            return physical;
        };
        let views: Vec<_> = [self.control.clone(), self.right_control.clone()]
            .into_iter()
            .flatten()
            .collect();
        for view in &views {
            if let Some(mut viewport) = view
                .get_viewport()
                .and_then(|v| v.try_cast::<SubViewport>().ok())
                && viewport.get_size() != layout.raster
            {
                viewport.set_size(layout.raster);
            }
            let mut view = view.clone();
            view.set_position(layout.content.position);
            view.set_size(layout.content.size);
        }
        if self.pane.kind != 3
            && let Some(shape) = Shape::new(physical)
        {
            let decoration = self
                .decoration
                .get_or_insert_with(|| Decoration::new(&views));
            decoration.update(shape, layout);
            decoration.set_focused(
                self.player
                    .bind()
                    .controller
                    .as_ref()
                    .is_some_and(Controller::is_focused),
            );
            if self.pane.kind <= 1
                && let Some(panel) = &self.panel
            {
                let bar = self
                    .bar
                    .get_or_insert_with(|| Bar::new(panel.clone(), &views));
                bar.layout(layout);
                bar.place(physical.y);
            }
        }
        self.canvas_layout = Some(layout);
        layout.physical
    }
    #[func]
    fn stacking_order(&self) -> i32 {
        self.presentation_order
    }
    #[func]
    fn placed_transform(
        &mut self,
        base: Transform3D,
        center: Vector3,
        orientation_pivot: Vector3,
    ) -> Transform3D {
        self.placement.apply(base, center, orientation_pivot)
    }
}

impl WeldSurface {
    fn canvas(&self) -> Option<Canvas> {
        let panel = self.panel.as_ref()?;
        let layout = self.canvas_layout?;
        let left = self.control.as_ref()?.get_viewport()?;
        let right = self.right_control.as_ref()?.get_viewport()?;
        Some(Canvas {
            plane: Plane {
                world: panel.get_global_transform(),
                size: layout.physical,
            },
            eyes: [left, right],
        })
    }

    fn blend_overlap(&mut self, back: &[Canvas], eyes: [Vector3; 2], amount: f32) -> bool {
        let Some(canvas) = self.canvas() else {
            return false;
        };
        if self.overlap.is_empty() {
            self.overlap = [self.control.as_ref(), self.right_control.as_ref()]
                .into_iter()
                .flatten()
                .zip(&self.outputs)
                .filter_map(|(source, output)| overlap::Pass::new(source, output))
                .collect();
        }
        if self.overlap.len() != 2 {
            return false;
        }
        let mut applied = true;
        for (index, pass) in self.overlap.iter_mut().enumerate() {
            applied &= pass.apply(canvas.plane, back, eyes[index], index, amount);
        }
        applied
    }

    fn apply_clip(&mut self, clip: Clip) {
        clip.apply(&mut self.material);
        if let Some(right) = &mut self.right_material {
            clip.apply(right);
        }
        self.player.bind_mut().shape = Some(clip);
    }
    fn style_content(&mut self, size: Vector2, owner: &mut WeldSurface) {
        let Some(shape) = owner.player.bind().shape.map(|clip| clip.shape) else {
            return;
        };
        let (Some(panel), Some(parent)) = (&self.panel, &owner.panel) else {
            return;
        };
        if !panel.is_instance_valid() || !parent.is_instance_valid() {
            return;
        }
        let local = parent.get_global_transform().affine_inverse() * panel.get_global_position();
        let Some(clip) = Clip::layer(shape, Vector2::new(local.x, -local.y), size) else {
            return;
        };
        self.apply_clip(clip);
    }
    pub(super) fn move_to(&mut self, world: Transform3D) {
        self.placement.move_to(world);
    }
    pub(super) fn workspace_anchor(&self) -> (Vector3, Vector3) {
        self.placement.anchor()
    }
    pub(super) fn controls(
        &mut self,
        aim: Transform3D,
        content_y: Option<f32>,
    ) -> Option<(Part, f32)> {
        let bar = self.bar.as_mut()?;
        if let Some(y) = content_y {
            let height = self.player.bind().shape?.shape.size.y;
            bar.approach(y, height);
        }
        let hit = bar.hit(aim);
        bar.highlight(hit.map(|(part, _)| part));
        hit
    }
    pub(super) fn movable(&self) -> bool {
        self.pane.kind <= 1
    }
    pub(super) fn show_controls(&mut self, opacity: f32) {
        if let Some(bar) = &mut self.bar {
            bar.show(opacity);
        }
    }
}

pub(super) struct Workspace {
    pub session: Session,
    pub panes: BTreeMap<u64, Gd<WeldSurface>>,
    pub selected: Option<u64>,
    preferences: Option<XrPreferences>,
    stack: Stack,
}
impl Workspace {
    pub fn new(session: Session, preferences: Option<XrPreferences>) -> Self {
        Self {
            session,
            panes: BTreeMap::new(),
            selected: None,
            preferences,
            stack: Stack::default(),
        }
    }
    pub fn tick(&mut self) -> Result<()> {
        if self.session.is_cancelled() {
            self.stop();
            return Ok(());
        }
        let snapshot = self.session.presentation();
        self.panes.retain(|id, surface| {
            if snapshot.panes.iter().any(|pane| pane.id == *id) {
                true
            } else {
                let mut player = surface.bind().player.clone();
                if player.is_instance_valid() {
                    player.bind_mut().stop();
                    player.queue_free();
                }
                false
            }
        });
        for pane in snapshot.panes.iter().cloned() {
            if let Some(surface) = self.panes.get_mut(&pane.id) {
                surface.bind_mut().pane = pane;
                continue;
            }
            let mut texture = ExternalTexture::new_gd();
            texture.set_size(Vector2::new(320.0, 180.0));
            let mut shader = Shader::new_gd();
            let sampler = if Os::singleton().get_name() == "Android" {
                "samplerExternalOES"
            } else {
                "sampler2D"
            };
            shader.set_code(&format!(
                "shader_type canvas_item;\nuniform {sampler} video;\n{}",
                include_str!("stream.gdshaderinc")
            ));
            let mut material = ShaderMaterial::new_gd();
            material.set_shader(&shader);
            material.set_shader_parameter("video", &texture.to_variant());
            let controller =
                self.session
                    .attach(&pane, texture.clone().upcast(), material.clone().upcast())?;
            let player = WeldVideoPlayer::for_surface(controller, self.preferences);
            let id = pane.id;
            let surface = Gd::from_object(WeldSurface {
                pane,
                player,
                _texture: texture,
                material,
                right_material: None,
                control: None,
                right_control: None,
                outputs: Vec::new(),
                panel: None,
                decoration: None,
                bar: None,
                placement: Placement::default(),
                presentation_order: -100,
                input_order: -100,
                canvas_layout: None,
                overlap: Vec::new(),
            });
            self.panes.insert(id, surface);
        }
        // Recheck topology after potentially slow Godot resource creation.
        // Only immutable snapshots cross from the receiver. No shared inventory
        // lock covers fence checks, texture imports or scene operations.
        let current = self.session.presentation();
        // Topology may change while new Godot textures are being created.
        // Wait for the complete presentation inventory before consuming it.
        if current.panes.len() != self.panes.len()
            || current
                .panes
                .iter()
                .any(|pane| !self.panes.contains_key(&pane.id))
        {
            return Ok(());
        }
        for pane in current.panes.iter().cloned() {
            if let Some(surface) = self.panes.get_mut(&pane.id) {
                let stereo = pane.is_stereo();
                let mut surface = surface.bind_mut();
                surface.pane = pane;
                if stereo && surface.panel.is_none() {
                    surface.enable_side_by_side();
                }
            }
        }
        let mut ready = BTreeMap::<u64, bool>::new();
        for surface in self.panes.values() {
            let surface = surface.bind();
            let player = surface.player.bind();
            let can_tick = player.controller.as_ref().is_some_and(Controller::ready);
            *ready.entry(surface.pane.window).or_insert(true) &= can_tick;
        }
        current.present_ready(&ready);
        for surface in self.panes.values() {
            let surface = surface.bind();
            if ready.get(&surface.pane.window) == Some(&true) {
                surface.player.clone().bind_mut().tick();
            }
        }
        Ok(())
    }
    pub fn surfaces(&self) -> Array<Gd<WeldSurface>> {
        self.panes
            .values()
            .filter(|pane| pane.bind().pane.selected)
            .cloned()
            .collect()
    }
    pub fn selected_player(&self) -> Option<Gd<WeldVideoPlayer>> {
        self.panes
            .get(&self.selected?)
            .map(|s| s.bind().player.clone())
    }
    pub fn sort_xr_windows(&mut self, viewer: Vector3, eyes: [Vector3; 2]) {
        for surface in self.panes.values_mut() {
            for pass in &mut surface.bind_mut().overlap {
                pass.clear();
            }
        }
        if !viewer.is_finite() {
            return;
        }
        let layers: Vec<_> = self
            .panes
            .values()
            .filter_map(|surface| {
                let surface = surface.bind();
                let panel = surface.panel.as_ref()?;
                if !surface.pane.selected
                    || !surface.is_mapped()
                    || !panel.is_instance_valid()
                    || !panel.is_visible_in_tree()
                {
                    return None;
                }
                Some(Layer {
                    id: surface.pane.id,
                    window: surface.pane.window,
                    root: surface.pane.kind != 3,
                    popup_parent: (surface.pane.kind == 2).then_some(surface.pane.parent),
                    stack: surface.pane.stack,
                    distance: panel.get_global_position().distance_to(viewer),
                })
            })
            .collect();
        for (id, order) in self.stack.update(&layers) {
            if let Some(surface) = self.panes.get_mut(&id) {
                let mut surface = surface.bind_mut();
                surface.presentation_order = -100 + order;
                surface.input_order = surface.presentation_order;
            }
        }
        if let Some(blend) = &self.stack.blend {
            let back: Option<Vec<_>> = blend
                .back
                .iter()
                .map(|id| self.panes.get(id)?.bind().canvas())
                .collect();
            if let Some(back) = back {
                let mut applied = true;
                for id in &blend.front {
                    if let Some(surface) = self.panes.get_mut(id) {
                        applied &= surface.bind_mut().blend_overlap(&back, eyes, blend.amount);
                    } else {
                        applied = false;
                    }
                }
                if !applied {
                    for id in &blend.front {
                        if let Some(surface) = self.panes.get_mut(id) {
                            for pass in &mut surface.bind_mut().overlap {
                                pass.clear();
                            }
                        }
                    }
                    return;
                }
                // A held gesture still owns its route. Fresh hits choose the
                // visually dominant family once the crossfade passes halfway.
                if blend.amount > 0.5
                    && let Some(first) = blend.back.first().and_then(|id| self.panes.get(id))
                {
                    let start = first.bind().presentation_order;
                    for (rank, id) in blend.front.iter().chain(&blend.back).enumerate() {
                        if let Some(surface) = self.panes.get_mut(id) {
                            surface.bind_mut().input_order = start + rank as i32;
                        }
                    }
                }
            }
        }
    }
    pub fn input_order(&self, player: &Gd<WeldVideoPlayer>) -> i32 {
        self.panes
            .values()
            .find_map(|surface| {
                let surface = surface.bind();
                (&surface.player == player).then_some(surface.input_order)
            })
            .unwrap_or(-100)
    }
    pub(super) fn control_surface(&self, player: &Gd<WeldVideoPlayer>) -> Option<Gd<WeldSurface>> {
        let window = self
            .panes
            .values()
            .find(|surface| &surface.bind().player == player)?
            .bind()
            .pane
            .controls_window()?;
        self.panes
            .values()
            .find(|surface| {
                let surface = surface.bind();
                surface.pane.window == window && surface.movable()
            })
            .cloned()
    }
    pub fn pick(&mut self, position: Vector2) -> Option<(Gd<WeldVideoPlayer>, Gd<Control>)> {
        let mut hit = None;
        for (id, surface) in &self.panes {
            let surface = surface.bind();
            if !surface.is_mapped() {
                continue;
            }
            let Some(control) = surface.control.as_ref().filter(|c| {
                c.is_instance_valid() && !c.is_queued_for_deletion() && c.is_visible_in_tree()
            }) else {
                continue;
            };
            let player = surface.player.bind();
            if player.controller.as_ref().is_some_and(Controller::captured) {
                hit = Some((*id, surface.player.clone(), control.clone()));
                break;
            }
            if control.get_global_rect().contains_point(position) {
                hit = Some((*id, surface.player.clone(), control.clone()));
            }
        }
        if let Some((id, player, control)) = hit {
            self.selected = Some(id);
            return Some((player, control));
        }
        self.panes.get(&self.selected?).and_then(|s| {
            let s = s.bind();
            Some((s.player.clone(), s.control.clone()?))
        })
    }
    pub fn stop(&mut self) {
        self.session.stop();
        for surface in self.panes.values() {
            let mut player = surface.bind().player.clone();
            if player.is_instance_valid() {
                player.bind_mut().stop();
                player.queue_free();
            }
        }
        self.panes.clear();
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        self.stop();
    }
}

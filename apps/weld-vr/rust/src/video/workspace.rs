//! Godot projection of the existing receiver's surface hierarchy. A view owns
//! presentation objects, never its own transport, decoder pool or input seat.
use super::WeldVideoPlayer;
use super::decoration::{Decoration, Shape};
use super::xr::controls::{Bar, Part, Placement};
use crate::{
    playback::{
        Controller,
        session::{Pane, Session},
    },
    presentation::XrPreferences,
};
use anyhow::Result;
use godot::{
    classes::{Control, ExternalTexture, IRefCounted, MeshInstance3D, Os, Shader, ShaderMaterial},
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
    pub(super) control: Option<Gd<Control>>,
    pub(super) panel: Option<Gd<MeshInstance3D>>,
    decoration: Option<Decoration>,
    bar: Option<Bar>,
    placement: Placement,
}

#[godot_api]
impl IRefCounted for WeldSurface {}

#[godot_api]
impl WeldSurface {
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
        Vector2::new(self.pane.position[0], self.pane.position[1])
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
        Vector2::new(self.pane.size[0], self.pane.size[1])
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
    #[func]
    fn video_player(&self) -> Gd<WeldVideoPlayer> {
        self.player.clone()
    }
    #[func]
    fn bind_control(&mut self, control: Gd<Control>) {
        self.control = Some(control);
    }
    #[func]
    fn bind_panel(&mut self, panel: Gd<MeshInstance3D>) {
        self.panel = Some(panel);
    }
    #[func]
    fn style_panel(&mut self, physical_size: Vector2) {
        if self.pane.kind == 3 {
            return;
        }
        let Some(shape) = Shape::new(physical_size) else {
            return;
        };
        let Some(panel) = self
            .panel
            .as_ref()
            .filter(|panel| panel.is_instance_valid() && !panel.is_queued_for_deletion())
        else {
            return;
        };
        let decoration = self
            .decoration
            .get_or_insert_with(|| Decoration::new(panel.clone()));
        decoration.update(shape, &mut self.material);
        decoration.set_focused(
            self.player
                .bind()
                .controller
                .as_ref()
                .is_some_and(Controller::is_focused),
        );
        self.player.bind_mut().shape = Some(shape);
        if self.pane.kind <= 1 {
            self.bar
                .get_or_insert_with(|| Bar::new(panel.clone()))
                .place(physical_size.y);
        }
    }
    #[func]
    fn placed_transform(&mut self, base: Transform3D) -> Transform3D {
        self.placement.apply(base)
    }
}

impl WeldSurface {
    pub(super) fn move_to(&mut self, world: Transform3D) {
        self.placement.move_to(world);
    }
    pub(super) fn controls(
        &mut self,
        aim: Transform3D,
        content_y: Option<f32>,
    ) -> Option<(Part, f32)> {
        let bar = self.bar.as_mut()?;
        if let Some(y) = content_y {
            let height = self.player.bind().shape?.size.y;
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
}
impl Workspace {
    pub fn new(session: Session, preferences: Option<XrPreferences>) -> Self {
        Self {
            session,
            panes: BTreeMap::new(),
            selected: None,
            preferences,
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
                control: None,
                panel: None,
                decoration: None,
                bar: None,
                placement: Placement::default(),
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
                surface.bind_mut().pane = pane;
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
        self.panes.values().cloned().collect()
    }
    pub fn selected_player(&self) -> Option<Gd<WeldVideoPlayer>> {
        self.panes
            .get(&self.selected?)
            .map(|s| s.bind().player.clone())
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

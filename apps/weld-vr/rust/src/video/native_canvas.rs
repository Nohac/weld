//! Keep a sampleable canvas separate from the OpenXR-owned output texture.
//! Godot redirects a native layer viewport into rotating swapchain images;
//! those viewports must not also serve as inputs to other window shaders.
use godot::{
    classes::{
        ColorRect, Control, Node3D, RenderingServer, Shader, ShaderMaterial, SubViewport, Viewport,
        sub_viewport::UpdateMode,
    },
    prelude::*,
};

pub(super) struct NativeCanvas {
    source: Gd<SubViewport>,
    pub target: Gd<SubViewport>,
    pub view: Gd<Control>,
    original_parent: Option<Gd<Viewport>>,
}
impl NativeCanvas {
    pub fn new(source: Gd<SubViewport>, parent: &mut Gd<Node3D>) -> Option<Self> {
        let original_parent = source.get_parent()?.get_viewport()?;
        let texture = source.get_texture()?;
        let mut target = SubViewport::new_alloc();
        target.set_disable_3d(true);
        target.set_transparent_background(true);
        target.set_size(source.get_size());
        target.set_update_mode(UpdateMode::ALWAYS);
        parent.add_child(&target);
        let mut shader = Shader::new_gd();
        shader.set_code(include_str!("../../../shaders/native_canvas.gdshader"));
        let mut material = ShaderMaterial::new_gd();
        material.set_shader(&shader);
        material.set_shader_parameter("canvas_image", &texture.to_variant());
        let mut view = ColorRect::new_alloc();
        view.set_mouse_filter(godot::classes::control::MouseFilter::IGNORE);
        view.set_size(source.get_size().to_vector2());
        view.set_material(&material);
        target.add_child(&view);
        // All source canvases are render children of their output canvases.
        // Godot draws the deeper level first, so every output can sample any
        // source in the same frame, without cycles or per-frame reparenting.
        RenderingServer::singleton()
            .viewport_set_parent_viewport(source.get_viewport_rid(), target.get_viewport_rid());
        Some(Self {
            source,
            target,
            view: view.upcast(),
            original_parent: Some(original_parent),
        })
    }
    pub fn resize(&mut self, size: Vector2i) {
        if self.target.get_size() != size {
            self.target.set_size(size);
            self.view.set_size(size.to_vector2());
        }
    }
    pub fn detach(&mut self) {
        if let Some(parent) = self.original_parent.take()
            && self.source.is_instance_valid()
            && parent.is_instance_valid()
        {
            RenderingServer::singleton().viewport_set_parent_viewport(
                self.source.get_viewport_rid(),
                parent.get_viewport_rid(),
            );
        }
    }
}
impl Drop for NativeCanvas {
    fn drop(&mut self) {
        self.detach();
    }
}

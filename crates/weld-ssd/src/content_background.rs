//! Shared rounded backing for placeholders and uncovered client viewport space.

use bevy::{
    asset::{Assets, Handle, load_internal_asset, uuid_handle},
    prelude::*,
    render::render_resource::AsBindGroup,
    shader::{Shader, ShaderRef},
    ui::UiSystems,
    ui_render::{UiMaterialPlugin, ui_material::UiMaterial},
    window::RequestRedraw,
};
use weld_app::{pixel_snap::PixelSnapSystems, surface::SurfaceNode};

use crate::{FrameContent, SsdSettings};

const SHADER: Handle<Shader> = uuid_handle!("2fc7b368-93d3-49ca-b3aa-e926a87013af");

#[derive(Asset, TypePath, AsBindGroup, Clone, PartialEq)]
pub(super) struct ContentBackground {
    #[uniform(0)]
    pub color: LinearRgba,
    /// Normalized bounds of the actual, pixel-snapped client content.
    #[uniform(1)]
    covered: Vec4,
}

impl UiMaterial for ContentBackground {
    fn fragment_shader() -> ShaderRef {
        SHADER.into()
    }
}

pub(super) fn install(app: &mut App) {
    load_internal_asset!(app, SHADER, "content_background.wesl", Shader::from_wesl);
    app.add_plugins(UiMaterialPlugin::<ContentBackground>::default())
        .add_systems(
            PostUpdate,
            sync.after(UiSystems::PostLayout)
                .after(PixelSnapSystems)
                .before(bevy::asset::AssetEventSystems),
        );
}

type ContentNodes<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static ComputedNode,
        &'static UiGlobalTransform,
        &'static Children,
        Option<&'static mut MaterialNode<ContentBackground>>,
    ),
    With<FrameContent>,
>;

fn sync(
    mut commands: Commands,
    settings: Res<SsdSettings>,
    mut materials: ResMut<Assets<ContentBackground>>,
    mut contents: ContentNodes,
    surfaces: Query<(&ComputedNode, &UiGlobalTransform, &Node), With<SurfaceNode>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let mut changed = false;
    for (entity, frame, transform, children, handle) in &mut contents {
        let covered = children.iter().find_map(|child| {
            let (surface, placement, node) = surfaces.get(child).ok()?;
            if node.display == Display::None || frame.size().min_element() <= 0.0 {
                return None;
            }
            let inverse = transform.try_inverse()?;
            let relative = inverse * placement.affine();
            let minimum =
                relative.transform_point2(-0.5 * surface.size()) / frame.size() + Vec2::splat(0.5);
            let maximum =
                relative.transform_point2(0.5 * surface.size()) / frame.size() + Vec2::splat(0.5);
            Some(Vec4::new(minimum.x, minimum.y, maximum.x, maximum.y))
        });
        let covered = covered.unwrap_or(Vec4::ZERO);
        if covered.x <= 0.0 && covered.y <= 0.0 && covered.z >= 1.0 && covered.w >= 1.0 {
            if handle.is_some() {
                commands
                    .entity(entity)
                    .remove::<MaterialNode<ContentBackground>>();
                changed = true;
            }
            continue;
        }
        let expected = ContentBackground {
            color: settings.placeholder.background.to_linear(),
            covered,
        };
        let Some(mut handle) = handle else {
            commands
                .entity(entity)
                .insert(MaterialNode(materials.add(expected)));
            changed = true;
            continue;
        };
        if materials.get(&handle.0) == Some(&expected) {
            continue;
        }
        changed = true;
        if materials.contains(&handle.0) {
            if let Some(mut material) = materials.get_mut(&handle.0) {
                *material = expected;
                // Retained presentation observes the node's change tick as well
                // as the updated GPU material, including palette-only reloads.
                handle.set_changed();
            }
        } else {
            handle.0 = materials.add(expected);
        }
    }
    if changed {
        redraw.write(RequestRedraw);
    }
}

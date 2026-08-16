//! Standard-distribution watermark and output-topology diagnostics.

use bevy::{
    app::{App, Plugin, Startup, Update},
    ecs::{
        change_detection::{DetectChanges, Ref},
        component::Component,
        entity::Entity,
        message::MessageReader,
        query::With,
        resource::Resource,
        system::{Commands, Query, ResMut},
    },
    math::{UVec2, Vec2},
    picking::Pickable,
    prelude::{
        AlignItems, BackgroundColor, BorderColor, BorderRadius, ChildOf, Children, Color,
        GlobalZIndex, Node, PositionType, UiRect, UiTargetCamera, px,
    },
    scene::{CommandsSceneExt, Scene, bsn},
    text::{FontSourceTemplate, TextColor, TextFont},
    ui::widget::{Text, TextShadow},
};
use weld_app::{
    input::GlobalShortcutAction,
    output::{
        OutputCompositionCamera, OutputFootprintProvenance, OutputGeometry, OutputInfo,
        OutputPlacement, OutputPosition, PrimaryOutput, WeldOutput,
    },
};

pub(crate) struct DistributionOverlayPlugin;

impl Plugin for DistributionOverlayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<OutputTopologyOverlayState>()
            .add_systems(Startup, spawn_distribution_overlay)
            .add_systems(Update, update_output_topology_overlay);
    }
}

#[derive(Resource, Default)]
struct OutputTopologyOverlayState {
    visible: bool,
    dirty: bool,
}

#[derive(Component)]
struct OutputTopologyOverlay;

#[derive(Clone)]
struct OutputTopologySnapshot {
    id: u64,
    name: String,
    physical_size: UVec2,
    physical_size_millimeters: Option<UVec2>,
    pixels_per_inch: Option<Vec2>,
    footprint_position: Vec2,
    footprint_size: Vec2,
    footprint_provenance: OutputFootprintProvenance,
    scale: f32,
    logical_size: Vec2,
    position: Vec2,
    primary: bool,
}

type OutputTopologyQueryItem<'world> = (
    &'world WeldOutput,
    &'world OutputInfo,
    Ref<'world, OutputGeometry>,
    Ref<'world, OutputPlacement>,
    Ref<'world, OutputPosition>,
    Option<&'world PrimaryOutput>,
);

fn spawn_distribution_overlay(mut commands: Commands) {
    commands.spawn_scene(bsn! {
        Pickable::IGNORE
        Node {
            position_type: PositionType::Absolute,
            top: px(24),
            right: px(24),
            width: px(240),
            height: px(88),
            padding: UiRect::all(px(16)),
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(18)),
        }
        Text("Weld Master")
        TextFont {
            font: FontSourceTemplate::Monospace,
            font_size: px(20.0),
        }
        TextColor(Color::srgb(0.9, 0.9, 0.9))
        TextShadow
        BackgroundColor(Color::srgba(0.08, 0.34, 0.48, 0.82))
        GlobalZIndex(weld_app::layer::SHELL_Z_INDEX)
    });
}

fn update_output_topology_overlay(
    mut commands: Commands,
    mut actions: MessageReader<GlobalShortcutAction>,
    mut state: ResMut<OutputTopologyOverlayState>,
    roots: Query<Entity, With<OutputTopologyOverlay>>,
    outputs: Query<OutputTopologyQueryItem<'_>>,
    primary_camera: Query<&OutputCompositionCamera, With<PrimaryOutput>>,
) {
    let toggles = actions
        .read()
        .filter(|action| matches!(action, GlobalShortcutAction::ToggleOutputTopology))
        .count();
    if toggles % 2 == 1 {
        state.visible = !state.visible;
        state.dirty = true;
    }
    state.dirty |= outputs
        .iter()
        .any(|(_, _, geometry, placement, position, _)| {
            geometry.is_changed() || placement.is_changed() || position.is_changed()
        });
    if !state.dirty {
        return;
    }
    state.dirty = false;

    for root in &roots {
        commands.entity(root).despawn();
    }
    if !state.visible {
        return;
    }

    let Ok(camera) = primary_camera.single() else {
        state.dirty = true;
        return;
    };
    let Some(camera) = camera.entity() else {
        state.dirty = true;
        return;
    };
    let mut snapshots = outputs
        .iter()
        .map(
            |(output, info, geometry, placement, position, primary)| OutputTopologySnapshot {
                id: output.id.raw(),
                name: info.name().to_owned(),
                physical_size: geometry.physical_size(),
                physical_size_millimeters: info.physical_size_millimeters(),
                pixels_per_inch: info.pixels_per_inch(geometry.physical_size()),
                footprint_position: placement.position_millimeters(),
                footprint_size: placement.size_millimeters(),
                footprint_provenance: placement.provenance(),
                scale: geometry.scale_factor(),
                logical_size: geometry.logical_size(),
                position: position.0,
                primary: primary.is_some(),
            },
        )
        .collect::<Vec<_>>();
    snapshots.sort_by_key(|snapshot| snapshot.id);
    let logical_rectangles = snapshots
        .iter()
        .map(|output| (output.position, output.logical_size))
        .collect::<Vec<_>>();
    let Some(logical_mapping) = DiagramMapping::new(&logical_rectangles, LOGICAL_CANVAS) else {
        state.dirty = true;
        return;
    };
    let physical_rectangles = snapshots
        .iter()
        .map(|output| (output.footprint_position, output.footprint_size))
        .collect::<Vec<_>>();
    let Some(physical_mapping) = DiagramMapping::new(&physical_rectangles, PHYSICAL_CANVAS) else {
        state.dirty = true;
        return;
    };

    let root = commands
        .spawn_scene(output_topology_panel())
        .insert((OutputTopologyOverlay, UiTargetCamera(camera)))
        .id();
    for snapshot in &snapshots {
        let (left, top, width, height) =
            logical_mapping.place(snapshot.position, snapshot.logical_size);
        commands
            .spawn_scene(logical_output_box(snapshot, left, top, width, height))
            .insert(ChildOf(root));

        let (left, top, width, height) =
            physical_mapping.place(snapshot.footprint_position, snapshot.footprint_size);
        commands
            .spawn_scene(physical_output_box(snapshot, left, top, width, height))
            .insert(ChildOf(root));
    }
}

const TOPOLOGY_PANEL_WIDTH: f32 = 900.0;
const TOPOLOGY_PANEL_HEIGHT: f32 = 350.0;

#[derive(Clone, Copy)]
struct DiagramCanvas {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
}

const LOGICAL_CANVAS: DiagramCanvas = DiagramCanvas {
    left: 20.0,
    top: 78.0,
    width: 410.0,
    height: 252.0,
};
const PHYSICAL_CANVAS: DiagramCanvas = DiagramCanvas {
    left: 470.0,
    top: 78.0,
    width: 410.0,
    height: 252.0,
};

struct DiagramMapping {
    minimum: Vec2,
    scale: f32,
    offset: Vec2,
}

impl DiagramMapping {
    fn new(rectangles: &[(Vec2, Vec2)], canvas: DiagramCanvas) -> Option<Self> {
        let minimum = rectangles
            .iter()
            .map(|(position, _)| *position)
            .reduce(Vec2::min)?;
        let maximum = rectangles
            .iter()
            .map(|(position, size)| *position + *size)
            .reduce(Vec2::max)?;
        let topology_size = maximum - minimum;
        if topology_size.x <= 0.0 || topology_size.y <= 0.0 {
            return None;
        }
        let scale = (canvas.width / topology_size.x).min(canvas.height / topology_size.y);
        let drawn_size = topology_size * scale;
        Some(Self {
            minimum,
            scale,
            offset: Vec2::new(
                canvas.left + (canvas.width - drawn_size.x) * 0.5,
                canvas.top + (canvas.height - drawn_size.y) * 0.5,
            ),
        })
    }

    fn place(&self, rectangle_position: Vec2, rectangle_size: Vec2) -> (f32, f32, f32, f32) {
        let position = self.offset + (rectangle_position - self.minimum) * self.scale;
        let size = rectangle_size * self.scale;
        (position.x, position.y, size.x, size.y)
    }
}

fn output_topology_panel() -> impl Scene {
    bsn! {
        Node {
            position_type: PositionType::Absolute,
            left: px(24),
            bottom: px(24),
            width: px(TOPOLOGY_PANEL_WIDTH),
            height: px(TOPOLOGY_PANEL_HEIGHT),
            padding: UiRect::all(px(12)),
            border: UiRect::all(px(1)),
            border_radius: BorderRadius::all(px(12)),
        }
        BorderColor::all(Color::srgba(0.45, 0.72, 0.88, 0.72))
        BackgroundColor(Color::srgba(0.025, 0.04, 0.065, 0.94))
        GlobalZIndex(weld_app::layer::SHELL_Z_INDEX)
        Pickable::IGNORE
        Children [
            (
                Pickable::IGNORE
                Node {
                    position_type: PositionType::Absolute,
                    left: px(16),
                    top: px(10),
                }
                Text("Output topology diagnostics")
                TextFont {
                    font: FontSourceTemplate::Monospace,
                    font_size: px(16.0),
                }
                TextColor(Color::srgb(0.92, 0.96, 1.0))
            ),
            (
                Pickable::IGNORE
                Node {
                    position_type: PositionType::Absolute,
                    left: px(20),
                    top: px(42),
                }
                Text("Logical workspace (mode / scale)")
                TextFont {
                    font: FontSourceTemplate::Monospace,
                    font_size: px(13.0),
                }
                TextColor(Color::srgb(0.72, 0.85, 0.94))
            ),
            (
                Pickable::IGNORE
                Node {
                    position_type: PositionType::Absolute,
                    left: px(470),
                    top: px(42),
                }
                Text("Physical placement (mm, scale-independent)")
                TextFont {
                    font: FontSourceTemplate::Monospace,
                    font_size: px(13.0),
                }
                TextColor(Color::srgb(0.72, 0.85, 0.94))
            ),
        ]
    }
}

fn logical_output_box(
    output: &OutputTopologySnapshot,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
) -> impl Scene {
    let physical_metadata = output
        .physical_size_millimeters
        .zip(output.pixels_per_inch)
        .map(|(millimeters, dpi)| {
            format!(
                "EDID {}x{} mm - {:.0}x{:.0} dpi",
                millimeters.x, millimeters.y, dpi.x, dpi.y
            )
        })
        .unwrap_or_else(|| "EDID size unavailable".to_owned());
    let primary = if output.primary { " [primary]" } else { "" };
    let label = format!(
        "{}{primary}\nmode {}x{} - scale {:.2}\nlogical {:.0}x{:.0} at ({:.0}, {:.0})\n{physical_metadata}",
        output.name,
        output.physical_size.x,
        output.physical_size.y,
        output.scale,
        output.logical_size.x,
        output.logical_size.y,
        output.position.x,
        output.position.y,
    );
    let border = if output.primary {
        Color::srgb(0.35, 0.88, 0.68)
    } else {
        Color::srgb(0.35, 0.68, 0.95)
    };
    let background = if output.primary {
        Color::srgba(0.08, 0.32, 0.23, 0.92)
    } else {
        Color::srgba(0.08, 0.20, 0.34, 0.92)
    };
    bsn! {
        Pickable::IGNORE
        Node {
            position_type: PositionType::Absolute,
            left: px(left),
            top: px(top),
            width: px(width),
            height: px(height),
            padding: UiRect::all(px(8)),
            border: UiRect::all(px(2)),
            border_radius: BorderRadius::all(px(6)),
            align_items: AlignItems::Center,
        }
        BorderColor::all(border)
        BackgroundColor(background)
        Children [(
            Pickable::IGNORE
            Text(label)
            TextFont {
                font: FontSourceTemplate::Monospace,
                font_size: px(11.0),
            }
            TextColor(Color::srgb(0.94, 0.97, 1.0))
        )]
    }
}

fn physical_output_box(
    output: &OutputTopologySnapshot,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
) -> impl Scene {
    let dpi = match output.footprint_provenance {
        OutputFootprintProvenance::Measured => output.pixels_per_inch.map_or_else(
            || "DPI unavailable".to_owned(),
            |dpi| {
                let diagonal_dpi =
                    output.physical_size.as_vec2().length() * 25.4 / output.footprint_size.length();
                format!(
                    "{:.0}x{:.0} axis dpi - {diagonal_dpi:.0} diagonal dpi",
                    dpi.x, dpi.y
                )
            },
        ),
        OutputFootprintProvenance::Assumed96Dpi => "96 assumed diagonal dpi".to_owned(),
    };
    let primary = if output.primary { " [primary]" } else { "" };
    let source = match output.footprint_provenance {
        OutputFootprintProvenance::Measured => "EDID",
        OutputFootprintProvenance::Assumed96Dpi => "assumed 96 DPI",
    };
    let label = format!(
        "{}{primary}\n{:.0}x{:.0} mm ({source})\n{dpi}",
        output.name, output.footprint_size.x, output.footprint_size.y
    );
    let border = if output.primary {
        Color::srgb(0.35, 0.88, 0.68)
    } else {
        Color::srgb(0.35, 0.68, 0.95)
    };
    let background = if output.primary {
        Color::srgba(0.08, 0.32, 0.23, 0.92)
    } else {
        Color::srgba(0.08, 0.20, 0.34, 0.92)
    };
    bsn! {
        Pickable::IGNORE
        Node {
            position_type: PositionType::Absolute,
            left: px(left),
            top: px(top),
            width: px(width),
            height: px(height),
            padding: UiRect::all(px(8)),
            border: UiRect::all(px(2)),
            border_radius: BorderRadius::all(px(6)),
            align_items: AlignItems::Center,
        }
        BorderColor::all(border)
        BackgroundColor(background)
        Children [(
            Pickable::IGNORE
            Text(label)
            TextFont {
                font: FontSourceTemplate::Monospace,
                font_size: px(11.0),
            }
            TextColor(Color::srgb(0.94, 0.97, 1.0))
        )]
    }
}

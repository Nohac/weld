use super::*;
use bevy::ecs::{change_detection::DetectChanges, system::RunSystemOnce};
use weld_app::{
    output::{OutputGeometry, OutputPosition, PrimaryOutput, WeldOutput},
    surface::{ClientWindowMetadata, SurfaceActionQueue},
};
use weld_client::ClientSurfaceMetadata;
use weld_tile::{TileFloatingRequest, TilePlugin, TileSelect};
use weld_window::{
    ManagedWindow, OccupiesWindow, WindowCommand, WindowCommandKind, WindowPlugin, WindowVacancy,
    workspace::{WorkspaceMember, WorkspaceRequest},
};

fn find(node: &Node, id: u64) -> Option<&Node> {
    if node.id == id {
        return Some(node);
    }
    node.nodes
        .iter()
        .chain(&node.floating_nodes)
        .find_map(|node| find(node, id))
}

fn process_node(node: &Node) -> Option<&Node> {
    if node.pid == Some(4242) {
        return Some(node);
    }
    node.nodes
        .iter()
        .chain(&node.floating_nodes)
        .find_map(process_node)
}

#[test]
fn inventories_publish_geometry_selection_and_removal_in_one_desktop_revision() {
    let directory = tempfile::tempdir().expect("runtime");
    let service = crate::PreparedIpcService::bind_in(directory.path())
        .expect("socket")
        .start()
        .expect("service");
    let publisher = service.desktop_publisher();
    let mut app = App::new();
    app.init_resource::<SurfaceActionQueue>().add_plugins((
        WindowPlugin,
        TilePlugin,
        SwayIpcPlugin(publisher.clone()),
    ));
    let output = app
        .world_mut()
        .spawn((
            WeldOutput {
                id: OutputId::new(7),
            },
            PrimaryOutput,
            OutputPosition(Vec2::new(-800.0, 100.0)),
            OutputGeometry::from_physical(UVec2::new(1600, 1200), 2.0),
        ))
        .id();
    app.world_mut()
        .run_system_once(
            move |mut creation: weld_window::workspace::WorkspaceCreation,
                  mut commands: Commands| {
                let workspace = creation.create("1".into(), output).expect("workspace");
                commands.trigger(WorkspaceRequest::SetVisible {
                    workspace,
                    visible: true,
                });
                commands.trigger(WorkspaceRequest::Focus(workspace));
            },
        )
        .expect("workspace setup");
    let first = app
        .world_mut()
        .spawn((
            ManagedWindow {
                id: WindowId::new(1),
            },
            WindowVacancy::Retain,
        ))
        .id();
    app.world_mut().spawn((
        ManagedWindow {
            id: WindowId::new(2),
        },
        WindowVacancy::Retain,
    ));
    app.update();
    let occupant = app
        .world_mut()
        .spawn((
            OccupiesWindow(first),
            ClientWindowMetadata(
                ClientSurfaceMetadata::new("capture-probe".into(), "First window".into())
                    .expect("metadata")
                    .with_local_process_id(Some(4242)),
            ),
        ))
        .id();
    app.world_mut().trigger(WindowCommand {
        window: first,
        kind: WindowCommandKind::Focus,
    });
    app.update();
    let initial = publisher.0.borrow().clone();
    assert_eq!(
        initial.outputs[0].rect,
        Rect {
            x: -800,
            y: 100,
            width: 800,
            height: 600
        }
    );
    assert_eq!(initial.outputs[0].scale, 2.0);
    assert!(initial.outputs[0].focused);
    let leaf = process_node(&initial.tree).expect("occupied window");
    let id = leaf.id;
    let geometry = app
        .world()
        .get::<weld_window::WindowGeometry>(first)
        .expect("geometry");
    assert_eq!(
        leaf.rect,
        rectangle(geometry.position + Vec2::new(-800.0, 100.0), geometry.size)
    );
    assert!(leaf.focused && leaf.visible);
    assert_eq!(leaf.app_id.as_deref(), Some("capture-probe"));
    let tick = app
        .world()
        .get_resource_ref::<WindowInventory>()
        .expect("inventory")
        .last_changed();
    app.update();
    assert_eq!(
        app.world()
            .get_resource_ref::<WindowInventory>()
            .expect("inventory")
            .last_changed(),
        tick
    );
    assert!(std::sync::Arc::ptr_eq(&initial, &publisher.0.borrow()));

    let attention = app
        .world_mut()
        .spawn(weld_window::attention::AttentionForWindow(first))
        .id();
    app.update();
    let urgent = publisher.0.borrow().clone();
    assert!(process_node(&urgent.tree).expect("window").urgent);
    assert!(urgent.tree.urgent);
    assert!(urgent.tree.nodes[0].urgent);
    assert!(urgent.tree.nodes[0].nodes[0].urgent);
    app.world_mut().despawn(attention);
    app.update();
    assert!(!publisher.0.borrow().tree.urgent);

    app.world_mut()
        .entity_mut(output)
        .remove::<OutputPosition>();
    app.update();
    assert!(publisher.0.borrow().outputs.is_empty());
    app.world_mut()
        .entity_mut(output)
        .insert(OutputPosition(Vec2::new(-800.0, 100.0)));
    app.update();
    assert_eq!(
        process_node(&publisher.0.borrow().tree)
            .expect("restored output")
            .id,
        id
    );

    app.world_mut().trigger(TileFloatingRequest {
        window: Some(first),
        enabled: Some(true),
    });
    app.update();
    let floating = publisher.0.borrow().clone();
    assert_eq!(
        find(&floating.tree, id).expect("same window").kind,
        NodeType::FloatingCon
    );
    assert!(
        floating.tree.nodes[0].nodes[0]
            .floating_nodes
            .iter()
            .any(|node| node.id == id)
    );

    let workspace = app
        .world()
        .get::<WorkspaceMember>(first)
        .expect("workspace")
        .0;
    app.world_mut().trigger(TileSelect(workspace));
    app.update();
    let selected = publisher.0.borrow().clone();
    assert!(selected.tree.nodes[0].nodes[0].focused);
    assert!(!find(&selected.tree, id).expect("window").focused);

    app.world_mut().trigger(WorkspaceRequest::SetVisible {
        workspace,
        visible: false,
    });
    app.update();
    assert!(
        !find(&publisher.0.borrow().tree, id)
            .expect("hidden window")
            .visible
    );
    app.world_mut().entity_mut(occupant).despawn();
    app.update();
    assert_eq!(
        find(&publisher.0.borrow().tree, id)
            .expect("retained slot")
            .pid,
        None
    );
    app.world_mut().entity_mut(first).despawn();
    app.update();
    assert!(find(&publisher.0.borrow().tree, id).is_none());
}

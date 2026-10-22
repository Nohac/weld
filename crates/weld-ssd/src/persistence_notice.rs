//! Brief, input-transparent feedback for a window's persistence toggle.

use std::time::{Duration, Instant};

use bevy::{
    ecs::{
        component::Component,
        entity::Entity,
        message::{MessageReader, MessageWriter},
        system::{Commands, Query, Res},
        template::template,
    },
    picking::Pickable,
    prelude::{
        AccessibleLabel, AlignItems, BackgroundColor, BorderColor, BorderRadius, ChildOf, Children,
        JustifyContent, Node, Overflow, PositionType, Scene, Text, TextColor, UiRect, ZIndex,
        percent, px,
    },
    scene::{CommandsSceneExt, bsn},
    text::{FontSize, Justify, TextLayout},
    window::RequestRedraw,
};
use weld_window::{
    PrimaryWindowPresentation, WindowVisibility, persistence::WindowPersistenceToggled,
};

use crate::SsdSettings;

#[derive(Component)]
pub(super) struct PersistenceNotice {
    pub(super) expires: Instant,
}

pub(super) fn sync(
    mut commands: Commands,
    mut toggles: MessageReader<WindowPersistenceToggled>,
    windows: Query<(&PrimaryWindowPresentation, &WindowVisibility)>,
    notices: Query<(Entity, &PersistenceNotice)>,
    settings: Res<SsdSettings>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let now = Instant::now();
    let latest = toggles
        .read()
        .filter_map(|toggle| {
            let (presentation, visibility) = windows.get(toggle.window).ok()?;
            (*visibility == WindowVisibility::Visible).then_some((presentation.entity(), toggle))
        })
        .last();
    for (entity, notice) in &notices {
        if latest.is_some() || now >= notice.expires {
            commands.entity(entity).despawn();
        }
        // Keep expiry progressing through the host's paced policy updates.
        redraw.write(RequestRedraw);
    }
    if let Some((root, toggle)) = latest {
        commands.spawn_scene(scene(toggle, &settings)).insert((
            ChildOf(root),
            PersistenceNotice {
                expires: now + Duration::from_millis(1500),
            },
        ));
        redraw.write(RequestRedraw);
    }
}

fn scene(toggle: &WindowPersistenceToggled, settings: &SsdSettings) -> impl Scene {
    let state = if toggle.enabled {
        "enabled"
    } else {
        "disabled"
    };
    let label = format!("Persistent window {state}\n\n{}", toggle.matching);
    let accessible = label.clone();
    let colors = if toggle.enabled {
        settings.persistent_focused
    } else {
        settings.focused
    };
    let mut font = settings.title_font.text_font();
    font.font_size = FontSize::Px(settings.title_font.size.max(18.0));
    let radius = f32::from(settings.corner_radius.min(64));
    bsn! {
        Pickable::IGNORE
        ZIndex(100)
        Node {
            position_type: PositionType::Absolute,
            left: px(0), top: px(0),
            width: percent(100), height: percent(100),
            align_items: AlignItems::Center, justify_content: JustifyContent::Center,
            overflow: Overflow::clip(),
        }
        Children [
            Pickable::IGNORE
            Node {
                width: px(240), min_height: px(240), max_width: percent(90), max_height: percent(90),
                padding: UiRect::all(px(20)), border: UiRect::all(px(2)),
                border_radius: BorderRadius::all(px(radius)),
                align_items: AlignItems::Center, justify_content: JustifyContent::Center,
                overflow: Overflow::clip(),
            }
            BackgroundColor({colors.background})
            BorderColor::all(colors.child_border)
            Children [
                Pickable::IGNORE
                Node { max_width: percent(100) }
                Text(label)
                AccessibleLabel::new(accessible)
                TextLayout::justify(Justify::Center)
                template(move |_| Ok(font.clone()))
                TextColor({colors.foreground})
            ]
        ]
    }
}

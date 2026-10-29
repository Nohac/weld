//! Solid reserved-window content showing the replacement criteria.

use bevy::{
    ecs::{
        change_detection::{DetectChanges, Ref},
        component::Component,
        entity::Entity,
        lifecycle::RemovedComponents,
        message::MessageWriter,
        system::{Commands, Query, Res},
        template::template,
    },
    picking::Pickable,
    prelude::{
        AccessibleLabel, AlignItems, Children, JustifyContent, Node, Overflow, Scene, Text,
        TextColor, TextFont, UiRect, percent, px,
    },
    scene::bsn,
    text::FontSize,
    window::RequestRedraw,
};
use weld_window::persistence::PersistentWindow;

use crate::{FrameGeometry, SsdSettings, window_scene};

#[derive(Component, Clone, Copy)]
pub(super) struct VacantText(Entity);

#[derive(Component, Clone, Copy, Default)]
pub(super) struct VacantContent;

fn description(persistent: Option<&PersistentWindow>) -> String {
    persistent.map_or_else(|| "Reserved window".into(), |persistent| {
        format!("Persistent window\n{}\n\nWaiting for:\n{}\n\nClose this empty window to remove it.",
            persistent.label(), persistent.match_description())
    })
}

fn text_font(settings: &SsdSettings) -> TextFont {
    let mut font = settings.title_font.text_font();
    font.font_size = FontSize::Px(settings.title_font.size.max(18.0));
    font
}

pub(super) fn scene(
    window: Entity,
    geometry: FrameGeometry,
    settings: &SsdSettings,
    persistent: Option<&PersistentWindow>,
) -> impl Scene {
    let label = description(persistent);
    let accessible = label.clone();
    let font = text_font(settings);
    let colors = settings.placeholder;
    let content = bsn! {
        VacantContent
        Node {
            width: percent(100), flex_grow: 1.0, flex_basis: px(0), min_width: px(0), min_height: px(0),
            border_radius: {geometry.content_radius()},
            padding: UiRect::all(px(16)), align_items: AlignItems::Center,
            justify_content: JustifyContent::Center, overflow: Overflow::clip(),
        }
        Children [
            template(move |_| Ok(VacantText(window)))
            Node { max_width: percent(100) }
            Text::new(label)
            AccessibleLabel::new(accessible)
            template(move |_| Ok(font.clone()))
            TextColor({colors.foreground})
            Pickable::IGNORE
        ]
    };
    window_scene(content, geometry)
}

type VacantLabels<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static VacantText,
        &'static mut Text,
        &'static mut TextFont,
        &'static mut TextColor,
    ),
>;

pub(super) fn sync(
    settings: Res<SsdSettings>,
    persistent: Query<Ref<PersistentWindow>>,
    mut removed: RemovedComponents<PersistentWindow>,
    mut labels: VacantLabels,
    mut redraw: MessageWriter<RequestRedraw>,
    mut commands: Commands,
) {
    let refresh = settings.is_changed() || removed.read().count() > 0;
    let mut changed = false;
    for (entity, label, mut text, mut font, mut color) in &mut labels {
        let state = persistent.get(label.0).ok();
        if !refresh && !state.as_ref().is_some_and(|state| state.is_changed()) {
            continue;
        }
        let expected = description(state.as_deref());
        if text.0 != expected {
            commands
                .entity(entity)
                .try_insert(AccessibleLabel::new(expected.clone()));
            text.0 = expected;
            changed = true;
        }
        let expected_font = text_font(&settings);
        if *font != expected_font {
            *font = expected_font;
            changed = true;
        }
        if color.0 != settings.placeholder.foreground {
            color.0 = settings.placeholder.foreground;
            changed = true;
        }
    }
    if changed {
        redraw.write(RequestRedraw);
    }
}

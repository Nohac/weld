mod decode;
mod receiver;
mod renderer;

use crate::geometry::fit_rect;
use bevy::{
    input::touch::TouchPhase,
    prelude::*,
    render::{
        Render, RenderApp, RenderSystems,
        extract_resource::{ExtractResource, ExtractResourcePlugin},
    },
    window::{AppLifecycle, PrimaryWindow, WindowFocused},
};
use receiver::{Input, Session, Shared};
use std::{sync::Arc, time::Instant};
use weld_client::{
    ButtonState, ClientPointerRoute, InputEventKind, InputPosition, LinuxButtonCode,
};

#[derive(Resource)]
struct Receiver(Session);
#[derive(Resource, Clone, ExtractResource)]
struct Stream {
    shared: Arc<Shared>,
    image: Handle<Image>,
}
#[derive(Component, Clone, Default)]
struct VideoPanel;
#[derive(Default, Resource)]
struct Pointer {
    held: Option<(u64, ClientPointerRoute)>,
    clock: Option<Instant>,
}

pub fn install(app: &mut App) {
    app.add_plugins(ExtractResourcePlugin::<Stream>::default())
        .init_resource::<Pointer>()
        .add_systems(Startup, start)
        .add_systems(Update, (lifecycle, layout, touch).chain());
    app.sub_app_mut(RenderApp)
        .init_resource::<renderer::VideoRenderer>()
        .add_systems(
            Render,
            renderer::present.in_set(RenderSystems::PrepareResources),
        );
}

fn start(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let result = bevy::android::ANDROID_APP
        .get()
        .and_then(|app| app.internal_data_path())
        .ok_or_else(|| anyhow::anyhow!("Android private files directory missing"))
        .and_then(|directory| Session::start(directory.join("weld-device")));
    match result {
        Ok(session) => {
            let image = images.add(Image::default());
            commands.spawn_scene(bsn! {
                VideoPanel
                Node { position_type: PositionType::Absolute }
                ImageNode { image: {image.clone()}, image_mode: NodeImageMode::Stretch }
            });
            commands.insert_resource(Stream {
                shared: session.shared.clone(),
                image,
            });
            commands.insert_resource(Receiver(session));
        }
        Err(error) => error!("could not start receiver: {error:#}"),
    }
}

fn lifecycle(
    receiver: Option<Res<Receiver>>,
    mut lifecycle: MessageReader<AppLifecycle>,
    mut focus: MessageReader<WindowFocused>,
    mut pointer: ResMut<Pointer>,
) {
    let Some(receiver) = receiver else {
        return;
    };
    for event in lifecycle.read() {
        let active = matches!(event, AppLifecycle::Running | AppLifecycle::WillResume);
        receiver.0.set_active(active);
        if !active {
            pointer.held = None;
        }
    }
    for event in focus.read() {
        if !event.focused {
            receiver.0.reset_input();
            pointer.held = None;
        }
    }
}

fn layout(
    receiver: Option<Res<Receiver>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut panels: Query<(&mut Node, &mut Visibility), With<VideoPanel>>,
    mut status: Query<&mut Text, With<crate::Status>>,
) {
    let Some(receiver) = receiver else {
        return;
    };
    if let Ok(message) = receiver.0.shared.status.lock() {
        for mut text in &mut status {
            if text.0 != *message {
                text.0.clone_from(&message);
            }
        }
    }
    let Ok(window) = windows.single() else {
        return;
    };
    let displayed = receiver
        .0
        .shared
        .displayed
        .lock()
        .ok()
        .and_then(|value| value.clone());
    for (mut node, mut visible) in &mut panels {
        if let Some((_, input)) = &displayed {
            let rect = fit_rect(
                [window.width() as f64, window.height() as f64],
                input.logical_size,
            );
            node.left = px(rect[0] as f32);
            node.top = px(rect[1] as f32);
            node.width = px(rect[2] as f32);
            node.height = px(rect[3] as f32);
            *visible = Visibility::Visible;
        } else {
            *visible = Visibility::Hidden;
        }
    }
}

fn touch(
    receiver: Option<Res<Receiver>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut events: MessageReader<TouchInput>,
    mut pointer: ResMut<Pointer>,
) {
    let Some(receiver) = receiver else {
        return;
    };
    let Ok(window) = windows.single() else {
        return;
    };
    if !window.focused
        || !receiver
            .0
            .shared
            .active
            .load(std::sync::atomic::Ordering::Acquire)
    {
        events.clear();
        pointer.held = None;
        return;
    }
    let displayed = receiver
        .0
        .shared
        .displayed
        .lock()
        .ok()
        .and_then(|value| value.clone());
    let Some((epoch, displayed)) = displayed else {
        pointer.held = None;
        return;
    };
    let rect = fit_rect(
        [window.width() as f64, window.height() as f64],
        displayed.logical_size,
    );
    for touch in events.read() {
        let position = InputPosition::new(f64::from(touch.position.x), f64::from(touch.position.y));
        let (route, focus, event) = match touch.phase {
            TouchPhase::Started if pointer.held.is_none() => {
                let Some(route) = displayed.pointer_route(rect, position) else {
                    continue;
                };
                pointer.held = Some((touch.id, route));
                (
                    route,
                    true,
                    InputEventKind::PointerButton {
                        position: Some(position),
                        button: LinuxButtonCode(0x110),
                        state: ButtonState::Pressed,
                    },
                )
            }
            TouchPhase::Moved if pointer.held.is_some_and(|(id, _)| id == touch.id) => {
                let Some((_, route)) = pointer.held else {
                    continue;
                };
                (
                    route,
                    false,
                    InputEventKind::PointerMotion {
                        position,
                        relative: None,
                    },
                )
            }
            TouchPhase::Ended | TouchPhase::Canceled
                if pointer.held.is_some_and(|(id, _)| id == touch.id) =>
            {
                let Some((_, route)) = pointer.held.take() else {
                    continue;
                };
                (
                    route,
                    false,
                    InputEventKind::PointerButton {
                        position: Some(position),
                        button: LinuxButtonCode(0x110),
                        state: ButtonState::Released,
                    },
                )
            }
            _ => continue,
        };
        let time = pointer
            .clock
            .get_or_insert_with(Instant::now)
            .elapsed()
            .as_millis() as u32;
        if !receiver.0.input(Input {
            epoch,
            route,
            position,
            event,
            focus,
            time,
        }) {
            pointer.held = None;
        }
    }
}

mod browser;
mod diagnostics;
mod enrollment;
mod insets;
mod receiver;
mod renderer;
mod reporting;

use crate::geometry::{Viewport, fit_rect};
use bevy::{
    input::touch::TouchPhase,
    prelude::*,
    render::{
        Render, RenderApp, RenderSystems,
        extract_resource::{ExtractResource, ExtractResourcePlugin},
    },
    window::{AppLifecycle, PrimaryWindow, WindowFocused},
    winit::{EventLoopProxyWrapper, UpdateMode, WinitSettings, WinitUserEvent},
};
use receiver::{Input, Session, Shared};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use weld_client::{ClientPointerRoute, InputPosition, MAX_TOUCH_CONTACTS, TouchEvent, TouchId};

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
struct Contacts {
    held: std::collections::HashMap<u64, ClientPointerRoute>,
    clock: Option<Instant>,
    mapping: Option<(u64, [f64; 4], [f64; 2])>,
}

#[derive(Default, Resource)]
struct Presentation {
    viewport: Option<Viewport>,
    changed: Option<Instant>,
}

pub fn install(app: &mut App) {
    browser::install(app);
    // Native input and receiver publication wake presentation immediately.
    // The fallback services viewport settling and asynchronous inset discovery.
    app.insert_resource(WinitSettings {
        focused_mode: UpdateMode::reactive(Duration::from_millis(100)),
        unfocused_mode: UpdateMode::reactive_low_power(Duration::from_secs(1)),
    });
    app.add_plugins(ExtractResourcePlugin::<Stream>::default())
        .init_resource::<Contacts>()
        .init_resource::<Presentation>()
        .init_resource::<insets::Insets>()
        .add_systems(Startup, start)
        .add_systems(Update, (lifecycle, viewport, layout, touch).chain());
    app.sub_app_mut(RenderApp)
        .init_resource::<renderer::VideoRenderer>()
        .add_systems(
            Render,
            renderer::present.in_set(RenderSystems::PrepareResources),
        );
}

fn start(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    proxy: Res<EventLoopProxyWrapper>,
) {
    let proxy = (**proxy).clone();
    let result = bevy::android::ANDROID_APP
        .get()
        .and_then(|app| app.internal_data_path())
        .ok_or_else(|| anyhow::anyhow!("Android private files directory missing"))
        .and_then(|directory| {
            Session::start(directory.join("weld-device"), move || {
                let _ = proxy.send_event(WinitUserEvent::WakeUp);
            })
        });
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
    mut contacts: ResMut<Contacts>,
) {
    let Some(receiver) = receiver else {
        return;
    };
    for event in lifecycle.read() {
        let active = matches!(event, AppLifecycle::Running | AppLifecycle::WillResume);
        receiver.0.set_active(active);
        if !active {
            contacts.held.clear();
        }
    }
    for event in focus.read() {
        if !event.focused {
            contacts.held.clear();
            receiver.0.reset_input();
        }
    }
}

fn viewport(
    receiver: Option<Res<Receiver>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut presentation: ResMut<Presentation>,
    mut status: Query<&mut Node, With<crate::StatusArea>>,
    mut insets: ResMut<insets::Insets>,
) {
    let (Some(receiver), Ok(window), Some(android)) =
        (receiver, windows.single(), bevy::android::ANDROID_APP.get())
    else {
        return;
    };
    let native = android.content_rect();
    let size = [window.physical_width(), window.physical_height()];
    let content = insets.content(size).map(|safe| {
        [
            native.left.max(safe[0]),
            native.top.max(safe[1]),
            native.right.min(safe[2]),
            native.bottom.min(safe[3]),
        ]
    });
    let next =
        content.and_then(|content| Viewport::new(size, content, f64::from(window.scale_factor())));
    if presentation.viewport != next {
        presentation.viewport = next;
        presentation.changed = Some(Instant::now());
        if let Some(viewport) = next {
            info!(?viewport, "phone presentation area changed");
            for mut node in &mut status {
                place(&mut node, viewport.safe);
            }
        }
    }
    if presentation
        .changed
        .is_some_and(|since| since.elapsed() >= Duration::from_millis(150))
    {
        if let Some(viewport) = presentation.viewport {
            receiver.0.set_preference(viewport.preference);
        }
        presentation.changed = None;
    }
}

fn place(node: &mut Node, rect: [f64; 4]) {
    node.left = px(rect[0] as f32);
    node.top = px(rect[1] as f32);
    node.width = px(rect[2] as f32);
    node.height = px(rect[3] as f32);
}

fn layout(
    receiver: Option<Res<Receiver>>,
    presentation: Res<Presentation>,
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
    let displayed = receiver
        .0
        .shared
        .displayed
        .lock()
        .ok()
        .and_then(|value| value.clone());
    for (mut node, mut visible) in &mut panels {
        if !receiver
            .0
            .shared
            .browser
            .load(std::sync::atomic::Ordering::Acquire)
            && let (Some((_, input)), Some(viewport)) = (&displayed, presentation.viewport)
        {
            let rect = fit_rect(viewport.video, input.logical_size);
            let mut next = node.clone();
            place(&mut next, rect);
            node.set_if_neq(next);
            visible.set_if_neq(Visibility::Visible);
        } else {
            visible.set_if_neq(Visibility::Hidden);
        }
    }
}

fn touch(
    receiver: Option<Res<Receiver>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut events: MessageReader<TouchInput>,
    mut contacts: ResMut<Contacts>,
    presentation: Res<Presentation>,
) {
    let Some(receiver) = receiver else {
        return;
    };
    let Ok(window) = windows.single() else {
        return;
    };
    if !window.focused
        || receiver
            .0
            .shared
            .browser
            .load(std::sync::atomic::Ordering::Acquire)
        || !receiver
            .0
            .shared
            .active
            .load(std::sync::atomic::Ordering::Acquire)
    {
        events.clear();
        if !contacts.held.is_empty() {
            receiver.0.reset_input();
        }
        contacts.held.clear();
        return;
    }
    let displayed = receiver
        .0
        .shared
        .displayed
        .lock()
        .ok()
        .and_then(|value| value.clone());
    let (Some((epoch, displayed)), Some(viewport)) = (displayed, presentation.viewport) else {
        events.clear();
        if !contacts.held.is_empty() {
            contacts.held.clear();
            receiver.0.reset_input();
        }
        contacts.mapping = None;
        return;
    };
    let rect = fit_rect(viewport.video, displayed.logical_size);
    let mapping = Some((epoch, rect, displayed.logical_size));
    if contacts.mapping != mapping {
        contacts.mapping = mapping;
        if !contacts.held.is_empty() {
            contacts.held.clear();
            receiver.0.reset_input();
            events.clear();
            return;
        }
    }
    let mut last = None;
    for touch in events.read() {
        let position = InputPosition::new(f64::from(touch.position.x), f64::from(touch.position.y));
        let (route, focus, event) = match touch.phase {
            TouchPhase::Started
                if contacts.held.len() < MAX_TOUCH_CONTACTS
                    && !contacts.held.contains_key(&touch.id) =>
            {
                let Some(route) = displayed.pointer_route(rect, position) else {
                    continue;
                };
                let focus = contacts.held.is_empty();
                contacts.held.insert(touch.id, route);
                (
                    route,
                    focus,
                    TouchEvent::Down {
                        id: TouchId(touch.id),
                        position,
                    },
                )
            }
            TouchPhase::Moved if contacts.held.contains_key(&touch.id) => {
                let Some(route) = contacts.held.get(&touch.id).copied() else {
                    continue;
                };
                (
                    route,
                    false,
                    TouchEvent::Motion {
                        id: TouchId(touch.id),
                        position,
                    },
                )
            }
            TouchPhase::Ended | TouchPhase::Canceled if contacts.held.contains_key(&touch.id) => {
                let Some(route) = contacts.held.remove(&touch.id) else {
                    continue;
                };
                (
                    route,
                    false,
                    if touch.phase == TouchPhase::Canceled {
                        TouchEvent::Cancel
                    } else {
                        TouchEvent::Up {
                            id: TouchId(touch.id),
                        }
                    },
                )
            }
            _ => continue,
        };
        let time = contacts
            .clock
            .get_or_insert_with(Instant::now)
            .elapsed()
            .as_millis() as u32;
        if touch.phase == TouchPhase::Canceled {
            contacts.held.clear();
        }
        if !receiver.0.input(Input {
            epoch,
            route,
            event,
            focus,
            time,
        }) {
            contacts.held.clear();
            return;
        }
        last = Some((route, time));
    }
    if let Some((route, time)) = last
        && !receiver.0.input(Input {
            epoch,
            route,
            event: TouchEvent::Frame,
            focus: false,
            time,
        })
    {
        contacts.held.clear();
    }
}

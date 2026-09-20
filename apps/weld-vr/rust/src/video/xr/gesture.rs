//! A shell gesture owns its press through release, independent of ray hover.
use super::{Sample, alive, controls::Part};
use crate::video::workspace::WeldSurface;
use godot::prelude::*;

struct Hold {
    surface: Gd<WeldSurface>,
    token: (u64, u64),
    workspace_anchor: (Vector3, Vector3),
    kind: Kind,
}
enum Kind {
    Drag { anchor: DragAnchor, grip: bool },
    Close,
    Gamepad,
}

#[derive(Clone, Copy)]
struct DragAnchor {
    position: Vector3,
}
impl DragAnchor {
    fn new(aim: Transform3D, panel: Transform3D) -> Self {
        Self {
            position: aim.affine_inverse() * panel.origin,
        }
    }
    fn pose(self, aim: Transform3D) -> Transform3D {
        // Translation includes manual pushing/pulling. The window placement
        // policy owns orientation, so controller roll cannot tilt the window.
        Transform3D::new(Basis::IDENTITY, aim * self.position)
    }
}
#[derive(Default)]
pub(super) struct Gesture {
    hold: Option<Hold>,
    buttons: Buttons,
    gamepad_clicked: bool,
}
#[derive(Default)]
struct Buttons {
    armed: bool,
    a_down: bool,
    grip_down: bool,
}
impl Buttons {
    fn step(&mut self, a: bool, grip: f32) -> (bool, bool) {
        if !self.armed {
            self.a_down = a;
            self.grip_down = grip > 0.35;
            self.armed = true;
            return (false, false);
        }
        let grip = grip >= if self.grip_down { 0.35 } else { 0.75 };
        let edges = (a && !self.a_down, grip && !self.grip_down);
        self.a_down = a;
        self.grip_down = grip;
        edges
    }
}
impl Gesture {
    pub fn discard_deleted_target(&mut self) {
        if self
            .hold
            .as_ref()
            .is_some_and(|hold| !alive(&hold.surface.bind().player))
        {
            self.reset();
        }
    }
    pub fn take_gamepad_click(&mut self) -> bool {
        std::mem::take(&mut self.gamepad_clicked)
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn owns(&self, sample: &Sample) -> bool {
        self.hold.as_ref().is_some_and(|hold| {
            sample.surface.as_ref() == Some(&hold.surface) && hold.token == sample.token
        })
    }
    pub fn active(&self) -> bool {
        self.hold.is_some()
    }
    pub fn step(&mut self, sample: &Sample, aim: Transform3D) -> bool {
        let a = sample.analog[0] > 0.5;
        let (new_a, new_grip) = self.buttons.step(a, sample.grip);
        let grip = self.buttons.grip_down;
        if let Some(mut hold) = self.hold.take() {
            if hold.token != sample.token
                || sample.surface.as_ref() != Some(&hold.surface)
                || hold.workspace_anchor != hold.surface.bind().workspace_anchor()
            {
                self.buttons = Buttons::default();
                return true;
            }
            let held = match hold.kind {
                Kind::Drag {
                    anchor,
                    grip: by_grip,
                } => {
                    let held = if by_grip { grip } else { a };
                    if held && aim.is_finite() {
                        hold.surface.bind_mut().move_to(anchor.pose(aim));
                    }
                    held
                }
                Kind::Close => {
                    if !a
                        && sample.chrome == Some(Part::Close)
                        && let Some(controller) = hold.surface.bind().player.bind().xr_controller()
                    {
                        controller.close_window();
                    }
                    a
                }
                Kind::Gamepad => {
                    self.gamepad_clicked = !a && sample.chrome == Some(Part::Gamepad);
                    a
                }
            };
            if held {
                self.hold = Some(hold);
            }
            return true;
        }
        let Some(surface) = sample
            .surface
            .as_ref()
            .filter(|surface| surface.bind().movable())
        else {
            return false;
        };
        let kind = if new_grip && (sample.hit || sample.chrome.is_some()) {
            Some(Kind::Drag {
                anchor: DragAnchor::new(aim, sample.panel.get_global_transform()),
                grip: true,
            })
        } else if new_a {
            match sample.chrome {
                Some(Part::Drag) => Some(Kind::Drag {
                    anchor: DragAnchor::new(aim, sample.panel.get_global_transform()),
                    grip: false,
                }),
                Some(Part::Close) => Some(Kind::Close),
                Some(Part::Gamepad) => Some(Kind::Gamepad),
                None => None,
            }
        } else {
            None
        };
        if let Some(kind) = kind {
            self.hold = Some(Hold {
                surface: surface.clone(),
                token: sample.token,
                workspace_anchor: surface.bind().workspace_anchor(),
                kind,
            });
            return true;
        }
        sample.chrome.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drag_keeps_anchor_and_allows_manual_push_while_layout_owns_rotation() {
        let start = Transform3D::new(Basis::IDENTITY, Vector3::new(0.2, 1.0, 0.0));
        let panel = Transform3D::new(
            Basis::from_axis_angle(Vector3::RIGHT, 0.15),
            Vector3::new(0.0, 1.3, -1.6),
        );
        let anchor = DragAnchor::new(start, panel);
        assert!((anchor.pose(start).origin - panel.origin).length() < 1e-6);
        let moved = Transform3D::new(
            Basis::from_axis_angle(Vector3::UP, 0.4),
            start.origin + Vector3::RIGHT,
        );
        let pose = anchor.pose(moved);
        assert_eq!(pose.basis, Basis::IDENTITY);
        assert!((pose.origin - panel.origin).length() > 0.1);
        assert_eq!(anchor.pose(moved), pose);
        let pushed = Transform3D::new(start.basis, start.origin + Vector3::FORWARD * 0.5);
        assert!(
            (anchor.pose(pushed).origin - panel.origin - Vector3::FORWARD * 0.5).length() < 1e-5
        );
    }
    #[test]
    fn shell_buttons_require_release_after_startup_and_emit_only_press_edges() {
        let mut buttons = Buttons::default();
        assert_eq!(buttons.step(true, 0.5), (false, false));
        assert_eq!(buttons.step(true, 1.0), (false, false));
        assert_eq!(buttons.step(false, 0.0), (false, false));
        assert_eq!(buttons.step(true, 1.0), (true, true));
        assert_eq!(buttons.step(true, 0.5), (false, false));
        assert!(buttons.grip_down);
        assert_eq!(buttons.step(false, 0.0), (false, false));
        assert!(!buttons.grip_down);
    }
}

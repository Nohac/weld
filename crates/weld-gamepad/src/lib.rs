//! Linux uinput output for an explicitly enabled remote controller. This device
//! is host-visible, not isolated to a Wayland client or a gaming sandbox.
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    UinputAbsSetup, uinput::VirtualDevice,
};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use std::{io, path::PathBuf};
use weld_hoist_core::{
    HoistPortResult,
    gamepad::{GamepadDevice, GamepadProvider, GamepadState},
};

pub struct UinputGamepadProvider;
impl GamepadProvider for UinputGamepadProvider {
    fn open(&mut self) -> HoistPortResult<Box<dyn GamepadDevice>> {
        Ok(Box::new(UinputGamepad::create()?))
    }
}

const KEYS: [KeyCode; 11] = [
    KeyCode::BTN_SOUTH,
    KeyCode::BTN_EAST,
    KeyCode::BTN_WEST,
    KeyCode::BTN_NORTH,
    KeyCode::BTN_TL,
    KeyCode::BTN_TR,
    KeyCode::BTN_SELECT,
    KeyCode::BTN_START,
    KeyCode::BTN_MODE,
    KeyCode::BTN_THUMBL,
    KeyCode::BTN_THUMBR,
];
const AXES: [AbsoluteAxisCode; 8] = [
    AbsoluteAxisCode::ABS_X,
    AbsoluteAxisCode::ABS_Y,
    AbsoluteAxisCode::ABS_RX,
    AbsoluteAxisCode::ABS_RY,
    AbsoluteAxisCode::ABS_Z,
    AbsoluteAxisCode::ABS_RZ,
    AbsoluteAxisCode::ABS_HAT0X,
    AbsoluteAxisCode::ABS_HAT0Y,
];

/// Closing the final uinput descriptor destroys the kernel device. The relay
/// additionally emits neutral state before dropping it on normal teardown.
pub struct UinputGamepad {
    device: VirtualDevice,
    previous: GamepadState,
    events: Vec<InputEvent>,
}
impl UinputGamepad {
    pub fn create() -> io::Result<Self> {
        let keys: AttributeSet<KeyCode> = KEYS.into_iter().collect();
        // Conventional xpad layout/GUID enables SDL's built-in gamepad mapping.
        let mut builder = VirtualDevice::builder()?
            .name("Weld XR Gamepad")
            .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0110))
            .with_keys(&keys)?;
        for (index, axis) in AXES.into_iter().enumerate() {
            let (minimum, maximum) = match index {
                0..4 => (-32768, 32767),
                4..6 => (0, 65535),
                _ => (-1, 1),
            };
            builder = builder.with_absolute_axis(&UinputAbsSetup::new(
                axis,
                AbsInfo::new(0, minimum, maximum, 0, 0, 0),
            ))?;
        }
        let device = builder.build()?;
        fcntl_setfl(&device, fcntl_getfl(&device)? | OFlags::NONBLOCK)?;
        Ok(Self {
            device,
            previous: GamepadState::default(),
            events: Vec::with_capacity(19),
        })
    }
    /// Resolve only this device's event nodes for an explicit readback probe.
    pub fn event_nodes(&mut self) -> io::Result<Vec<PathBuf>> {
        self.device.enumerate_dev_nodes_blocking()?.collect()
    }
    pub fn write_state(&mut self, state: GamepadState) -> io::Result<()> {
        self.events.clear();
        for ((key, old), value) in KEYS
            .into_iter()
            .zip(button_values(self.previous))
            .zip(button_values(state))
        {
            if old != value {
                self.events
                    .push(InputEvent::new(EventType::KEY.0, key.0, i32::from(value)));
            }
        }
        for ((axis, old), value) in AXES
            .into_iter()
            .zip(axis_values(self.previous))
            .zip(axis_values(state))
        {
            if old != value {
                self.events
                    .push(InputEvent::new(EventType::ABSOLUTE.0, axis.0, value));
            }
        }
        if !self.events.is_empty() {
            self.device.emit(&self.events)?;
        }
        self.previous = state;
        Ok(())
    }
}
impl GamepadDevice for UinputGamepad {
    fn update(&mut self, state: GamepadState) -> HoistPortResult<()> {
        Ok(self.write_state(state)?)
    }
}
fn button_values(state: GamepadState) -> [bool; 11] {
    let buttons = state.buttons;
    [
        buttons.south,
        buttons.east,
        buttons.west,
        buttons.north,
        buttons.left_shoulder,
        buttons.right_shoulder,
        buttons.select,
        buttons.start,
        buttons.guide,
        buttons.left_stick,
        buttons.right_stick,
    ]
}
fn axis_values(state: GamepadState) -> [i32; 8] {
    [
        i32::from(state.left[0]),
        i32::from(state.left[1]),
        i32::from(state.right[0]),
        i32::from(state.right[1]),
        i32::from(state.triggers[0]),
        i32::from(state.triggers[1]),
        state.dpad[0].value(),
        state.dpad[1].value(),
    ]
}

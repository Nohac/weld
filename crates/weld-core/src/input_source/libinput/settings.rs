//! Live libinput configuration, reapplied on hotplug and preference changes.

use smithay::reexports::input::{AsRaw, ClickMethod, Device, DeviceConfigError, TapButtonMap, ffi};
use tracing::{debug, warn};
use weld_input::{
    FingerButtonMap, PointerDeviceSettings, TouchpadClickMethod, TouchpadPreferences,
};

#[derive(Default)]
pub(super) struct PointerDevices {
    settings: PointerDeviceSettings,
    devices: Vec<ConfiguredDevice>,
}

struct ConfiguredDevice {
    device: Device,
    identifier: String,
    applied: Option<TouchpadPreferences>,
}

impl PointerDevices {
    pub(super) fn add(&mut self, device: Device) {
        let name = device.name();
        let name = name
            .trim()
            .chars()
            .map(|c| if c == ' ' || c.is_control() { '_' } else { c })
            .collect::<String>();
        let identifier = format!("{}:{}:{name}", device.id_vendor(), device.id_product());
        debug!(%identifier, "registered native input device");
        let mut device = ConfiguredDevice {
            device,
            identifier,
            applied: None,
        };
        device.apply(&self.settings);
        self.devices.push(device);
    }

    pub(super) fn remove(&mut self, device: &Device) {
        self.devices.retain(|entry| entry.device != *device);
    }

    pub(super) fn configure(&mut self, settings: PointerDeviceSettings) {
        if self.settings == settings {
            return;
        }
        self.settings = settings;
        for device in &mut self.devices {
            device.apply(&self.settings);
        }
    }
}

impl ConfiguredDevice {
    fn apply(&mut self, settings: &PointerDeviceSettings) {
        let touchpad = self.device.config_tap_finger_count() > 0;
        let preferences = settings.resolve(&self.identifier, touchpad);
        if self.applied == Some(preferences) {
            return;
        }
        if touchpad {
            let mapping = match preferences.tap_button_map {
                Some(FingerButtonMap::LeftRightMiddle) => Some(TapButtonMap::LeftRightMiddle),
                Some(FingerButtonMap::LeftMiddleRight) => Some(TapButtonMap::LeftMiddleRight),
                None => self.device.config_tap_default_button_map(),
            };
            if let Some(mapping) = mapping {
                let result = self.device.config_tap_set_button_map(mapping);
                report(&self.identifier, "tap_button_map", result);
            }
            let enabled = preferences
                .tap
                .unwrap_or_else(|| self.device.config_tap_default_enabled());
            let result = self.device.config_tap_set_enabled(enabled);
            report(&self.identifier, "tap", result);
        } else if preferences.tap.is_some() || preferences.tap_button_map.is_some() {
            warn!(device = %self.identifier, "device does not support tap configuration");
        }
        let supported = self.device.config_click_methods();
        if !supported.is_empty() {
            let method = match preferences.click_method {
                Some(TouchpadClickMethod::ButtonAreas) => Some(ClickMethod::ButtonAreas),
                Some(TouchpadClickMethod::Clickfinger) => Some(ClickMethod::Clickfinger),
                Some(TouchpadClickMethod::None) => None,
                None => self.device.config_click_default_method(),
            };
            let result = match method {
                Some(method) => self.device.config_click_set_method(method),
                None => disable_click_method(&mut self.device),
            };
            report(&self.identifier, "click_method", result);
        } else if preferences.click_method.is_some() {
            warn!(device = %self.identifier, "device does not support configurable click methods");
        }
        self.applied = Some(preferences);
        debug!(device = %self.identifier, ?preferences, "applied native pointer preferences");
    }
}

fn report(identifier: &str, setting: &str, result: Result<(), DeviceConfigError>) {
    if let Err(error) = result {
        warn!(
            device = identifier,
            setting,
            ?error,
            "could not apply input setting"
        );
    }
}

fn disable_click_method(device: &mut Device) -> Result<(), DeviceConfigError> {
    // input 0.10's safe setter accepts only enabled methods. The native API also
    // accepts NONE, needed for explicit disable and restoring a NONE default.
    // SAFETY: Device owns a live libinput reference; the setter borrows it for
    // this call on the host thread and retains no Rust memory.
    let status = unsafe {
        ffi::libinput_device_config_click_set_method(
            device.as_raw_mut(),
            ffi::libinput_config_click_method_LIBINPUT_CONFIG_CLICK_METHOD_NONE,
        )
    };
    match status {
        ffi::libinput_config_status_LIBINPUT_CONFIG_STATUS_SUCCESS => Ok(()),
        ffi::libinput_config_status_LIBINPUT_CONFIG_STATUS_UNSUPPORTED => {
            Err(DeviceConfigError::Unsupported)
        }
        _ => Err(DeviceConfigError::Invalid),
    }
}

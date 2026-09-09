//! Reloadable legacy-repeat fallback, separate from immutable cadence ownership.

use bevy::{
    app::App,
    ecs::{resource::Resource, world::World},
};
use weld_core::runtime::HostCommand;

pub use weld_core::input::{KeyboardRepeatMode, LegacyKeyRepeat};

/// Live keyboard compatibility policy. Replace this resource to update the host.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Resource)]
pub struct KeyboardSettings {
    pub legacy_repeat: LegacyKeyRepeat,
}

#[derive(Default, Resource)]
struct PublishedKeyboardSettings(Option<KeyboardSettings>);

pub(super) fn register(app: &mut App) {
    app.init_resource::<KeyboardSettings>()
        .init_resource::<PublishedKeyboardSettings>();
}

pub(super) fn take_settings_command(world: &mut World) -> Option<HostCommand> {
    let settings = *world.get_resource::<KeyboardSettings>()?;
    let mut published = world.get_resource_mut::<PublishedKeyboardSettings>()?;
    if published.0 == Some(settings) {
        return None;
    }
    published.0 = Some(settings);
    Some(HostCommand::SetLegacyKeyRepeat(settings.legacy_repeat))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_setting_publishes_only_initial_and_changed_values() {
        let mut app = App::new();
        register(&mut app);
        assert_eq!(
            take_settings_command(app.world_mut()),
            Some(HostCommand::SetLegacyKeyRepeat(LegacyKeyRepeat::Client))
        );
        assert_eq!(take_settings_command(app.world_mut()), None);
        for legacy_repeat in [
            LegacyKeyRepeat::Disabled,
            LegacyKeyRepeat::Emulated,
            LegacyKeyRepeat::Client,
        ] {
            app.insert_resource(KeyboardSettings { legacy_repeat });
            assert_eq!(
                take_settings_command(app.world_mut()),
                Some(HostCommand::SetLegacyKeyRepeat(legacy_repeat))
            );
            assert_eq!(take_settings_command(app.world_mut()), None);
        }
    }
}

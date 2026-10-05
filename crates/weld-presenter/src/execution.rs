use bevy_app::{App, Plugin, PluginGroupBuilder};
use bevy_ecs::schedule::{Schedules, SingleThreadedExecutor};
use bevy_render::pipelined_rendering::PipelinedRenderingPlugin;

/// Assemble non-pipelined rendering whether or not Bevy's build enables its
/// optional pipelined plugin. Native import and sampling share one context.
pub fn presentation_plugins(mut plugins: PluginGroupBuilder) -> PluginGroupBuilder {
    if plugins.contains::<PipelinedRenderingPlugin>() {
        plugins = plugins.disable::<PipelinedRenderingPlugin>();
    }
    plugins.add(PresentationPlugin)
}

/// Explicit sequential-executor guard if a consumer enables Bevy multithreading.
/// Both current hosts omit that feature, also keeping internal parallel queries
/// sequential. Decode and transport workers retain their independent executors.
pub struct PresentationPlugin;

impl Plugin for PresentationPlugin {
    fn build(&self, _app: &mut App) {}

    fn finish(&self, app: &mut App) {
        for sub_app in app.sub_apps_mut().iter_mut() {
            if let Some(mut schedules) = sub_app.world_mut().get_resource_mut::<Schedules>() {
                for (_, schedule) in schedules.iter_mut() {
                    schedule.set_executor(SingleThreadedExecutor::new());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_app::PluginGroup;

    struct EmptyGroup;
    impl PluginGroup for EmptyGroup {
        fn build(self) -> PluginGroupBuilder {
            PluginGroupBuilder::start::<Self>()
        }
    }

    #[test]
    fn optional_pipeline_can_be_present_or_absent_in_the_feature_graph() {
        for plugins in [
            EmptyGroup.build(),
            EmptyGroup.build().add(PipelinedRenderingPlugin),
        ] {
            let plugins = presentation_plugins(plugins);
            assert!(!plugins.enabled::<PipelinedRenderingPlugin>());
            assert!(plugins.enabled::<PresentationPlugin>());
        }
    }
}

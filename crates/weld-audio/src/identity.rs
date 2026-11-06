//! Associate audio with the same host-local launch ID stamped by Wayland listeners.
use crate::CaptureTarget;
use anyhow::{Context, Result};
use pipewire::{keys, properties::PropertiesBox, spa::utils::dict::DictRef};
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr},
    process::Command,
    ptr::NonNull,
};

/// Weld-owned client/stream property inherited by a launch's audio helpers.
/// This is an association hint in the user's audio session, not authentication.
pub const LAUNCH_PROPERTY: &str = "weld.launch.id";

pub(crate) fn launch_tag(launch: u64) -> String {
    format!("weld.{}.launch.{launch:x}", std::process::id())
}

/// Add the Wayland listener's launch identity to native and Pulse-compatible
/// audio properties before spawning. Existing routing and application properties
/// are preserved; a nested Weld launch replaces its inherited launch identity.
pub fn attribute_launch(command: &mut Command, launch: u64) -> Result<()> {
    let inherited = command
        .get_envs()
        .find(|(key, _)| *key == OsStr::new("PIPEWIRE_PROPS"))
        .map(|(_, value)| value.map(OsStr::to_owned))
        .unwrap_or_else(|| std::env::var_os("PIPEWIRE_PROPS"));
    let properties = attributed_properties(inherited.as_deref(), &launch_tag(launch))?;
    command
        .env("PIPEWIRE_PROPS", properties)
        .env(format!("PULSE_PROP_{LAUNCH_PROPERTY}"), launch_tag(launch));
    Ok(())
}

fn attributed_properties(inherited: Option<&OsStr>, tag: &str) -> Result<String> {
    let mut properties = if let Some(inherited) = inherited {
        let text = CString::new(
            inherited
                .to_str()
                .context("PipeWire properties must be UTF-8")?,
        )
        .context("audio properties contain a null byte")?;
        // SAFETY: text is NUL-terminated and lives through the call. PipeWire
        // copies the parsed data and returns a new allocation or null. The Rust
        // bindings do not yet expose this SPA-JSON parser through a safe method.
        let raw = unsafe { pipewire::sys::pw_properties_new_string(text.as_ptr()) };
        let raw = NonNull::new(raw).context("could not parse inherited PipeWire properties")?;
        // SAFETY: raw is a non-null, uniquely owned pw_properties allocation;
        // PropertiesBox takes responsibility for freeing it exactly once.
        unsafe { PropertiesBox::from_raw(raw) }
    } else {
        PropertiesBox::new()
    };
    properties.insert(LAUNCH_PROPERTY, tag);
    let values: BTreeMap<_, _> = properties.dict().iter().collect();
    serde_json::to_string(&values).context("serializing attributed audio properties")
}

#[derive(Default)]
pub(crate) struct StreamIdentity {
    pub process: Option<u32>,
    pub launch: Option<String>,
}
impl StreamIdentity {
    pub fn from_properties(props: &DictRef) -> Self {
        Self {
            process: props
                .get(*keys::APP_PROCESS_ID)
                .or_else(|| props.get(*keys::SEC_PID))
                .and_then(|value| value.parse().ok()),
            launch: props.get(LAUNCH_PROPERTY).map(str::to_owned),
        }
    }

    /// Match the stream's explicit identity, consulting its owning client only
    /// when the stream omits that field. A conflicting stream value wins over
    /// the client's value. Attributed targets require the inherited launch tag;
    /// missing/mismatched tags never fall back to a PID or application name.
    /// Unattributed targets require exact PID equality. This deliberately avoids
    /// inferring ownership from process ancestry, which can change on exit or
    /// reparenting. PID metadata is a current association hint, not a process
    /// lifetime handle; unavailable identifiers leave the stream unmatched.
    /// Called on registry/property changes, never during audio sample processing.
    pub fn belongs_to(&self, target: &CaptureTarget, client: Option<&Self>) -> bool {
        match target {
            CaptureTarget::Launch(expected) => {
                self.launch
                    .as_deref()
                    .or_else(|| client.and_then(|client| client.launch.as_deref()))
                    == Some(expected.as_str())
            }
            CaptureTarget::Process(expected) => {
                self.process
                    .or_else(|| client.and_then(|client| client.process))
                    == Some(*expected)
            }
            CaptureTarget::Output(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_identity_matches_helpers_and_rejects_pid_fallback_or_conflicts() {
        let target = CaptureTarget::Launch(launch_tag(7));
        let client = StreamIdentity {
            process: Some(10),
            launch: Some(launch_tag(7)),
        };
        let helper = StreamIdentity {
            process: Some(99),
            launch: None,
        };
        assert!(helper.belongs_to(&target, Some(&client)));
        assert!(!helper.belongs_to(&target, None));
        assert!(
            !StreamIdentity {
                process: Some(10),
                launch: Some(launch_tag(8))
            }
            .belongs_to(&target, Some(&client))
        );
        assert!(
            !StreamIdentity {
                process: Some(10),
                launch: None
            }
            .belongs_to(&target, None)
        );
        assert!(StreamIdentity::default().belongs_to(&CaptureTarget::Process(10), Some(&client)));
        assert!(!helper.belongs_to(&CaptureTarget::Process(10), Some(&client)));
    }
    #[test]
    fn nested_launch_replaces_identity_without_changing_other_environment() -> Result<()> {
        let mut command = Command::new("unused-test-command");
        command
            .env(
                "PIPEWIRE_PROPS",
                r#"target.object=private application.name="Keep this label""#,
            )
            .env("PULSE_PROP", "media.role=music")
            .env("PULSE_SINK", "private");
        attribute_launch(&mut command, 1)?;
        attribute_launch(&mut command, 2)?;
        let environment: BTreeMap<_, _> = command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect();
        let native: serde_json::Value = serde_json::from_str(&environment["PIPEWIRE_PROPS"])?;
        assert_eq!(native[LAUNCH_PROPERTY], launch_tag(2));
        assert_eq!(native[*keys::TARGET_OBJECT], "private");
        assert_eq!(native[*keys::APP_NAME], "Keep this label");
        assert_eq!(
            environment[&format!("PULSE_PROP_{LAUNCH_PROPERTY}")],
            launch_tag(2)
        );
        assert_eq!(environment["PULSE_PROP"], "media.role=music");
        assert_eq!(environment["PULSE_SINK"], "private");
        Ok(())
    }
    #[test]
    fn launch_tag_preserves_routing_and_quoted_properties_and_replaces_old_tag() -> Result<()> {
        let inherited = OsStr::new(
            r#"{ target.object = "private" application.name = "Player with spaces" weld.launch.id = old }"#,
        );
        // PipeWire parses its own SPA-JSON syntax; serialized output is ordinary JSON.
        let json: serde_json::Value =
            serde_json::from_str(&attributed_properties(Some(inherited), "new")?)?;
        assert_eq!(json[*keys::TARGET_OBJECT], "private");
        assert_eq!(json[*keys::APP_NAME], "Player with spaces");
        assert_eq!(json[LAUNCH_PROPERTY], "new");
        Ok(())
    }
}

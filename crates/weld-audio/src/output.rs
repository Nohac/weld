use std::{
    collections::HashMap,
    ffi::{CStr, OsString},
    sync::{Arc, Mutex, OnceLock, Weak},
};
static OUTPUTS: OnceLock<Mutex<HashMap<u64, Weak<Output>>>> = OnceLock::new();
use crate::worker::Worker;
use anyhow::Result;
use pipewire::{keys, node::Node, properties::properties, spa};

/// A private output remains alive until the last launch/capture owner drops it.
/// Its node is never linked to hardware. Listening requires a separate playback stream.
#[derive(Clone)]
pub struct PrivateOutput(Arc<Output>);
struct Output {
    name: String,
    _worker: Worker,
}
impl std::fmt::Debug for PrivateOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PrivateOutput").field(&self.0.name).finish()
    }
}
impl PrivateOutput {
    pub fn new(launch: u64) -> Result<Self> {
        anyhow::ensure!(
            !OUTPUTS
                .get_or_init(Default::default)
                .lock()
                .map_err(|_| anyhow::anyhow!("audio output directory unavailable"))?
                .get(&launch)
                .is_some_and(|output| output.strong_count() > 0),
            "private audio output identity already exists"
        );
        let name = output_name(launch);
        let node_name = name.clone();
        let worker = Worker::spawn(move |core, _| {
            let audio_position =
                CStr::from_bytes_with_nul(spa::sys::SPA_KEY_AUDIO_POSITION)?.to_str()?;
            Ok(core.create_object::<Node>(
                "adapter",
                &properties! {
                    *keys::FACTORY_NAME => "support.null-audio-sink",
                    *keys::NODE_NAME => node_name,
                    *keys::NODE_DESCRIPTION => "Weld private application audio",
                    *keys::MEDIA_CLASS => "Audio/Sink",
                    audio_position => "[ FL FR ]",
                    *keys::AUDIO_RATE => "48000",
                    *keys::OBJECT_LINGER => "false",
                    *keys::NODE_VIRTUAL => "true",
                    *keys::PRIORITY_SESSION => "0",
                    "monitor.channel-volumes" => "false",
                },
            )?)
        })?;
        worker.wait_ready()?;
        let output = Arc::new(Output {
            name,
            _worker: worker,
        });
        let mut outputs = OUTPUTS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| anyhow::anyhow!("audio output directory unavailable"))?;
        outputs.retain(|_, output| output.strong_count() > 0);
        anyhow::ensure!(
            !outputs.contains_key(&launch),
            "private audio output identity already exists"
        );
        outputs.insert(launch, Arc::downgrade(&output));
        Ok(Self(output))
    }
    pub fn name(&self) -> &str {
        &self.0.name
    }
    /// Hints for native and Pulse-compatible clients, applied before process creation.
    /// An application that explicitly selects a different device can override hints.
    pub fn environment(&self) -> Vec<(OsString, OsString)> {
        routing_environment(self.name())
    }
}
pub fn output_name(launch: u64) -> String {
    format!("weld.{}.launch.{launch:x}", std::process::id())
}

/// Resolve a host-owned launch output before falling back to process capture.
pub fn application_target(process: u32, launch: Option<u64>) -> crate::CaptureTarget {
    let output = launch.and_then(|id| OUTPUTS.get()?.lock().ok()?.get(&id)?.upgrade());
    output.map_or(crate::CaptureTarget::Process(process), |output| {
        crate::CaptureTarget::Output(output.name.clone())
    })
}

/// Launch defaults for an output, including a missing output when audio is unavailable.
/// Missing private targets fail connection instead of selecting the default speakers.
pub fn routing_environment(name: &str) -> Vec<(OsString, OsString)> {
    let target_object = *keys::TARGET_OBJECT;
    let dont_reconnect = *keys::NODE_DONT_RECONNECT;
    vec![
        ("PULSE_SINK".into(), name.into()),
        ("PIPEWIRE_NODE".into(), name.into()),
        (
            "PULSE_PROP".into(),
            format!(
                "{target_object}={name} node.dont-fallback=true {dont_reconnect}=true"
            )
            .into(),
        ),
        (
            "PIPEWIRE_PROPS".into(),
            format!(
                "{{ {target_object} = \"{name}\" node.dont-fallback = true {dont_reconnect} = true }}"
            )
            .into(),
        ),
    ]
}

use anyhow::{Context, Result};
use pipewire::spa::{
    self,
    param::audio::{AudioFormat, AudioInfoRaw},
    pod::{Object, Value, serialize::PodSerializer},
};
use weld_media::audio::{CHANNELS, SAMPLE_RATE};

pub(crate) fn format() -> Result<Vec<u8>> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::S16LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS);
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    positions[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    positions[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(positions);
    Ok(PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )
    .context("serializing audio format")?
    .0
    .into_inner())
}

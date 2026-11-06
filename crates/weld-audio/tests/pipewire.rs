use anyhow::{Result, ensure};
use pipewire::keys;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use weld_audio::{
    Capture, CaptureTarget, Playback, PrivateOutput, application_target, attribute_launch,
};
use weld_media::audio::AudioPacket;

#[test]
#[ignore = "requires a running PipeWire session; routes only to a private sink"]
fn private_output_capture_and_playback() -> Result<()> {
    let output = PrivateOutput::new(0xabcdef)?;
    assert_eq!(
        application_target(None, Some(0xabcdef)),
        Some(CaptureTarget::Output(output.name().into()))
    );
    let origin = Instant::now();
    let capture = Capture::new(
        CaptureTarget::Output(output.name().into()),
        origin,
        Arc::new(|| {}),
    )?;
    let second = Capture::new(
        CaptureTarget::Output(output.name().into()),
        origin,
        Arc::new(|| {}),
    )?;
    let by_process = Capture::new(
        CaptureTarget::Process(std::process::id()),
        origin,
        Arc::new(|| {}),
    )?;
    let mut process_nonzero = 0;
    let mut second_nonzero = 0;
    let playback = Playback::new(Some(output.name().into()))?;
    let stalled = Capture::new(
        CaptureTarget::Output(output.name().into()),
        origin,
        Arc::new(|| {}),
    )?;
    let mut first_time = None;
    let mut last_time = 0;
    let mut nonzero = 0;
    let mut packets = 0;
    for index in 0..150 {
        let samples = (0..960)
            .flat_map(|sample| {
                let phase =
                    (index * 960 + sample) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0;
                let value = (phase.sin() * 1000.0) as i16;
                let bytes = value.to_le_bytes();
                [bytes[0], bytes[1], bytes[0], bytes[1]]
            })
            .collect();
        playback.push(AudioPacket {
            stream: 1,
            timestamp_micros: index * 20_000,
            samples,
        })?;
        std::thread::sleep(Duration::from_millis(20));
        for packet in by_process.drain()? {
            process_nonzero += packet.samples.iter().filter(|byte| **byte != 0).count();
        }
        for packet in second.drain()? {
            second_nonzero += packet.samples.iter().filter(|byte| **byte != 0).count();
        }
        for packet in capture.drain()? {
            ensure!(packet.is_valid(), "invalid capture");
            first_time.get_or_insert(packet.timestamp_micros);
            last_time = packet.timestamp_micros;
            nonzero += packet.samples.iter().filter(|byte| **byte != 0).count();
            packets += 1;
        }
    }
    ensure!(
        packets > 10 && nonzero > 1000,
        "captured {packets} packets, {nonzero} nonzero bytes"
    );
    ensure!(
        process_nonzero > 1000,
        "process stream discovery did not capture sound: {process_nonzero}"
    );
    ensure!(
        second_nonzero > 1000,
        "shared capture subscriber did not receive sound"
    );
    ensure!(
        last_time > first_time.unwrap_or_default() + 2_000_000,
        "capture timestamps did not progress"
    );
    ensure!(
        last_time < origin.elapsed().as_micros() as u64 + 100_000,
        "capture timestamps are ahead of the recording clock"
    );
    ensure!(
        stalled.dropped_packets() > 0,
        "stalled subscriber was not bounded"
    );
    ensure!(capture.dropped_packets() == 0, "capture lost packets");
    // Passive capture and drained playback should stop processing silence.
    std::thread::sleep(Duration::from_millis(350));
    capture.drain()?;
    std::thread::sleep(Duration::from_millis(100));
    ensure!(
        capture.drain()?.is_empty(),
        "idle private audio graph kept processing"
    );
    playback.push(AudioPacket {
        stream: 1,
        timestamp_micros: 3_500_000,
        samples: vec![5; 3840],
    })?;
    std::thread::sleep(Duration::from_millis(150));
    ensure!(
        capture
            .drain()?
            .iter()
            .any(|packet| packet.samples.iter().any(|byte| *byte != 0)),
        "short sound did not resume idle playback"
    );
    Ok(())
}

#[test]
#[ignore = "requires PipeWire, WirePlumber, paplay and pw-play; plays only silence"]
fn launch_environment_routes_native_and_pulse_clients() -> Result<()> {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let output = PrivateOutput::new(0xabcdef01)?;
    let mut file = tempfile::Builder::new().suffix(".wav").tempfile()?;
    let length: u32 = 48_000 * 4 * 3;
    file.write_all(b"RIFF")?;
    file.write_all(&(length + 36).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16_u32.to_le_bytes())?;
    file.write_all(&1_u16.to_le_bytes())?;
    file.write_all(&2_u16.to_le_bytes())?;
    file.write_all(&48_000_u32.to_le_bytes())?;
    file.write_all(&192_000_u32.to_le_bytes())?;
    file.write_all(&4_u16.to_le_bytes())?;
    file.write_all(&16_u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&length.to_le_bytes())?;
    file.write_all(&vec![0; length as usize])?;
    for (index, executable) in ["pw-play", "paplay"].into_iter().enumerate() {
        // This launch deliberately has no registered private output of its own:
        // capture must associate the child through the inherited tag alone.
        let launch = 0xabcdef02 + index as u64;
        let target =
            application_target(Some(std::process::id()), Some(launch)).expect("launch target");
        assert!(matches!(&target, CaptureTarget::Launch(_)));
        let capture = Capture::new(target, Instant::now(), Arc::new(|| {}))?;
        let wrong = Capture::new(
            application_target(None, Some(launch + 100)).expect("other launch"),
            Instant::now(),
            Arc::new(|| {}),
        )?;
        let mut command = Command::new(executable);
        command
            .arg(file.path())
            .envs(output.environment())
            .stdout(Stdio::null());
        attribute_launch(&mut command, launch)?;
        let mut child = command.spawn()?;
        let result = (|| -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut identity_seen = Vec::new();
            while Instant::now() < deadline {
                let dump = Command::new("pw-dump").output()?;
                let objects: Vec<serde_json::Value> = serde_json::from_slice(&dump.stdout)?;
                let props = |value: &serde_json::Value| value["info"]["props"].clone();
                let sink = objects
                    .iter()
                    .find(|value| props(value)[*keys::NODE_NAME] == output.name())
                    .and_then(|value| value["id"].as_u64());
                let clients: Vec<_> = objects
                    .iter()
                    .filter(|value| {
                        props(value)[*keys::APP_PROCESS_ID].as_u64() == Some(u64::from(child.id()))
                    })
                    .filter_map(|value| value["id"].as_u64())
                    .collect();
                let nodes: Vec<_> = objects
                    .iter()
                    .filter(|value| {
                        props(value)[*keys::MEDIA_CLASS] == "Stream/Output/Audio"
                            && (props(value)[*keys::APP_PROCESS_ID].as_u64()
                                == Some(u64::from(child.id()))
                                || props(value)[*keys::CLIENT_ID]
                                    .as_u64()
                                    .is_some_and(|id| clients.contains(&id)))
                    })
                    .filter_map(|value| value["id"].as_u64())
                    .collect();
                identity_seen = objects
                    .iter()
                    .filter(|value| {
                        nodes.contains(&value["id"].as_u64().unwrap_or_default())
                            || clients.contains(&value["id"].as_u64().unwrap_or_default())
                    })
                    .map(|value| {
                        (
                            value["id"].clone(),
                            props(value)[weld_audio::LAUNCH_PROPERTY].clone(),
                        )
                    })
                    .collect();
                let links: Vec<_> = objects
                    .iter()
                    .filter(|value| {
                        value["info"]["output-node-id"]
                            .as_u64()
                            .is_some_and(|id| nodes.contains(&id))
                            && objects.iter().any(|node| {
                                node["id"] == value["info"]["input-node-id"]
                                    && props(node)[*keys::MEDIA_CLASS] == "Audio/Sink"
                            })
                    })
                    .collect();
                if !links.is_empty() {
                    ensure!(
                        sink.is_some()
                            && links
                                .iter()
                                .all(|link| link["info"]["input-node-id"].as_u64() == sink),
                        "{executable} was linked outside its private output"
                    );
                    if !capture.drain()?.is_empty() {
                        ensure!(wrong.drain()?.is_empty(), "unrelated launch captured audio");
                        return Ok(());
                    }
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            anyhow::bail!(
                "{executable} did not route and publish capturable launch-tagged audio: {identity_seen:?}"
            )
        })();
        let _ = child.kill();
        let _ = child.wait();
        result?;
    }
    Ok(())
}

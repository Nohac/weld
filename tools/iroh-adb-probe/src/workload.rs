//! Paced synthetic frames and sequence-checked echo RTT, not codec/display latency.
use crate::{
    Flow,
    tunnel::{Counters, Snapshot},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{MissedTickBehavior, interval, sleep_until, timeout},
};

const RATES: [u32; 3] = [0, 24, 96];
const MAX_FRAME: usize = 256 * 1024;

#[derive(Default, Serialize)]
struct Phase {
    target_mbps: u32,
    duration_seconds: f64,
    received_bytes: u64,
    received_frames: u64,
    received_mbps: f64,
    echo_samples: usize,
    echo_p50_ms: f64,
    echo_p95_ms: f64,
    echo_p99_ms: f64,
    echo_max_ms: f64,
    cpu_ticks: Option<u64>,
    sent_packets: u64,
    received_packets: u64,
    adapter_dropped_packets: u64,
}

// Linux and Android expose process utime/stime in clock ticks. No FFI required.
fn cpu_ticks() -> Option<u64> {
    let value = std::fs::read_to_string("/proc/self/stat").ok()?;
    let (_, fields) = value.rsplit_once(')')?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    fields
        .get(11)?
        .parse::<u64>()
        .ok()?
        .checked_add(fields.get(12)?.parse().ok()?)
}

async fn echo(mut flow: Flow) -> Result<()> {
    let mut sequence = [0; 8];
    loop {
        // Boundary EOF is graceful; partial sequence is corruption.
        if flow.read.read(&mut sequence[..1]).await? == 0 {
            break;
        }
        flow.read.read_exact(&mut sequence[1..]).await?;
        flow.write.write_all(&sequence).await?;
    }
    flow.write.shutdown().await?;
    Ok(())
}

pub async fn serve(
    control: Flow,
    mut media: Flow,
    seconds: u64,
    counters: Option<Arc<Counters>>,
) -> Result<()> {
    let send = async {
        let mut ticker = interval(Duration::from_nanos(1_000_000_000 / 90));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        for (phase, rate) in RATES.iter().enumerate() {
            media.write.write_u8(u8::try_from(phase)?).await?;
            media.write.write_u32(0).await?;
            let start = Instant::now();
            let cpu = cpu_ticks();
            let before = counters
                .as_ref()
                .map(|value| value.snapshot())
                .unwrap_or_default();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
            let frame_size = usize::try_from(u64::from(*rate) * 1_000_000 / 8 / 90)?;
            let payload = vec![0xa5; frame_size];
            let mut frames = 0_u64;
            loop {
                tokio::select! {
                    biased;
                    _ = sleep_until(deadline) => break,
                    _ = ticker.tick() => {}
                }
                if *rate == 0 {
                    continue;
                }
                media.write.write_u8(u8::try_from(phase)?).await?;
                media.write.write_u32(u32::try_from(payload.len())?).await?;
                media.write.write_all(&payload).await?;
                frames += 1;
            }
            let after = counters
                .as_ref()
                .map(|value| value.snapshot())
                .unwrap_or_default();
            println!(
                "{}",
                serde_json::json!({"event":"source_phase", "target_mbps":rate,
                "seconds":start.elapsed().as_secs_f64(), "frames_written":frames,
                "payload_bytes_written":frames * u64::try_from(frame_size)?,
                "cpu_ticks":cpu_ticks().zip(cpu).map(|(a,b)|a.saturating_sub(b)),
                "sent_packets":after.sent_packets.saturating_sub(before.sent_packets),
                "adapter_dropped_packets":after.adapter_dropped_packets.saturating_sub(before.adapter_dropped_packets)})
            );
        }
        media.write.write_u8(3).await?;
        media.write.write_u32(0).await?;
        media.write.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    };
    tokio::try_join!(send, echo(control))?;
    Ok(())
}

fn percentile(values: &[f64], percentile: usize) -> f64 {
    values
        .get(values.len().saturating_sub(1) * percentile / 100)
        .copied()
        .unwrap_or_default()
}

pub async fn receive(
    mut control: Flow,
    mut media: Flow,
    counters: Option<Arc<Counters>>,
) -> Result<()> {
    let phase = AtomicUsize::new(0);
    let done = AtomicBool::new(false);
    let read = async {
        let mut phases: Vec<Phase> = RATES
            .iter()
            .map(|rate| Phase {
                target_mbps: *rate,
                ..Phase::default()
            })
            .collect();
        let mut current = None;
        let mut start = Instant::now();
        let mut cpu = cpu_ticks();
        let mut before = Snapshot::default();
        let mut buffer = vec![0; MAX_FRAME];
        loop {
            let next = usize::from(media.read.read_u8().await?);
            let length = usize::try_from(media.read.read_u32().await?)?;
            ensure!(next <= 3 && length <= MAX_FRAME, "invalid media record");
            if length == 0 {
                ensure!(
                    next == current.map_or(0, |old: usize| old + 1),
                    "invalid phase order"
                );
                if let Some(old) = current {
                    let result: &mut Phase = &mut phases[old];
                    result.duration_seconds = start.elapsed().as_secs_f64();
                    result.received_mbps =
                        result.received_bytes as f64 * 8.0 / result.duration_seconds / 1e6;
                    result.cpu_ticks = cpu_ticks().zip(cpu).map(|(a, b)| a.saturating_sub(b));
                    let after = counters
                        .as_ref()
                        .map(|value| value.snapshot())
                        .unwrap_or_default();
                    result.sent_packets = after.sent_packets.saturating_sub(before.sent_packets);
                    result.received_packets = after
                        .received_packets
                        .saturating_sub(before.received_packets);
                    result.adapter_dropped_packets = after
                        .adapter_dropped_packets
                        .saturating_sub(before.adapter_dropped_packets);
                }
                if next == 3 {
                    done.store(true, Ordering::Release);
                    break;
                }
                current = Some(next);
                phase.store(next, Ordering::Release);
                start = Instant::now();
                cpu = cpu_ticks();
                before = counters
                    .as_ref()
                    .map(|value| value.snapshot())
                    .unwrap_or_default();
            } else {
                ensure!(current == Some(next), "media outside phase");
                media.read.read_exact(&mut buffer[..length]).await?;
                ensure!(
                    buffer[..length].iter().all(|byte| *byte == 0xa5),
                    "corrupt media payload"
                );
                phases[next].received_bytes += u64::try_from(length)?;
                phases[next].received_frames += 1;
            }
        }
        Ok::<_, anyhow::Error>(phases)
    };
    let ping = async {
        let mut samples: [Vec<f64>; 3] = std::array::from_fn(|_| Vec::with_capacity(1000));
        let mut ticker = interval(Duration::from_nanos(1_000_000_000 / 90));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        for sequence in 0_u64..4000 {
            ticker.tick().await;
            if done.load(Ordering::Acquire) {
                control.write.shutdown().await?;
                return Ok(samples);
            }
            let selected = phase.load(Ordering::Acquire);
            let started = Instant::now();
            let response = timeout(Duration::from_secs(2), async {
                control.write.write_u64(sequence).await?;
                control.read.read_u64().await
            })
            .await
            .context("echo timed out")??;
            ensure!(response == sequence, "echo sequence mismatch");
            if phase.load(Ordering::Acquire) == selected {
                samples[selected].push(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        Err::<_, anyhow::Error>(io::Error::other("echo sample bound exceeded").into())
    };
    let (mut phases, samples) = tokio::try_join!(read, ping)?;
    for (result, mut samples) in phases.iter_mut().zip(samples) {
        samples.sort_by(f64::total_cmp);
        ensure!(!samples.is_empty(), "no echo samples for phase");
        result.echo_samples = samples.len();
        result.echo_p50_ms = percentile(&samples, 50);
        result.echo_p95_ms = percentile(&samples, 95);
        result.echo_p99_ms = percentile(&samples, 99);
        result.echo_max_ms = percentile(&samples, 100);
        println!(
            "{}",
            serde_json::json!({"event":"receiver_phase", "measurements":result})
        );
    }
    Ok(())
}

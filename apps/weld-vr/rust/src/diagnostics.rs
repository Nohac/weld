//! Application-owned tracing bridge. Workers never call Godot or block on log I/O.
//! Only bounded formatted summaries reach the main-thread Godot/logcat sink.
use godot::prelude::{godot_print, godot_warn};
use std::{
    io::{self, Write},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
};
use tracing_subscriber::EnvFilter;

const MAX_LINE: usize = 3500;
struct Sink {
    sender: SyncSender<String>,
    receiver: Mutex<Receiver<String>>,
    lost: AtomicU64,
}
static SINK: OnceLock<Sink> = OnceLock::new();
static RECORD: AtomicU64 = AtomicU64::new(1);

pub fn init() {
    if SINK.get().is_some() {
        return;
    }
    let (sender, receiver) = mpsc::sync_channel(256);
    if SINK
        .set(Sink {
            sender,
            receiver: Mutex::new(receiver),
            lost: AtomicU64::new(0),
        })
        .is_err()
    {
        return;
    }
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,weld_media_diag=debug,weld_vr_diag=debug"));
    if let Err(error) = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(LogLine::default)
        .try_init()
    {
        godot_warn!("Weld diagnostics could not install its subscriber: {error}");
    }
}

#[derive(Default)]
struct LogLine(Vec<u8>);
impl Write for LogLine {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = MAX_LINE.saturating_sub(self.0.len());
        self.0
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for LogLine {
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }
        if let Some(sink) = SINK.get() {
            let mut line = String::from_utf8_lossy(&self.0).trim_end().to_owned();
            if self.0.len() == MAX_LINE {
                line.push_str(" [truncated]");
            }
            if sink.sender.try_send(line).is_err() {
                sink.lost.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Called only by the scene's main thread, not by codec/transport workers.
pub fn drain() {
    let Some(sink) = SINK.get() else {
        return;
    };
    let lost = sink.lost.swap(0, Ordering::Relaxed);
    if lost != 0 {
        godot_print!("WELD_DIAGNOSTICS_GAP dropped_lines={lost}");
    }
    let Ok(receiver) = sink.receiver.lock() else {
        return;
    };
    for line in receiver.try_iter().take(64) {
        // Godot's Android logger truncates long messages before logcat sees
        // them. Preserve a complete record in bounded, reassemblable chunks.
        let parts = chunks(&line);
        let id = RECORD.fetch_add(1, Ordering::Relaxed);
        for (index, part) in parts.iter().enumerate() {
            godot_print!(
                "WELD_TRACE id={id} part={} total={} {part}",
                index + 1,
                parts.len()
            );
        }
    }
}

fn chunks(mut text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    while !text.is_empty() {
        let mut end = text.len().min(700);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        parts.push(&text[..end]);
        text = &text[end..];
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formatting_cannot_create_an_unbounded_log_record() {
        let mut line = LogLine::default();
        assert_eq!(line.write(&vec![b'x'; MAX_LINE * 3]).unwrap(), MAX_LINE * 3);
        assert_eq!(line.0.len(), MAX_LINE);
    }
    #[test]
    fn android_chunks_preserve_utf8_and_every_byte() {
        let text = "Frame latency — μs ".repeat(200);
        let parts = chunks(&text);
        assert!(parts.iter().all(|part| part.len() <= 700));
        assert_eq!(parts.concat(), text);
    }
}

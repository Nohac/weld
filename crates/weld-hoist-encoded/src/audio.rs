//! Audio lifecycle follows authorized surface mappings and the source media clock.
#[cfg(target_os = "linux")]
mod native {
    use std::{
        collections::{HashMap, HashSet},
        sync::Arc,
        time::Instant,
    };
    use weld_audio::{Capture, Playback};
    use weld_client::{ClientSurfaceId, ClientSurfaceMetadata};
    use weld_hoist_protocol::{EncodedBuffer, HoistSessionId, SourceEnvelope, SourceMessage};
    use weld_media::audio::AudioPacket;

    #[derive(Default)]
    pub(crate) struct Source {
        wake: Option<Arc<dyn Fn() + Send + Sync>>,
        origin: Option<Instant>,
        mappings: HashMap<ClientSurfaceId, (HoistSessionId, Key)>,
        captures: HashMap<Key, Capture>,
        failed: HashSet<Key>,
    }
    #[derive(Clone, Copy, Hash, PartialEq, Eq)]
    enum Key {
        Launch(u64),
        Process(u32),
    }
    impl Source {
        pub fn new(origin: Instant, wake: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
            Self {
                origin: Some(origin),
                wake,
                ..Self::default()
            }
        }
        pub fn observe(
            &mut self,
            session: HoistSessionId,
            surface: ClientSurfaceId,
            metadata: &ClientSurfaceMetadata,
        ) {
            let (Some(wake), Some(origin)) = (&self.wake, self.origin) else {
                return;
            };
            let Some(key) = metadata
                .local_launch_id()
                .map(Key::Launch)
                .or_else(|| metadata.local_process_id().map(Key::Process))
            else {
                return;
            };
            let Some(target) = weld_audio::application_target(
                metadata.local_process_id(),
                metadata.local_launch_id(),
            ) else {
                return;
            };
            self.mappings.insert(surface, (session, key));
            if self.failed.contains(&key)
                || self.captures.contains_key(&key)
                || self.captures.len() >= 16
            {
                return;
            }
            match Capture::new(target, origin, wake.clone()) {
                Ok(capture) => {
                    self.captures.insert(key, capture);
                }
                Err(error) => {
                    self.failed.insert(key);
                    tracing::warn!(%error, "application audio unavailable");
                }
            }
        }
        pub fn remove(&mut self, surface: ClientSurfaceId) {
            self.mappings.remove(&surface);
            self.failed
                .retain(|key| self.mappings.values().any(|(_, mapped)| mapped == key));
            self.captures
                .retain(|key, _| self.mappings.values().any(|(_, mapped)| mapped == key));
        }
        pub fn drain(&mut self) -> Vec<SourceEnvelope<EncodedBuffer>> {
            let mut output = Vec::new();
            self.captures.retain(|key, capture| {
                let Some((surface, (session, _))) = self
                    .mappings
                    .iter()
                    .filter(|(_, (_, mapped))| mapped == key)
                    .min_by_key(|(surface, _)| **surface)
                else {
                    return false;
                };
                match capture.drain() {
                    Ok(packets) => {
                        output.extend(packets.into_iter().map(|packet| SourceEnvelope {
                            session: *session,
                            message: SourceMessage::Audio {
                                surface: *surface,
                                packet,
                            },
                        }))
                    }
                    Err(error) => {
                        self.failed.insert(*key);
                        tracing::warn!(%error, "application audio capture stopped");
                        return false;
                    }
                }
                true
            });
            output
        }
    }
    #[derive(Default)]
    pub(crate) struct Destination {
        mapped: HashSet<(HoistSessionId, ClientSurfaceId)>,
        playback: HashMap<(HoistSessionId, ClientSurfaceId, u64), Playback>,
        failed: bool,
    }
    impl Destination {
        pub fn map(&mut self, session: HoistSessionId, surface: ClientSurfaceId) {
            self.mapped.insert((session, surface));
        }
        pub fn remove(&mut self, surface: ClientSurfaceId) {
            self.mapped.retain(|(_, mapped)| *mapped != surface);
            self.playback.retain(|(_, mapped, _), _| *mapped != surface);
        }
        pub fn receive(
            &mut self,
            session: HoistSessionId,
            surface: ClientSurfaceId,
            packet: AudioPacket,
        ) -> anyhow::Result<()> {
            anyhow::ensure!(packet.is_valid(), "invalid remote audio packet");
            // Datagrams may arrive before mapping or after withdrawal. Neither may
            // create playback; only the reliable mapped-surface record admits it.
            if self.failed || !self.mapped.contains(&(session, surface)) {
                return Ok(());
            }
            let key = (session, surface, packet.stream);
            if !self.playback.contains_key(&key) && self.playback.len() < 64 {
                match Playback::new(None) {
                    Ok(playback) => {
                        self.playback.insert(key, playback);
                    }
                    Err(error) => {
                        self.failed = true;
                        return Err(error);
                    }
                }
            }
            if let Some(playback) = self.playback.get(&key)
                && let Err(error) = playback.push(packet)
            {
                self.failed = true;
                self.playback.clear();
                return Err(error);
            }
            Ok(())
        }
    }
}
#[cfg(target_os = "linux")]
pub(crate) use native::{Destination, Source};

#[cfg(not(target_os = "linux"))]
mod unsupported {
    use std::{sync::Arc, time::Instant};
    use weld_client::{ClientSurfaceId, ClientSurfaceMetadata};
    use weld_hoist_protocol::{EncodedBuffer, HoistSessionId, SourceEnvelope};
    use weld_media::audio::AudioPacket;
    #[derive(Default)]
    pub(crate) struct Source;
    impl Source {
        pub fn new(_: Instant, _: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
            Self
        }
        pub fn observe(
            &mut self,
            _: HoistSessionId,
            _: ClientSurfaceId,
            _: &ClientSurfaceMetadata,
        ) {
        }
        pub fn remove(&mut self, _: ClientSurfaceId) {}
        pub fn drain(&mut self) -> Vec<SourceEnvelope<EncodedBuffer>> {
            Vec::new()
        }
    }
    #[derive(Default)]
    pub(crate) struct Destination;
    impl Destination {
        pub fn map(&mut self, _: HoistSessionId, _: ClientSurfaceId) {}
        pub fn remove(&mut self, _: ClientSurfaceId) {}
        pub fn receive(
            &mut self,
            _: HoistSessionId,
            _: ClientSurfaceId,
            _: AudioPacket,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }
}
#[cfg(not(target_os = "linux"))]
pub(crate) use unsupported::{Destination, Source};

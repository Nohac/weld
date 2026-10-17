//! Bounded connection transfer from authenticated listeners into host servicing.

use super::{SessionId, pairing::ConnectionControl};
use anyhow::Result;
use std::sync::{Arc, Mutex};
use weld_access::AccessSession;
use weld_media::VideoCodec;

/// Bounded handoff from authenticated listeners to the host's encoded relay owner.
/// The lock covers queue transfer; codec creation and relay servicing run after it is released.
#[derive(Clone)]
pub struct SourceAdmissions<T: ConnectionControl + Clone>(Arc<Mutex<AdmissionQueue<T>>>);
type AdmittedSource<T> = (SessionId, T, AccessSession, VideoCodec);
struct AdmissionQueue<T: ConnectionControl + Clone>(Vec<AdmittedSource<T>>);
impl<T: ConnectionControl + Clone> Default for SourceAdmissions<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(AdmissionQueue(Vec::new()))))
    }
}
impl<T: ConnectionControl + Clone> Drop for AdmissionQueue<T> {
    fn drop(&mut self) {
        for (_, peer, access, _) in self.0.drain(..) {
            access.end();
            peer.close();
        }
    }
}
impl<T: ConnectionControl + Clone> SourceAdmissions<T> {
    pub fn admit(
        &self,
        session: SessionId,
        peer: T,
        access: AccessSession,
        codec: VideoCodec,
    ) -> Result<()> {
        let result = (|| {
            let mut queue = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("peer handoff unavailable"))?;
            let retired: Vec<_> = queue
                .0
                .extract_if(.., |(_, peer, access, _)| {
                    !access.is_authorized() || !peer.is_connected()
                })
                .collect();
            let result = if queue.0.len() >= 8 {
                Err(anyhow::anyhow!("peer handoff full"))
            } else if !access.is_authorized() || !peer.is_connected() {
                Err(anyhow::anyhow!("source session ended before handoff"))
            } else {
                queue.0.push((session, peer.clone(), access.clone(), codec));
                Ok(())
            };
            drop(queue);
            for (_, peer, access, _) in retired {
                access.end();
                peer.close();
            }
            result
        })();
        if result.is_err() {
            access.end();
            peer.close();
        }
        result
    }
    pub fn take(&self) -> Vec<AdmittedSource<T>> {
        self.drain()
            .into_iter()
            .filter(|(_, peer, access, _)| {
                if access.is_authorized() && peer.is_connected() {
                    true
                } else {
                    access.end();
                    peer.close();
                    false
                }
            })
            .collect()
    }
    fn drain(&self) -> Vec<AdmittedSource<T>> {
        self.0
            .lock()
            .map(|mut queue| std::mem::take(&mut queue.0))
            .unwrap_or_default()
    }
    pub fn remove(&self, session: SessionId) {
        let retired = self
            .0
            .lock()
            .map(|mut queue| {
                queue
                    .0
                    .extract_if(.., |(id, _, _, _)| *id == session)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (_, peer, access, _) in retired {
            access.end();
            peer.close();
        }
    }
    pub fn close(&self) {
        for (_, peer, access, _) in self.drain() {
            access.end();
            peer.close();
        }
    }
}

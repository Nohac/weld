use std::{
    collections::VecDeque,
    fmt,
    io::{IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use calloop::{Interest, Mode, generic::Generic};
use rustix::{
    buffer::spare_capacity,
    event::epoll,
    io::{Errno, ioctl_fionbio},
    net::{
        AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
        SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketFlags,
        SocketType, accept_with, bind, connect, listen, recvmsg, sendmsg, socket_with, socketpair,
    },
};
use serde::{Serialize, de::DeserializeOwned};

const MAX_PACKET_BYTES: usize = 192 * 1024;
const MAX_PACKET_FDS: usize = 16;
const SOCKET_EVENT: epoll::EventData = epoll::EventData::new_u64(1);

#[derive(Debug)]
pub enum TransportError {
    Io(std::io::Error),
    Codec(postcard::Error),
    PacketTooLarge {
        bytes: usize,
    },
    TooManyFileDescriptors {
        count: usize,
    },
    TruncatedPacket,
    PartialPacket {
        expected: usize,
        sent: usize,
    },
    PeerUserMismatch {
        expected: u32,
        actual: u32,
    },
    PeerRoleMismatch {
        expected: LocalPeerRole,
        actual: LocalPeerRole,
    },
    Protocol(String),
    Disconnected,
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "local transport I/O failed: {error}"),
            Self::Codec(error) => write!(formatter, "local transport message is invalid: {error}"),
            Self::PacketTooLarge { bytes } => {
                write!(
                    formatter,
                    "local transport packet is too large: {bytes} bytes"
                )
            }
            Self::TooManyFileDescriptors { count } => write!(
                formatter,
                "local transport packet has too many file descriptors: {count}"
            ),
            Self::TruncatedPacket => formatter.write_str("local transport packet was truncated"),
            Self::PartialPacket { expected, sent } => write!(
                formatter,
                "local transport sent a partial packet: {sent} of {expected} bytes"
            ),
            Self::PeerUserMismatch { expected, actual } => write!(
                formatter,
                "local transport peer uses uid {actual}, expected {expected}"
            ),
            Self::PeerRoleMismatch { expected, actual } => write!(
                formatter,
                "local transport peer has role {actual:?}, expected {expected:?}"
            ),
            Self::Protocol(message) => {
                write!(formatter, "local transport protocol failed: {message}")
            }
            Self::Disconnected => formatter.write_str("local transport peer disconnected"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Codec(error) => Some(error),
            _ => None,
        }
    }
}

impl From<Errno> for TransportError {
    fn from(error: Errno) -> Self {
        Self::Io(std::io::Error::from_raw_os_error(error.raw_os_error()))
    }
}

impl From<postcard::Error> for TransportError {
    fn from(error: postcard::Error) -> Self {
        Self::Codec(error)
    }
}

struct LocalPacket {
    bytes: Vec<u8>,
    file_descriptors: Vec<OwnedFd>,
}

impl LocalPacket {
    fn encode(
        message: &impl Serialize,
        file_descriptors: Vec<OwnedFd>,
    ) -> Result<Self, TransportError> {
        let bytes = postcard::to_allocvec(message)?;
        validate_packet(&bytes, &file_descriptors)?;
        Ok(Self {
            bytes,
            file_descriptors,
        })
    }
}

pub struct ReceivedLocalPacket<T> {
    pub message: T,
    pub file_descriptors: Vec<OwnedFd>,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
pub enum LocalPeerRole {
    Source,
    Destination,
}

impl LocalPeerRole {
    const fn peer(self) -> Self {
        match self {
            Self::Source => Self::Destination,
            Self::Destination => Self::Source,
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct AuthenticatedPacket<T> {
    role: LocalPeerRole,
    message: T,
}

pub struct LocalPacketListener {
    socket: OwnedFd,
    path: PathBuf,
    local_role: LocalPeerRole,
}

impl LocalPacketListener {
    pub fn bind(path: impl AsRef<Path>, local_role: LocalPeerRole) -> Result<Self, TransportError> {
        let path = path.as_ref();
        let address = SocketAddrUnix::new(path)?;
        let socket = socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )?;
        bind(&socket, &address)?;
        listen(&socket, 8)?;
        Ok(Self {
            socket,
            path: path.to_owned(),
            local_role,
        })
    }

    pub fn accept(&self) -> Result<Option<LocalPacketConnection>, TransportError> {
        match accept_with(&self.socket, SocketFlags::CLOEXEC | SocketFlags::NONBLOCK) {
            Ok(socket) => LocalPacketConnection::from_socket(socket, self.local_role).map(Some),
            Err(Errno::AGAIN) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Waits for one startup peer without timer polling.
    pub fn accept_blocking(&self) -> Result<LocalPacketConnection, TransportError> {
        loop {
            if let Some(connection) = self.accept()? {
                return Ok(connection);
            }
            let mut descriptor = [rustix::event::PollFd::new(
                &self.socket,
                rustix::event::PollFlags::IN,
            )];
            rustix::event::poll(&mut descriptor, None)?;
        }
    }
}

impl AsFd for LocalPacketListener {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

impl Drop for LocalPacketListener {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), %error, "failed to remove local hoist socket");
        }
    }
}

#[derive(Clone)]
pub struct LocalPacketConnection(Arc<LocalPacketConnectionInner>);

struct LocalPacketConnectionInner {
    socket: OwnedFd,
    epoll: OwnedFd,
    local_role: LocalPeerRole,
    state: Mutex<ConnectionState>,
}

#[derive(Default)]
struct ConnectionState {
    outgoing: VecDeque<LocalPacket>,
    received: VecDeque<LocalPacket>,
    disconnected: bool,
    registered: bool,
    failure: Option<TransportError>,
    receive_scratch: Vec<u8>,
}

impl LocalPacketConnection {
    fn state(&self) -> MutexGuard<'_, ConnectionState> {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn connect(
        path: impl AsRef<Path>,
        local_role: LocalPeerRole,
    ) -> Result<Self, TransportError> {
        let address = SocketAddrUnix::new(path.as_ref())?;
        let socket = socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )?;
        connect(&socket, &address)?;
        Self::from_socket(socket, local_role)
    }

    pub fn pair() -> Result<(Self, Self), TransportError> {
        let flags = SocketFlags::CLOEXEC | SocketFlags::NONBLOCK;
        let (left, right) = socketpair(AddressFamily::UNIX, SocketType::SEQPACKET, flags, None)?;
        Ok((
            Self::from_socket(left, LocalPeerRole::Source)?,
            Self::from_socket(right, LocalPeerRole::Destination)?,
        ))
    }

    fn from_socket(socket: OwnedFd, local_role: LocalPeerRole) -> Result<Self, TransportError> {
        let expected_uid = rustix::process::geteuid().as_raw();
        let actual_uid = rustix::net::sockopt::socket_peercred(&socket)?.uid.as_raw();
        if actual_uid != expected_uid {
            return Err(TransportError::PeerUserMismatch {
                expected: expected_uid,
                actual: actual_uid,
            });
        }
        ioctl_fionbio(&socket, true)?;
        let epoll = epoll::create(epoll::CreateFlags::CLOEXEC)?;
        epoll::add(&epoll, &socket, SOCKET_EVENT, receive_event_flags())?;
        Ok(Self(Arc::new(LocalPacketConnectionInner {
            socket,
            epoll,
            local_role,
            state: Mutex::new(ConnectionState {
                receive_scratch: vec![0_u8; MAX_PACKET_BYTES],
                registered: true,
                ..ConnectionState::default()
            }),
        })))
    }

    /// Wraps the stable aggregate epoll descriptor for insertion into calloop.
    pub fn into_event_source(self) -> Generic<Self, TransportError> {
        Generic::new_with_error(self, Interest::READ, Mode::Level)
    }

    pub fn queue(
        &self,
        message: &impl Serialize,
        file_descriptors: Vec<OwnedFd>,
    ) -> Result<(), TransportError> {
        let mut state = self.state();
        if state.disconnected {
            return Err(TransportError::Disconnected);
        }
        state.outgoing.push_back(LocalPacket::encode(
            &AuthenticatedPacket {
                role: self.0.local_role,
                message,
            },
            file_descriptors,
        )?);
        drop(state);
        self.update_interest()?;
        Ok(())
    }

    /// Drains all readable packets and writable queued datagrams to `EAGAIN`.
    pub fn pump(&self) -> Result<(), TransportError> {
        let mut events = Vec::with_capacity(4);
        epoll::wait(
            &self.0.epoll,
            spare_capacity(&mut events),
            Some(&rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )?;
        for event in events.drain(..) {
            let data = event.data;
            let flags = event.flags;
            if data != SOCKET_EVENT {
                continue;
            }
            if flags.intersects(epoll::EventFlags::ERR | epoll::EventFlags::HUP) {
                self.state().disconnected = true;
            }
            if flags.contains(epoll::EventFlags::IN) {
                self.receive_ready()?;
            }
            if flags.contains(epoll::EventFlags::OUT) {
                self.send_ready()?;
            }
        }
        self.retire_disconnected_socket()?;
        self.update_interest()?;
        let state = self.state();
        if state.disconnected && state.received.is_empty() {
            Err(TransportError::Disconnected)
        } else {
            Ok(())
        }
    }

    pub fn drain<T: DeserializeOwned>(
        &self,
    ) -> Result<Vec<ReceivedLocalPacket<T>>, TransportError> {
        let pump_result = self.pump();
        let mut state = self.state();
        if state.received.is_empty() {
            if let Some(error) = state.failure.take() {
                return Err(error);
            }
            pump_result?;
        }
        state
            .received
            .drain(..)
            .map(|packet| {
                let LocalPacket {
                    bytes,
                    file_descriptors,
                } = packet;
                let packet = postcard::from_bytes::<AuthenticatedPacket<T>>(&bytes)?;
                let expected = self.0.local_role.peer();
                if packet.role != expected {
                    return Err(TransportError::PeerRoleMismatch {
                        expected,
                        actual: packet.role,
                    });
                }
                Ok(ReceivedLocalPacket {
                    message: packet.message,
                    file_descriptors,
                })
            })
            .collect()
    }

    pub fn runtime_wake_source(&self) -> weld_core::host::ClientRuntimeWakeSource {
        let descriptor = self.clone();
        let connection = self.clone();
        weld_core::host::ClientRuntimeWakeSource::new(descriptor, move || {
            if let Err(error) = connection.pump()
                && !matches!(error, TransportError::Disconnected)
            {
                connection.record_failure(error);
            }
            Ok(())
        })
    }

    pub fn is_disconnected(&self) -> bool {
        self.state().disconnected
    }

    fn send_ready(&self) -> Result<(), TransportError> {
        let mut state = self.state();
        while let Some(packet) = state.outgoing.front() {
            let borrowed = packet
                .file_descriptors
                .iter()
                .map(AsFd::as_fd)
                .collect::<Vec<_>>();
            let mut control_space =
                [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_PACKET_FDS))];
            let mut control = SendAncillaryBuffer::new(&mut control_space);
            if !borrowed.is_empty() && !control.push(SendAncillaryMessage::ScmRights(&borrowed)) {
                return Err(TransportError::TooManyFileDescriptors {
                    count: borrowed.len(),
                });
            }
            match sendmsg(
                &self.0.socket,
                &[IoSlice::new(&packet.bytes)],
                &mut control,
                SendFlags::NOSIGNAL,
            ) {
                Ok(sent) if sent == packet.bytes.len() => {
                    state.outgoing.pop_front();
                }
                Ok(sent) => {
                    return Err(TransportError::PartialPacket {
                        expected: packet.bytes.len(),
                        sent,
                    });
                }
                Err(Errno::AGAIN) => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn receive_ready(&self) -> Result<(), TransportError> {
        let mut state = self.state();
        loop {
            let mut bytes = std::mem::take(&mut state.receive_scratch);
            let mut control_space =
                [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_PACKET_FDS))];
            let mut control = RecvAncillaryBuffer::new(&mut control_space);
            let received = {
                let mut iov = [IoSliceMut::new(&mut bytes)];
                recvmsg(
                    &self.0.socket,
                    &mut iov,
                    &mut control,
                    RecvFlags::CMSG_CLOEXEC,
                )
            };
            let received = match received {
                Ok(received) => received,
                Err(Errno::AGAIN) => {
                    state.receive_scratch = bytes;
                    break;
                }
                Err(error) => {
                    state.receive_scratch = bytes;
                    return Err(error.into());
                }
            };
            if received.bytes == 0 {
                state.receive_scratch = bytes;
                state.disconnected = true;
                break;
            }
            if received
                .flags
                .intersects(ReturnFlags::TRUNC | ReturnFlags::CTRUNC)
            {
                state.receive_scratch = bytes;
                return Err(TransportError::TruncatedPacket);
            }
            let packet_bytes = bytes[..received.bytes].to_vec();
            state.receive_scratch = bytes;
            let mut file_descriptors = Vec::new();
            for message in control.drain() {
                if let RecvAncillaryMessage::ScmRights(rights) = message {
                    file_descriptors.extend(rights);
                }
            }
            validate_packet(&packet_bytes, &file_descriptors)?;
            state.received.push_back(LocalPacket {
                bytes: packet_bytes,
                file_descriptors,
            });
        }
        Ok(())
    }

    fn update_interest(&self) -> Result<(), TransportError> {
        if !self.state().registered {
            return Ok(());
        }
        let mut flags = receive_event_flags();
        if !self.state().outgoing.is_empty() {
            flags |= epoll::EventFlags::OUT;
        }
        epoll::modify(&self.0.epoll, &self.0.socket, SOCKET_EVENT, flags)?;
        Ok(())
    }

    fn retire_disconnected_socket(&self) -> Result<(), TransportError> {
        let mut state = self.state();
        if !state.disconnected || !state.registered {
            return Ok(());
        }
        epoll::delete(&self.0.epoll, &self.0.socket)?;
        state.registered = false;
        Ok(())
    }

    pub(crate) fn record_failure(&self, error: TransportError) {
        let mut state = self.state();
        state.failure.get_or_insert(error);
        state.disconnected = true;
        let registered = state.registered;
        state.registered = false;
        drop(state);
        if registered {
            let _ = epoll::delete(&self.0.epoll, &self.0.socket);
        }
        let _ = rustix::net::shutdown(&self.0.socket, rustix::net::Shutdown::Both);
    }
}

impl AsFd for LocalPacketConnection {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.epoll.as_fd()
    }
}

fn receive_event_flags() -> epoll::EventFlags {
    epoll::EventFlags::IN | epoll::EventFlags::ERR | epoll::EventFlags::HUP
}

fn validate_packet(bytes: &[u8], file_descriptors: &[OwnedFd]) -> Result<(), TransportError> {
    if bytes.len() > MAX_PACKET_BYTES {
        return Err(TransportError::PacketTooLarge { bytes: bytes.len() });
    }
    if file_descriptors.len() > MAX_PACKET_FDS {
        return Err(TransportError::TooManyFileDescriptors {
            count: file_descriptors.len(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct Message {
        sequence: u64,
        label: String,
    }

    #[test]
    fn seqpacket_preserves_postcard_message_and_descriptor_boundary() {
        let (sender, receiver) = LocalPacketConnection::pair().expect("socket pair");
        let descriptor = std::fs::File::open("/dev/null").expect("test descriptor");
        sender
            .queue(
                &Message {
                    sequence: 7,
                    label: String::from("surface"),
                },
                vec![descriptor.into()],
            )
            .expect("queued packet");
        sender.pump().expect("sent packet");
        let packets = receiver.drain::<Message>().expect("received packet");

        assert_eq!(packets.len(), 1);
        assert_eq!(
            packets[0].message,
            Message {
                sequence: 7,
                label: String::from("surface")
            }
        );
        assert_eq!(packets[0].file_descriptors.len(), 1);
    }

    #[test]
    fn calloop_wakes_for_a_queued_packet() {
        let (sender, receiver) = LocalPacketConnection::pair().expect("socket pair");
        let mut event_loop = calloop::EventLoop::<bool>::try_new().expect("event loop");
        event_loop
            .handle()
            .insert_source(receiver.into_event_source(), |_, connection, received| {
                let packets = connection.drain::<u64>()?;
                *received = packets.iter().any(|packet| packet.message == 42);
                Ok(calloop::PostAction::Continue)
            })
            .expect("transport source");
        sender.queue(&42_u64, Vec::new()).expect("queued packet");
        sender.pump().expect("sent packet");
        let mut received = false;

        event_loop
            .dispatch(Some(std::time::Duration::from_millis(50)), &mut received)
            .expect("event dispatch");

        assert!(received);
    }

    #[test]
    fn final_packet_is_delivered_before_disconnect() {
        let (sender, receiver) = LocalPacketConnection::pair().expect("socket pair");
        sender.queue(&9_u64, Vec::new()).expect("queued packet");
        sender.pump().expect("sent packet");
        drop(sender);

        let packets = receiver.drain::<u64>().expect("final packet");

        assert_eq!(packets[0].message, 9);
        assert!(matches!(
            receiver.drain::<u64>(),
            Err(TransportError::Disconnected)
        ));
    }

    #[test]
    fn packet_role_must_match_the_connection_endpoint() {
        let flags = SocketFlags::CLOEXEC | SocketFlags::NONBLOCK;
        let (left, right) = socketpair(AddressFamily::UNIX, SocketType::SEQPACKET, flags, None)
            .expect("socket pair");
        let receiver = LocalPacketConnection::from_socket(left, LocalPeerRole::Source)
            .expect("source endpoint");
        let sender = LocalPacketConnection::from_socket(right, LocalPeerRole::Source)
            .expect("mislabeled endpoint");
        sender.queue(&3_u64, Vec::new()).expect("queued packet");
        sender.pump().expect("sent packet");

        assert!(matches!(
            receiver.drain::<u64>(),
            Err(TransportError::PeerRoleMismatch {
                expected: LocalPeerRole::Destination,
                actual: LocalPeerRole::Source,
            })
        ));
    }
}

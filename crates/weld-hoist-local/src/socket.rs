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
const MAX_QUEUED_PACKETS: usize = 256;
const MAX_QUEUED_FILE_DESCRIPTORS: usize = 128;
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
    SendQueueFull {
        packets: usize,
        file_descriptors: usize,
        preflush_sent: usize,
        preflush_blocked: bool,
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
            Self::SendQueueFull {
                packets,
                file_descriptors,
                preflush_sent,
                preflush_blocked,
            } => write!(
                formatter,
                "local transport send queue is full: {packets} packets and {file_descriptors} file descriptors; preflush sent {preflush_sent} and blocked={preflush_blocked}"
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
    queued_file_descriptors: usize,
    received: VecDeque<LocalPacket>,
    disconnected: bool,
    registered: bool,
    failure: Option<TransportError>,
    receive_scratch: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct FlushStatus {
    sent_packets: usize,
    blocked: bool,
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

    pub(crate) fn source_handoff_pair() -> Result<(Self, OwnedFd), TransportError> {
        let flags = SocketFlags::CLOEXEC | SocketFlags::NONBLOCK;
        let (source, destination) =
            socketpair(AddressFamily::UNIX, SocketType::SEQPACKET, flags, None)?;
        Ok((
            Self::from_socket(source, LocalPeerRole::Source)?,
            destination,
        ))
    }

    pub(crate) fn from_received_fd(
        socket: OwnedFd,
        local_role: LocalPeerRole,
    ) -> Result<Self, TransportError> {
        Self::from_socket(socket, local_role)
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
        let packet = LocalPacket::encode(
            &AuthenticatedPacket {
                role: self.0.local_role,
                message,
            },
            file_descriptors,
        )?;
        let preflush = match self.send_ready() {
            Ok(status) => status,
            Err(error) => {
                self.record_outgoing_failure(&error);
                return Err(error);
            }
        };
        let mut state = self.state();
        if state.disconnected {
            return Err(TransportError::Disconnected);
        }
        let queued_file_descriptors = state
            .queued_file_descriptors
            .checked_add(packet.file_descriptors.len())
            .ok_or(TransportError::SendQueueFull {
                packets: state.outgoing.len(),
                file_descriptors: usize::MAX,
                preflush_sent: preflush.sent_packets,
                preflush_blocked: preflush.blocked,
            })?;
        if state.outgoing.len() >= MAX_QUEUED_PACKETS
            || queued_file_descriptors > MAX_QUEUED_FILE_DESCRIPTORS
        {
            let packets = state.outgoing.len();
            drop(state);
            self.record_failure(TransportError::SendQueueFull {
                packets,
                file_descriptors: queued_file_descriptors,
                preflush_sent: preflush.sent_packets,
                preflush_blocked: preflush.blocked,
            });
            return Err(TransportError::SendQueueFull {
                packets,
                file_descriptors: queued_file_descriptors,
                preflush_sent: preflush.sent_packets,
                preflush_blocked: preflush.blocked,
            });
        }
        state.queued_file_descriptors = queued_file_descriptors;
        state.outgoing.push_back(packet);
        drop(state);
        if let Err(error) = self.send_ready() {
            self.record_outgoing_failure(&error);
            return Err(error);
        }
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
                let _ = self.send_ready()?;
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

    /// Flushes startup records before the event loop begins driving the socket.
    pub(crate) fn flush_blocking(&self) -> Result<(), TransportError> {
        loop {
            self.pump()?;
            if self.state().outgoing.is_empty() {
                return Ok(());
            }
            wait_readable(self.as_fd())?;
        }
    }

    /// Receives exactly one startup record before the event loop begins.
    pub(crate) fn receive_blocking<T: DeserializeOwned>(
        &self,
    ) -> Result<ReceivedLocalPacket<T>, TransportError> {
        loop {
            let mut packets = self.drain::<T>()?;
            match packets.len() {
                0 => wait_readable(self.as_fd())?,
                1 => return Ok(packets.remove(0)),
                count => {
                    return Err(TransportError::Protocol(format!(
                        "startup received {count} records instead of one"
                    )));
                }
            }
        }
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

    fn send_ready(&self) -> Result<FlushStatus, TransportError> {
        let mut state = self.state();
        let mut status = FlushStatus::default();
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
                    state.queued_file_descriptors = state
                        .queued_file_descriptors
                        .saturating_sub(packet.file_descriptors.len());
                    state.outgoing.pop_front();
                    status.sent_packets = status.sent_packets.saturating_add(1);
                }
                Ok(sent) => {
                    return Err(TransportError::PartialPacket {
                        expected: packet.bytes.len(),
                        sent,
                    });
                }
                Err(Errno::AGAIN) => {
                    status.blocked = true;
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(status)
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
        state.outgoing.clear();
        state.queued_file_descriptors = 0;
        Ok(())
    }

    pub(crate) fn record_failure(&self, error: TransportError) {
        let mut state = self.state();
        state.failure.get_or_insert(error);
        state.disconnected = true;
        state.outgoing.clear();
        state.queued_file_descriptors = 0;
        let registered = state.registered;
        state.registered = false;
        drop(state);
        if registered {
            let _ = epoll::delete(&self.0.epoll, &self.0.socket);
        }
        let _ = rustix::net::shutdown(&self.0.socket, rustix::net::Shutdown::Both);
    }

    fn record_outgoing_failure(&self, error: &TransportError) {
        let recorded = match error {
            TransportError::Io(error) => {
                let error = error.raw_os_error().map_or_else(
                    || std::io::Error::new(error.kind(), error.to_string()),
                    std::io::Error::from_raw_os_error,
                );
                TransportError::Io(error)
            }
            TransportError::TooManyFileDescriptors { count } => {
                TransportError::TooManyFileDescriptors { count: *count }
            }
            TransportError::PartialPacket { expected, sent } => TransportError::PartialPacket {
                expected: *expected,
                sent: *sent,
            },
            _ => TransportError::Protocol(error.to_string()),
        };
        self.record_failure(recorded);
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

fn wait_readable(descriptor: BorrowedFd<'_>) -> Result<(), TransportError> {
    let mut descriptors = [rustix::event::PollFd::new(
        &descriptor,
        rustix::event::PollFlags::IN,
    )];
    rustix::event::poll(&mut descriptors, None)?;
    Ok(())
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
    fn queue_immediately_flushes_writable_packets_in_order() {
        let (sender, receiver) = LocalPacketConnection::pair().expect("socket pair");
        for sequence in 0..3_u64 {
            sender
                .queue(&sequence, Vec::new())
                .expect("queue writable packet");
            assert!(sender.state().outgoing.is_empty());
        }

        let packets = receiver.drain::<u64>().expect("receive ordered packets");
        assert_eq!(
            packets
                .into_iter()
                .map(|packet| packet.message)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn blocked_peer_reaches_the_userspace_packet_bound() {
        let (sender, receiver) = LocalPacketConnection::pair().expect("socket pair");
        constrain_socket_buffers(&sender, &receiver);
        let error = (0..10_000_u64)
            .find_map(|sequence| sender.queue(&sequence, Vec::new()).err())
            .expect("blocked peer should reach the bounded queue");

        assert!(matches!(
            error,
            TransportError::SendQueueFull {
                packets: MAX_QUEUED_PACKETS,
                preflush_sent: 0,
                preflush_blocked: true,
                ..
            }
        ));
        assert!(sender.is_disconnected());
        assert!(sender.state().outgoing.is_empty());
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

    #[test]
    fn descriptor_bearing_send_queue_is_bounded() {
        let (sender, receiver) = LocalPacketConnection::pair().expect("socket pair");
        constrain_socket_buffers(&sender, &receiver);
        let error = (0..10_000_usize)
            .find_map(|sequence| {
                let descriptor = std::fs::File::open("/dev/null").expect("test descriptor");
                sender.queue(&sequence, vec![descriptor.into()]).err()
            })
            .expect("blocked peer should reach the descriptor bound");

        assert!(matches!(
            error,
            TransportError::SendQueueFull {
                file_descriptors,
                preflush_sent: 0,
                preflush_blocked: true,
                ..
            } if file_descriptors == MAX_QUEUED_FILE_DESCRIPTORS + 1
        ));
        assert!(sender.is_disconnected());
    }

    fn constrain_socket_buffers(sender: &LocalPacketConnection, receiver: &LocalPacketConnection) {
        rustix::net::sockopt::set_socket_send_buffer_size(&sender.0.socket, 4_096)
            .expect("small send buffer");
        rustix::net::sockopt::set_socket_recv_buffer_size(&receiver.0.socket, 4_096)
            .expect("small receive buffer");
    }
}

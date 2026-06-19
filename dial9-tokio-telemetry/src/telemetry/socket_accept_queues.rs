//! Socket accept queue snapshots sampled from Linux sock_diag.

use std::time::Duration;

const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_millis(400);

/// Configuration for socket accept queue sampling.
///
/// Built via `SocketAcceptQueuesConfig::builder()...build()` and enabled with
/// [`TracedRuntimeBuilder::with_socket_accept_queues`](crate::telemetry::TracedRuntimeBuilder::with_socket_accept_queues).
///
/// # Performance
///
/// Full scans can be expensive because they walk `/proc/self/fd` to find this
/// process's listeners. The cost grows with the number of open file descriptors
/// in this process, including accepted sockets, open files, pipes, and similar
/// handles.
///
/// To avoid that cost on every sample, this source caches the classification of
/// TCP listeners visible in the current network namespace. While that listener
/// set is stable, samples do not need a full file descriptor scan and should be
/// cheap.
///
/// # Reliability
///
/// Listeners classified as foreign are cached as foreign. If such a listener is
/// later transferred into this process with `SCM_RIGHTS`, it will not be tracked
/// while it keeps the same kernel socket identity.
#[derive(Debug, Clone, bon::Builder)]
pub struct SocketAcceptQueuesConfig {
    /// Minimum time between samples.
    #[builder(default = DEFAULT_SAMPLE_INTERVAL)]
    sample_interval: Duration,
}

impl Default for SocketAcceptQueuesConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl SocketAcceptQueuesConfig {
    /// Minimum time between samples.
    pub fn sample_interval(&self) -> Duration {
        self.sample_interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sample_interval_is_400ms() {
        assert_eq!(
            SocketAcceptQueuesConfig::default().sample_interval(),
            Duration::from_millis(400)
        );
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::SocketAcceptQueuesConfig;
    use crate::rate_limit::rate_limited;
    use crate::telemetry::buffer::record_encodable_event;
    use crate::telemetry::events::clock_monotonic_ns;
    use crate::telemetry::format::TcpAcceptQueueEvent;
    use crate::telemetry::recorder::source::{FlushContext, Source};
    use netlink_packet_core::{
        NLM_F_DUMP, NLM_F_REQUEST, NetlinkBuffer, NetlinkMessage, NetlinkPayload,
    };
    use netlink_packet_sock_diag::inet::{ExtensionFlags, InetRequest, SocketId, StateFlags};
    use netlink_packet_sock_diag::{AF_INET, AF_INET6, IPPROTO_TCP, SockDiagMessage, TCP_LISTEN};
    use netlink_sys::{Socket, SocketAddr, protocols::NETLINK_SOCK_DIAG};
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::io;
    use std::net::IpAddr;
    use std::os::fd::AsRawFd;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    const NETLINK_RECEIVE_TIMEOUT: Duration = Duration::from_millis(100);

    /// Flush-thread source that samples TCP listener accept queue depth.
    ///
    /// See [issue #501](https://github.com/dial9-rs/dial9/issues/501) for details.
    #[derive(Debug)]
    pub(crate) struct SocketAcceptQueuesSource {
        config: SocketAcceptQueuesConfig,
        last_sample: Option<Instant>,
        cache: SocketAcceptQueueCache,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct SocketKey {
        inode: u64,
        cookie: u64,
    }

    #[derive(Debug, Default)]
    struct SocketAcceptQueueCache {
        owned: HashMap<SocketKey, PathBuf>,
        foreign: HashSet<SocketKey>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TcpAcceptQueueSnapshot {
        key: SocketKey,
        ip_version: u8,
        local_addr: IpAddr,
        local_port: u16,
        pending_connections: u32,
        backlog_limit: u32,
    }

    impl SocketAcceptQueuesSource {
        pub(crate) fn new(config: SocketAcceptQueuesConfig) -> Self {
            Self {
                config,
                last_sample: None,
                cache: SocketAcceptQueueCache::default(),
            }
        }
    }

    impl SocketAcceptQueueCache {
        fn prune(&mut self, active_keys: &HashSet<SocketKey>) {
            self.owned.retain(|key, _| active_keys.contains(key));
            self.foreign.retain(|key| active_keys.contains(key));
        }
    }

    impl Source for SocketAcceptQueuesSource {
        fn flush(&mut self, ctx: &FlushContext<'_>) {
            let now = Instant::now();
            if let Some(last_sample) = self.last_sample
                && now.duration_since(last_sample) < self.config.sample_interval
            {
                return;
            }
            self.last_sample = Some(now);

            match collect_socket_accept_queues(&mut self.cache) {
                Ok(snapshots) => {
                    let timestamp_ns = clock_monotonic_ns();
                    for snapshot in snapshots {
                        record_encodable_event(
                            &snapshot.into_event(timestamp_ns),
                            ctx.collector,
                            ctx.drain_epoch,
                        );
                    }
                }
                Err(e) => rate_limited!(Duration::from_secs(60), {
                    tracing::warn!("failed to read socket accept queues via sock_diag: {e}");
                }),
            }
        }

        fn name(&self) -> &'static str {
            "socket_accept_queues"
        }
    }

    impl TcpAcceptQueueSnapshot {
        fn into_event(self, timestamp_ns: u64) -> TcpAcceptQueueEvent {
            TcpAcceptQueueEvent {
                timestamp_ns,
                socket_cookie: self.key.cookie,
                socket_inode: self.key.inode,
                ip_version: self.ip_version,
                local_addr: self.local_addr.to_string(),
                local_port: self.local_port,
                pending_connections: self.pending_connections,
                backlog_limit: self.backlog_limit,
            }
        }
    }

    fn collect_socket_accept_queues(
        cache: &mut SocketAcceptQueueCache,
    ) -> io::Result<Vec<TcpAcceptQueueSnapshot>> {
        let mut socket = Socket::new(NETLINK_SOCK_DIAG)?;
        set_socket_receive_timeout(&socket, NETLINK_RECEIVE_TIMEOUT)?;
        let _local_addr = socket.bind_auto()?;
        let kernel_addr = SocketAddr::new(0, 0);
        socket.connect(&kernel_addr)?;

        let mut snapshots = Vec::new();
        dump_tcp_listeners(&socket, AF_INET, 1, &mut snapshots)?;
        dump_tcp_listeners(&socket, AF_INET6, 2, &mut snapshots)?;
        classify_process_listeners(
            snapshots,
            cache,
            read_socket_inode_for_fd_path,
            scan_process_socket_fds,
        )
    }

    fn dump_tcp_listeners(
        socket: &Socket,
        family: u8,
        sequence_number: u32,
        snapshots: &mut Vec<TcpAcceptQueueSnapshot>,
    ) -> io::Result<()> {
        let socket_id = match family {
            AF_INET => SocketId::new_v4(),
            AF_INET6 => SocketId::new_v6(),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsupported inet socket family {family}"),
                ));
            }
        };

        let request = InetRequest {
            family,
            protocol: IPPROTO_TCP,
            extensions: ExtensionFlags::empty(),
            states: StateFlags::LISTEN,
            socket_id,
        };
        let mut message = NetlinkMessage::from(SockDiagMessage::InetRequest(request));
        message.header.flags = NLM_F_REQUEST | NLM_F_DUMP;
        message.header.sequence_number = sequence_number;
        message.finalize();

        let mut request_bytes = vec![0; message.buffer_len()];
        message.serialize(&mut request_bytes);
        let sent = socket.send(&request_bytes, 0)?;
        if sent != request_bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!(
                    "short netlink sock_diag request write: sent {sent} of {} bytes",
                    request_bytes.len()
                ),
            ));
        }

        loop {
            let (response_bytes, _sender) = socket.recv_from_full()?;
            if response_bytes.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "sock_diag netlink socket returned an empty response",
                ));
            }

            let done = parse_response_datagram(&response_bytes, sequence_number, snapshots)?;
            if done {
                return Ok(());
            }
        }
    }

    fn parse_response_datagram(
        bytes: &[u8],
        sequence_number: u32,
        snapshots: &mut Vec<TcpAcceptQueueSnapshot>,
    ) -> io::Result<bool> {
        let mut offset = 0;
        let mut done = false;

        while offset < bytes.len() {
            let remaining = &bytes[offset..];
            let packet = NetlinkBuffer::new_checked(&remaining).map_err(decode_error)?;
            let packet_len = packet.length() as usize;
            if packet_len == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sock_diag returned zero-length netlink message",
                ));
            }
            let message = NetlinkMessage::<SockDiagMessage>::deserialize(&remaining[..packet_len])
                .map_err(decode_error)?;

            if message.header.sequence_number == sequence_number {
                match message.payload {
                    NetlinkPayload::Done(_) => done = true,
                    NetlinkPayload::Error(error) => {
                        if error.code.is_some() {
                            return Err(error.into());
                        }
                    }
                    NetlinkPayload::InnerMessage(SockDiagMessage::InetResponse(response)) => {
                        if let Some(snapshot) = snapshot_from_response(&response) {
                            snapshots.push(snapshot);
                        }
                    }
                    _ => {}
                }
            }

            offset += nlmsg_align(packet_len);
        }

        Ok(done)
    }

    fn snapshot_from_response(
        response: &netlink_packet_sock_diag::inet::InetResponse,
    ) -> Option<TcpAcceptQueueSnapshot> {
        let header = &response.header;
        let socket_inode = u64::from(header.inode);
        if header.state != TCP_LISTEN {
            return None;
        }

        Some(TcpAcceptQueueSnapshot {
            key: SocketKey {
                inode: socket_inode,
                cookie: socket_cookie_from_diag(header.socket_id.cookie),
            },
            ip_version: match header.family {
                AF_INET => 4,
                AF_INET6 => 6,
                _ => return None,
            },
            local_addr: header.socket_id.source_address,
            local_port: header.socket_id.source_port,
            pending_connections: header.recv_queue,
            backlog_limit: header.send_queue,
        })
    }

    fn classify_process_listeners(
        snapshots: Vec<TcpAcceptQueueSnapshot>,
        cache: &mut SocketAcceptQueueCache,
        mut read_fd_inode: impl FnMut(&Path) -> io::Result<Option<u64>>,
        mut scan_socket_fds: impl FnMut(&HashSet<u64>) -> io::Result<HashMap<u64, PathBuf>>,
    ) -> io::Result<Vec<TcpAcceptQueueSnapshot>> {
        // The expensive operation is walking /proc/self/fd. Do it only when
        // sock_diag reports a listener we have not classified before.
        let active_keys = snapshots
            .iter()
            .map(|snapshot| snapshot.key)
            .collect::<HashSet<_>>();
        cache.prune(&active_keys);

        let mut owned_snapshots = Vec::new();
        let mut unknown_snapshots = Vec::new();
        let mut unknown_inodes = HashSet::new();

        for snapshot in snapshots {
            let key = snapshot.key;

            if let Some(fd_path) = cache.owned.get(&key) {
                if read_fd_inode(fd_path)? == Some(key.inode) {
                    owned_snapshots.push(snapshot);
                    continue;
                }
                cache.owned.remove(&key);
            }

            if cache.foreign.contains(&key) {
                continue;
            }

            unknown_inodes.insert(key.inode);
            unknown_snapshots.push(snapshot);
        }

        if unknown_snapshots.is_empty() {
            return Ok(owned_snapshots);
        }

        let owned_fds_by_inode = scan_socket_fds(&unknown_inodes)?;
        for snapshot in unknown_snapshots {
            if let Some(fd_path) = owned_fds_by_inode.get(&snapshot.key.inode) {
                cache.owned.insert(snapshot.key, fd_path.clone());
                owned_snapshots.push(snapshot);
            } else {
                cache.foreign.insert(snapshot.key);
            }
        }

        Ok(owned_snapshots)
    }

    fn set_socket_receive_timeout(socket: &Socket, timeout: Duration) -> io::Result<()> {
        let timeout = libc::timeval {
            tv_sec: timeout.as_secs().try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("socket receive timeout {timeout:?} exceeds timeval.tv_sec"),
                )
            })?,
            tv_usec: timeout.subsec_micros().into(),
        };
        let optlen: libc::socklen_t = std::mem::size_of_val(&timeout).try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "timeval size exceeds libc::socklen_t",
            )
        })?;

        // SAFETY: `socket.as_raw_fd()` is a live netlink socket owned by `socket`,
        // `timeout` points to a properly initialized `timeval`, and the length
        // matches the pointed-to value for the duration of the syscall.
        let result = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &timeout as *const libc::timeval as *const libc::c_void,
                optlen,
            )
        };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn scan_process_socket_fds(target_inodes: &HashSet<u64>) -> io::Result<HashMap<u64, PathBuf>> {
        let mut fds_by_inode = HashMap::new();
        if target_inodes.is_empty() {
            return Ok(fds_by_inode);
        }

        for entry in fs::read_dir("/proc/self/fd")? {
            let entry = entry?;
            let path = entry.path();
            if let Some(inode) = read_socket_inode_for_fd_path(&path)?
                && target_inodes.contains(&inode)
            {
                fds_by_inode.entry(inode).or_insert(path);
                if fds_by_inode.len() == target_inodes.len() {
                    break;
                }
            }
        }
        Ok(fds_by_inode)
    }

    fn read_socket_inode_for_fd_path(path: &Path) -> io::Result<Option<u64>> {
        match fs::read_link(path) {
            Ok(target) => parse_socket_inode(&target),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn parse_socket_inode(path: &Path) -> io::Result<Option<u64>> {
        let Some(text) = path.to_str() else {
            return Ok(None);
        };
        let Some(inode) = text
            .strip_prefix("socket:[")
            .and_then(|value| value.strip_suffix(']'))
        else {
            return Ok(None);
        };

        inode.parse::<u64>().map(Some).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid /proc/self/fd socket inode {inode:?}: {e}"),
            )
        })
    }

    fn socket_cookie_from_diag(bytes: [u8; 8]) -> u64 {
        let low = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64;
        let high = u32::from_ne_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as u64;
        (high << 32) | low
    }

    fn decode_error(error: impl std::fmt::Display) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error.to_string())
    }

    fn nlmsg_align(len: usize) -> usize {
        (len + 3) & !3
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::cell::Cell;
        use std::collections::HashMap;
        use std::path::PathBuf;

        #[test]
        fn parse_socket_inode_extracts_socket_symlink_inode() {
            assert_eq!(
                parse_socket_inode(Path::new("socket:[12345]")).unwrap(),
                Some(12345)
            );
        }

        #[test]
        fn parse_socket_inode_ignores_non_socket_symlink() {
            assert_eq!(parse_socket_inode(Path::new("/dev/null")).unwrap(), None);
        }

        #[test]
        fn cached_owned_listener_does_not_scan_process_fds_again() {
            let key = SocketKey {
                inode: 123,
                cookie: 456,
            };
            let snapshot = snapshot_with_key(key);
            let fd_path = PathBuf::from("/proc/self/fd/7");
            let mut cache = SocketAcceptQueueCache::default();
            cache.owned.insert(key, fd_path.clone());
            let scans = Cell::new(0);

            let snapshots = classify_process_listeners(
                vec![snapshot.clone()],
                &mut cache,
                |path| {
                    assert_eq!(path, fd_path.as_path());
                    Ok(Some(key.inode))
                },
                |_| {
                    scans.set(scans.get() + 1);
                    Ok(HashMap::new())
                },
            )
            .unwrap();

            assert_eq!(snapshots, vec![snapshot]);
            assert_eq!(scans.get(), 0);
        }

        #[test]
        fn unknown_listener_scans_once_and_caches_owned_listener() {
            let key = SocketKey {
                inode: 123,
                cookie: 456,
            };
            let snapshot = snapshot_with_key(key);
            let fd_path = PathBuf::from("/proc/self/fd/7");
            let mut cache = SocketAcceptQueueCache::default();
            let scans = Cell::new(0);

            let snapshots = classify_process_listeners(
                vec![snapshot.clone()],
                &mut cache,
                |_| panic!("unknown listeners should not validate cached fd paths"),
                |target_inodes| {
                    scans.set(scans.get() + 1);
                    assert!(target_inodes.contains(&key.inode));
                    Ok(HashMap::from([(key.inode, fd_path.clone())]))
                },
            )
            .unwrap();

            assert_eq!(snapshots, vec![snapshot]);
            assert_eq!(cache.owned.get(&key), Some(&fd_path));
            assert_eq!(scans.get(), 1);
        }

        #[test]
        fn cached_foreign_listener_does_not_rescan() {
            let key = SocketKey {
                inode: 123,
                cookie: 456,
            };
            let mut cache = SocketAcceptQueueCache::default();
            cache.foreign.insert(key);
            let scans = Cell::new(0);

            let snapshots = classify_process_listeners(
                vec![snapshot_with_key(key)],
                &mut cache,
                |_| panic!("foreign listeners should not validate fd paths"),
                |_| {
                    scans.set(scans.get() + 1);
                    Ok(HashMap::new())
                },
            )
            .unwrap();

            assert!(snapshots.is_empty());
            assert_eq!(scans.get(), 0);
        }

        #[test]
        fn inactive_cache_entries_are_pruned_before_classification() {
            let old_key = SocketKey {
                inode: 123,
                cookie: 456,
            };
            let new_key = SocketKey {
                inode: 123,
                cookie: 789,
            };
            let fd_path = PathBuf::from("/proc/self/fd/7");
            let mut cache = SocketAcceptQueueCache::default();
            cache.foreign.insert(old_key);

            let snapshots = classify_process_listeners(
                vec![snapshot_with_key(new_key)],
                &mut cache,
                |_| panic!("new listener should be unknown, not cached"),
                |_| Ok(HashMap::from([(new_key.inode, fd_path.clone())])),
            )
            .unwrap();

            assert_eq!(snapshots, vec![snapshot_with_key(new_key)]);
            assert!(!cache.foreign.contains(&old_key));
            assert_eq!(cache.owned.get(&new_key), Some(&fd_path));
        }

        #[test]
        fn socket_cookie_from_diag_combines_kernel_cookie_words() {
            let low = 0x5566_7788_u32.to_ne_bytes();
            let high = 0x1122_3344_u32.to_ne_bytes();

            assert_eq!(
                socket_cookie_from_diag([
                    low[0], low[1], low[2], low[3], high[0], high[1], high[2], high[3],
                ]),
                0x1122_3344_5566_7788
            );
        }

        fn snapshot_with_key(key: SocketKey) -> TcpAcceptQueueSnapshot {
            TcpAcceptQueueSnapshot {
                key,
                ip_version: 4,
                local_addr: "127.0.0.1".parse().unwrap(),
                local_port: 8080,
                pending_connections: 1,
                backlog_limit: 128,
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::SocketAcceptQueuesSource;

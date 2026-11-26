//! Which local process is on the other end of a connection to the page. A task's agent could call the page's API
//! (`curl`, a browser it drives) to answer its own confirmations; the daemon refuses that when it knows the process
//! (`ictl_daemon::procs`). Linux (`/proc`) and macOS (`libproc`); elsewhere `None`, and only the daemon's own checks
//! apply.

use std::net::SocketAddr;

/// The process holding the client end of a loopback connection from `client` to `server` (our end); `None` for a
/// client on another machine, or when it can't be found
pub fn local_client(client: SocketAddr, server: SocketAddr) -> Option<u32> {
    let client = unmapped(client);
    if !client.ip().is_loopback() {
        return None;
    }
    find(client, unmapped(server))
}

fn unmapped(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(v4.into(), v6.port()),
            None => a,
        },
        SocketAddr::V4(_) => a,
    }
}

#[cfg(target_os = "linux")]
fn find(client: SocketAddr, server: SocketAddr) -> Option<u32> {
    let inode = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .find_map(|table| socket_inode(&table, client, server))?;
    let want = format!("socket:[{inode}]");
    let me = std::process::id();
    // the processes this user may look into; another user's fds can't be read, and aren't an agent of ours
    for proc in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = proc.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        if pid == me {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(proc.path().join("fd")) else { continue };
        if fds.flatten().any(|fd| std::fs::read_link(fd.path()).is_ok_and(|l| l.as_os_str() == want.as_str())) {
            return Some(pid);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn find(client: SocketAddr, server: SocketAddr) -> Option<u32> {
    let me = std::process::id();
    // Only scan processes owned by the effective UID: another user's fds can't be inspected anyway, and aren't an
    // agent started by us.
    for pid in darwin::user_pids()? {
        let Ok(uid_pid) = u32::try_from(pid) else { continue };
        if uid_pid == 0 || uid_pid == me {
            continue;
        }
        let Some(fds) = darwin::process_fds(pid) else { continue };
        for fd in fds {
            if fd.proc_fdtype != libc::PROX_FDTYPE_SOCKET as u32 {
                continue;
            }
            let Some(info) = darwin::socket_fdinfo(pid, fd.proc_fd) else { continue };
            if darwin::socket_endpoints(&info) == Some((client, server)) {
                return Some(uid_pid);
            }
        }
    }
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn find(_client: SocketAddr, _server: SocketAddr) -> Option<u32> {
    None
}

/// Minimal Darwin LP64 `libproc` / `<sys/proc_info.h>` bindings for looking up TCP loopback endpoints.
///
/// `libc` exposes `proc_listpids`, `proc_pidinfo`, `proc_pidfdinfo`, `proc_fdinfo`, `PROC_PIDLISTFDS`, and
/// `PROX_FDTYPE_SOCKET` on macOS, but omits `PROC_UID_ONLY`, `PROC_PIDFDSOCKETINFO`, and `struct socket_fdinfo`.
/// The pure ABI structs and endpoint parser are compiled under `cfg(test)` as well so their layout and decoding are
/// verified on every host before running against the real Darwin kernel in macOS CI.
#[cfg(any(target_os = "macos", test))]
mod darwin {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    use super::unmapped;

    #[cfg(target_os = "macos")]
    const PROC_UID_ONLY: u32 = 4;
    #[cfg(target_os = "macos")]
    const PROC_PIDFDSOCKETINFO: libc::c_int = 3;

    pub(super) const SOCKINFO_IN: i32 = 1;
    pub(super) const SOCKINFO_TCP: i32 = 2;
    pub(super) const INI_IPV4: u8 = 0x1;
    pub(super) const INI_IPV6: u8 = 0x2;

    pub(super) const DARWIN_AF_INET: i32 = 2;
    pub(super) const DARWIN_AF_INET6: i32 = 30;
    pub(super) const DARWIN_SOCK_STREAM: i32 = 1;
    pub(super) const DARWIN_IPPROTO_TCP: i32 = 6;
    const SOCK_MAXADDRLEN: usize = 255;

    /// Largest member of `socket_info.soi_proto` is `struct un_sockinfo`:
    /// two `u64` handles (16) + two `SOCK_MAXADDRLEN` (255) address buffers = 526, padded to 8-byte alignment = 528.
    const SOI_PROTO_SIZE: usize = (16 + 2 * SOCK_MAXADDRLEN + 7) & !7;

    #[cfg(target_os = "macos")]
    const _: () = {
        assert!(DARWIN_AF_INET == libc::AF_INET);
        assert!(DARWIN_AF_INET6 == libc::AF_INET6);
        assert!(DARWIN_SOCK_STREAM == libc::SOCK_STREAM);
        assert!(DARWIN_IPPROTO_TCP == libc::IPPROTO_TCP);
        assert!(SOCK_MAXADDRLEN == libc::SOCK_MAXADDRLEN as usize);
    };

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub(super) struct ProcFileInfo {
        pub fi_openflags: u32,
        pub fi_status: u32,
        pub fi_offset: i64,
        pub fi_type: i32,
        pub fi_guardflags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub(super) struct VinfoStat {
        pub vst_dev: u32,
        pub vst_mode: u16,
        pub vst_nlink: u16,
        pub vst_ino: u64,
        pub vst_uid: u32,
        pub vst_gid: u32,
        pub vst_atime: i64,
        pub vst_atimensec: i64,
        pub vst_mtime: i64,
        pub vst_mtimensec: i64,
        pub vst_ctime: i64,
        pub vst_ctimensec: i64,
        pub vst_birthtime: i64,
        pub vst_birthtimensec: i64,
        pub vst_size: i64,
        pub vst_blocks: i64,
        pub vst_blksize: i32,
        pub vst_flags: u32,
        pub vst_gen: u32,
        pub vst_rdev: u32,
        pub vst_qspare: [i64; 2],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub(super) struct SockbufInfo {
        pub sbi_cc: u32,
        pub sbi_hiwat: u32,
        pub sbi_mbcnt: u32,
        pub sbi_mbmax: u32,
        pub sbi_lowat: u32,
        pub sbi_flags: i16,
        pub sbi_timeo: i16,
    }

    /// `union { struct in4in6_addr ina_46; struct in6_addr ina_6; }` (16 bytes, 4-byte aligned).
    /// IPv4 sits in the last 4 bytes (`i46a_addr4`) in network byte order; IPv6 occupies all 16 bytes.
    #[repr(C, align(4))]
    #[derive(Clone, Copy)]
    pub(super) struct InAddr46(pub [u8; 16]);

    impl InAddr46 {
        fn v4(self) -> IpAddr {
            Ipv4Addr::new(self.0[12], self.0[13], self.0[14], self.0[15]).into()
        }

        fn v6(self) -> IpAddr {
            Ipv6Addr::from(self.0).into()
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub(super) struct In4Info {
        pub in4_tos: u8,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub(super) struct In6Info {
        pub in6_hlim: u8,
        pub in6_cksum: i32,
        pub in6_ifindex: u16,
        pub in6_hops: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub(super) struct InSockInfo {
        pub insi_fport: i32,
        pub insi_lport: i32,
        pub insi_gencnt: u64,
        pub insi_flags: u32,
        pub insi_flow: u32,
        pub insi_vflag: u8,
        pub insi_ip_ttl: u8,
        pub rfu_1: u32,
        pub insi_faddr: InAddr46,
        pub insi_laddr: InAddr46,
        pub insi_v4: In4Info,
        pub insi_v6: In6Info,
    }

    /// `socket_info.soi_proto` union (528 bytes, 8-byte aligned). Both `SOCKINFO_IN` (`pri_in`) and `SOCKINFO_TCP`
    /// (`pri_tcp.tcpsi_ini`) begin with `InSockInfo` at offset 0.
    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub(super) struct SocketProto {
        pub in_info: InSockInfo,
        pub _tail: [u8; SOI_PROTO_SIZE - std::mem::size_of::<InSockInfo>()],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub(super) struct SocketInfo {
        pub soi_stat: VinfoStat,
        pub soi_so: u64,
        pub soi_pcb: u64,
        pub soi_type: i32,
        pub soi_protocol: i32,
        pub soi_family: i32,
        pub soi_options: i16,
        pub soi_linger: i16,
        pub soi_state: i16,
        pub soi_qlen: i16,
        pub soi_incqlen: i16,
        pub soi_qlimit: i16,
        pub soi_timeo: i16,
        pub soi_error: u16,
        pub soi_oobmark: u32,
        pub soi_rcv: SockbufInfo,
        pub soi_snd: SockbufInfo,
        pub soi_kind: i32,
        pub rfu_1: u32,
        pub soi_proto: SocketProto,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    pub(super) struct SocketFdInfo {
        pub pfi: ProcFileInfo,
        pub psi: SocketInfo,
    }

    /// Darwin stores `inp_lport` / `inp_fport` (network-byte-order `u16`) zero-extended into a `c_int`.
    fn darwin_port(raw: i32) -> Option<u16> {
        u16::try_from(raw).ok().map(u16::from_be)
    }

    /// `(local, remote)` addresses of a connected TCP socket from Darwin's `socket_fdinfo`, with IPv4-mapped IPv6
    /// addresses normalised to IPv4 via [`unmapped`].
    pub(super) fn socket_endpoints(info: &SocketFdInfo) -> Option<(SocketAddr, SocketAddr)> {
        let psi = &info.psi;
        if psi.soi_kind != SOCKINFO_TCP && psi.soi_kind != SOCKINFO_IN {
            return None;
        }
        if psi.soi_type != DARWIN_SOCK_STREAM || (psi.soi_protocol != 0 && psi.soi_protocol != DARWIN_IPPROTO_TCP) {
            return None;
        }
        if psi.soi_family != DARWIN_AF_INET && psi.soi_family != DARWIN_AF_INET6 {
            return None;
        }
        let ini = &psi.soi_proto.in_info;
        let lport = darwin_port(ini.insi_lport)?;
        let fport = darwin_port(ini.insi_fport)?;
        if lport == 0 || fport == 0 {
            return None;
        }
        let (lip, fip) = if ini.insi_vflag & INI_IPV4 != 0 {
            (ini.insi_laddr.v4(), ini.insi_faddr.v4())
        } else if ini.insi_vflag & INI_IPV6 != 0 {
            (ini.insi_laddr.v6(), ini.insi_faddr.v6())
        } else {
            return None;
        };
        Some((unmapped(SocketAddr::new(lip, lport)), unmapped(SocketAddr::new(fip, fport))))
    }

    #[cfg(target_os = "macos")]
    pub(super) fn user_pids() -> Option<Vec<libc::pid_t>> {
        // SAFETY: `geteuid` takes no pointers and always succeeds.
        let uid = unsafe { libc::geteuid() };
        // SAFETY: NULL buffer queries the byte count needed for the UID's process list.
        let bytes = unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, std::ptr::null_mut(), 0) };
        if bytes <= 0 {
            return None;
        }
        let item = std::mem::size_of::<libc::pid_t>();
        let mut cap = (bytes as usize / item).saturating_add(64);
        let mut pids = Vec::new();
        for _ in 0..3 {
            let buf_bytes = libc::c_int::try_from(cap.checked_mul(item)?).ok()?;
            pids.resize(cap, 0);
            // SAFETY: `pids` is a contiguous writable buffer of `buf_bytes` bytes.
            let got = unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, pids.as_mut_ptr().cast(), buf_bytes) };
            if got <= 0 {
                return None;
            }
            let count = (got as usize) / item;
            pids.truncate(count);
            if count < cap {
                return Some(pids);
            }
            cap = cap.saturating_mul(2);
        }
        Some(pids)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn process_fds(pid: libc::pid_t) -> Option<Vec<libc::proc_fdinfo>> {
        // SAFETY: NULL buffer queries the byte count needed for `pid`'s open descriptors.
        let bytes = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if bytes <= 0 {
            return None;
        }
        let item = std::mem::size_of::<libc::proc_fdinfo>();
        let mut cap = (bytes as usize / item).saturating_add(16);
        let mut fds = Vec::new();
        for _ in 0..3 {
            let buf_bytes = libc::c_int::try_from(cap.checked_mul(item)?).ok()?;
            // SAFETY: `libc::proc_fdinfo` is a plain C struct (`i32` + `u32`); all-zero is valid.
            fds.resize_with(cap, || unsafe { std::mem::zeroed() });
            // SAFETY: `fds` is a contiguous writable buffer of `buf_bytes` bytes.
            let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), buf_bytes) };
            if got <= 0 {
                return None;
            }
            let count = (got as usize) / item;
            fds.truncate(count);
            if count < cap {
                return Some(fds);
            }
            cap = cap.saturating_mul(2);
        }
        Some(fds)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn socket_fdinfo(pid: libc::pid_t, fd: i32) -> Option<SocketFdInfo> {
        // SAFETY: `SocketFdInfo` contains only plain integers and byte arrays; all-zero is valid.
        let mut info: SocketFdInfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<SocketFdInfo>() as libc::c_int;
        // SAFETY: `info` is a `#[repr(C)]` buffer of exactly `size` bytes matching Darwin's `struct socket_fdinfo`.
        let n = unsafe { libc::proc_pidfdinfo(pid, fd, PROC_PIDFDSOCKETINFO, (&raw mut info).cast(), size) };
        (n == size).then_some(info)
    }
}

/// In a `/proc/net/tcp{,6}` table, the inode of the socket whose own address is `local` and whose peer is `remote`
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn socket_inode(table: &str, local: SocketAddr, remote: SocketAddr) -> Option<u64> {
    table.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (l, r, inode) = (f.get(1)?, f.get(2)?, f.get(9)?);
        (parse_addr(l)? == local && parse_addr(r)? == remote).then(|| inode.parse().ok()).flatten()
    })
}

/// `0100007F:1F92` (IPv4) or 32 hex digits (IPv6), each 32-bit word in the kernel's byte order, then the port
fn parse_addr(s: &str) -> Option<SocketAddr> {
    let (addr, port) = s.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |i: usize| u32::from_str_radix(addr.get(i * 8..i * 8 + 8)?, 16).ok().map(u32::to_ne_bytes);
    let a = match addr.len() {
        8 => SocketAddr::new(std::net::Ipv4Addr::from(word(0)?).into(), port),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
            }
            SocketAddr::new(std::net::Ipv6Addr::from(b).into(), port)
        }
        _ => return None,
    };
    Some(unmapped(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_endian = "little")]
    fn reads_the_kernel_s_socket_table() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:2012 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0 100 0 0 10 0
   1: 0100007F:D431 0100007F:2012 01 00000000:00000000 00:00000000 00000000  1000        0 222 1 0 20 4 30 10 -1
   2: 0100007F:2012 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 333 1 0 20 4 30 10 -1";
        let (client, server): (SocketAddr, SocketAddr) =
            ("127.0.0.1:54321".parse().unwrap(), "127.0.0.1:8210".parse().unwrap());
        assert_eq!(socket_inode(table, client, server), Some(222), "the client's end, not ours");
        assert_eq!(socket_inode(table, server, client), Some(333));
        let v6 = "  sl  local_address                         remote_address                        st
   0: 0000000000000000FFFF00000100007F:D431 0000000000000000FFFF00000100007F:2012 01 00000000:00000000 00:00000000 00000000  1000        0 444 1";
        assert_eq!(socket_inode(v6, client, server), Some(444), "an IPv4-mapped address is the IPv4 one");
        assert_eq!(parse_addr("00000000000000000000000001000000:0050"), Some("[::1]:80".parse().unwrap()));
    }

    #[test]
    fn darwin_socket_fdinfo_layout_and_endpoint_decoding() {
        use std::mem::{offset_of, size_of};

        // Verified against XNU bsd/sys/proc_info.h on LP64 (arm64 / x86_64 macOS).
        assert_eq!(size_of::<darwin::ProcFileInfo>(), 24);
        assert_eq!(size_of::<darwin::VinfoStat>(), 136);
        assert_eq!(size_of::<darwin::SockbufInfo>(), 24);
        assert_eq!(size_of::<darwin::InSockInfo>(), 80);
        assert_eq!(size_of::<darwin::SocketProto>(), 528);
        assert_eq!(size_of::<darwin::SocketInfo>(), 768);
        assert_eq!(size_of::<darwin::SocketFdInfo>(), 792);
        assert_eq!(offset_of!(darwin::SocketFdInfo, psi), 24);
        assert_eq!(offset_of!(darwin::SocketInfo, soi_type), 152);
        assert_eq!(offset_of!(darwin::SocketInfo, soi_protocol), 156);
        assert_eq!(offset_of!(darwin::SocketInfo, soi_family), 160);
        assert_eq!(offset_of!(darwin::SocketInfo, soi_kind), 232);
        assert_eq!(offset_of!(darwin::SocketInfo, soi_proto), 240);
        assert_eq!(offset_of!(darwin::InSockInfo, insi_fport), 0);
        assert_eq!(offset_of!(darwin::InSockInfo, insi_lport), 4);
        assert_eq!(offset_of!(darwin::InSockInfo, insi_vflag), 24);
        assert_eq!(offset_of!(darwin::InSockInfo, insi_faddr), 32);
        assert_eq!(offset_of!(darwin::InSockInfo, insi_laddr), 48);

        let base = |family: i32, vflag: u8, laddr: [u8; 16], lport: u16, faddr: [u8; 16], fport: u16| {
            // SAFETY: all-zero is a valid `SocketFdInfo`.
            let mut info: darwin::SocketFdInfo = unsafe { std::mem::zeroed() };
            info.psi.soi_kind = darwin::SOCKINFO_TCP;
            info.psi.soi_type = darwin::DARWIN_SOCK_STREAM;
            info.psi.soi_protocol = darwin::DARWIN_IPPROTO_TCP;
            info.psi.soi_family = family;
            info.psi.soi_proto.in_info.insi_vflag = vflag;
            info.psi.soi_proto.in_info.insi_lport = i32::from(lport.to_be());
            info.psi.soi_proto.in_info.insi_fport = i32::from(fport.to_be());
            info.psi.soi_proto.in_info.insi_laddr = darwin::InAddr46(laddr);
            info.psi.soi_proto.in_info.insi_faddr = darwin::InAddr46(faddr);
            info
        };

        let lo4 = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 127, 0, 0, 1];
        let v4 = base(darwin::DARWIN_AF_INET, darwin::INI_IPV4, lo4, 54321, lo4, 255);
        assert_eq!(
            darwin::socket_endpoints(&v4),
            Some(("127.0.0.1:54321".parse().unwrap(), "127.0.0.1:255".parse().unwrap()))
        );

        let lo6 = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let v6 = base(darwin::DARWIN_AF_INET6, darwin::INI_IPV6, lo6, 54321, lo6, 8202);
        assert_eq!(
            darwin::socket_endpoints(&v6),
            Some(("[::1]:54321".parse().unwrap(), "[::1]:8202".parse().unwrap()))
        );

        // IPv4-mapped IPv6 reported either with INP_IPV4|INP_V4MAPPEDV6 or with INI_IPV6 is normalised to IPv4.
        let mapped_v4flag = base(darwin::DARWIN_AF_INET6, darwin::INI_IPV4 | 0x4, lo4, 54321, lo4, 8202);
        assert_eq!(
            darwin::socket_endpoints(&mapped_v4flag),
            Some(("127.0.0.1:54321".parse().unwrap(), "127.0.0.1:8202".parse().unwrap()))
        );
        let mapped_bytes = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 127, 0, 0, 1];
        let mapped_v6flag = base(darwin::DARWIN_AF_INET6, darwin::INI_IPV6, mapped_bytes, 54321, mapped_bytes, 8202);
        assert_eq!(
            darwin::socket_endpoints(&mapped_v6flag),
            Some(("127.0.0.1:54321".parse().unwrap(), "127.0.0.1:8202".parse().unwrap()))
        );

        // Non-TCP / listening / out-of-range port sockets are ignored.
        let mut unix = v4;
        unix.psi.soi_kind = 3; // SOCKINFO_UN
        assert_eq!(darwin::socket_endpoints(&unix), None);
        let mut listening = v4;
        listening.psi.soi_proto.in_info.insi_fport = 0;
        assert_eq!(darwin::socket_endpoints(&listening), None);
        let mut bad_port = v4;
        bad_port.psi.soi_proto.in_info.insi_lport = -1;
        assert_eq!(darwin::socket_endpoints(&bad_port), None);
    }

    #[test]
    fn a_client_on_another_machine_isn_t_looked_up() {
        assert_eq!(local_client("192.168.1.5:5000".parse().unwrap(), "0.0.0.0:8202".parse().unwrap()), None);
    }

    /// Child process helper for [`finds_the_process_on_the_other_end`]: holds a TCP connection to
    /// `ICTL_TEST_PEER_CONNECT` open until its stdin closes.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn loopback_client_worker() {
        let Some(target) = std::env::var_os("ICTL_TEST_PEER_CONNECT") else { return };
        let Ok(_stream) = std::net::TcpStream::connect(target.to_string_lossy().as_ref()) else { return };
        let _ = std::io::Read::read(&mut std::io::stdin(), &mut [0u8; 1]);
    }

    /// The real thing on both Linux (`/proc`) and macOS (`libproc`): a separate child process connects over IPv4 and
    /// IPv6 loopback, its PID is found, our own end is ignored, and an in-process connection returns `None`.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn finds_the_process_on_the_other_end() {
        use std::net::{TcpListener, TcpStream};
        use std::process::{Child, Command, Stdio};

        struct KillOnDrop(Child);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let check_child = |listener: TcpListener| {
            let addr = listener.local_addr().unwrap();
            let mut child = KillOnDrop(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "viewer::peer::tests::loopback_client_worker", "--test-threads=1"])
                    .env("ICTL_TEST_PEER_CONNECT", addr.to_string())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let (_conn, peer) = listener.accept().unwrap();
            let found = (0..50).find_map(|_| {
                local_client(peer, addr).or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    None
                })
            });
            assert_eq!(found, Some(child.0.id()), "child PID for {peer} -> {addr}");
            assert_eq!(local_client(addr, peer), None, "our own end must be skipped");
            // Drop stdin so the worker exits cleanly before KillOnDrop runs.
            drop(child.0.stdin.take());
        };

        // IPv4 loopback (required on every platform).
        let v4 = TcpListener::bind("127.0.0.1:0").unwrap();
        let v4_addr = v4.local_addr().unwrap();
        // An in-process loopback client shares our own PID and must be skipped.
        let _self_client = TcpStream::connect(v4_addr).unwrap();
        let (_self_server, self_peer) = v4.accept().unwrap();
        assert_eq!(local_client(self_peer, v4_addr), None, "own PID must not be returned");
        check_child(v4);

        // IPv6 loopback (required on macOS; skipped only if the host/container has no IPv6 loopback).
        match TcpListener::bind("[::1]:0") {
            Ok(v6) => check_child(v6),
            Err(e) if cfg!(target_os = "linux") && e.kind() == std::io::ErrorKind::AddrNotAvailable => {}
            Err(e) => panic!("bind [::1]:0 failed: {e}"),
        }
    }
}

//! Which local process is on the other end of a connection to the page. A task's agent could call the page's API
//! (`curl`, a browser it drives) to answer its own confirmations; the daemon refuses that when it knows the process
//! (`ictl_daemon::procs`). Linux only (`/proc`); elsewhere `None`, and only the daemon's own checks apply.

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

#[cfg(not(target_os = "linux"))]
fn find(_client: SocketAddr, _server: SocketAddr) -> Option<u32> {
    None
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
    fn a_client_on_another_machine_isn_t_looked_up() {
        assert_eq!(local_client("192.168.1.5:5000".parse().unwrap(), "0.0.0.0:8202".parse().unwrap()), None);
    }

    /// The real thing: a child connects (no process of its own holds the socket: `read` is a builtin)
    #[test]
    #[cfg(target_os = "linux")]
    fn finds_the_process_on_the_other_end() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        let script = format!("exec 3<>/dev/tcp/127.0.0.1/{}; read -t 5 -u 3", addr.port());
        let Ok(mut child) = std::process::Command::new("bash").args(["-c", &script]).spawn() else { return };
        let (_conn, peer) = server.accept().unwrap();
        let found = (0..50).find_map(|_| {
            local_client(peer, addr).or_else(|| {
                std::thread::sleep(std::time::Duration::from_millis(20));
                None
            })
        });
        assert_eq!(found, Some(child.id()));
        let _ = child.kill();
        let _ = child.wait();
    }
}

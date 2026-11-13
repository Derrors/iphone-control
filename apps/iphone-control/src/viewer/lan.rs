//! Who may reach the control page (docs/task-control.md 7.2, D3).
//!
//! By default the page is on `127.0.0.1` only, as before. With `viewer --lan` (`IPHONE_CONTROL_VIEWER_LAN=1`) the
//! same port, still plain HTTP and **without a login** (that comes later), listens on every IPv4 interface, so the
//! page can be opened from another machine on the local network. What still protects it:
//!
//! - connections only from loopback, private (10/8, 172.16/12, 192.168/16), link-local (169.254/16) and CGNAT
//!   (100.64/10, which Tailscale uses) addresses: a machine reachable from the internet isn't operated from there;
//! - the `Host` header must name this machine (loopback names, its host name and `<name>.local`, its interfaces'
//!   addresses, names given with `--lan-host`), against DNS rebinding;
//! - an action's `Origin` must be this page at one of those names, against other web pages posting to it.

use std::net::{IpAddr, Ipv4Addr};

const LOOPBACK: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// The names the page answers to
#[derive(Debug, Clone)]
pub struct Access {
    lan: bool,
    /// Besides the loopback names, lowercase; IPv6 addresses in brackets
    hosts: Vec<String>,
    /// This machine's IPv4 addresses other than loopback, to print
    addresses: Vec<Ipv4Addr>,
}

impl Access {
    /// `127.0.0.1` only
    pub fn local() -> Self {
        Self { lan: false, hosts: Vec::new(), addresses: Vec::new() }
    }

    /// Every IPv4 interface: this machine's names and addresses, and `extra` (`--lan-host`)
    pub fn lan(extra: &[String]) -> Self {
        let addresses: Vec<Ipv4Addr> = interface_ipv4s().into_iter().filter(|a| !a.is_loopback()).collect();
        let mut names: Vec<String> = Vec::new();
        if let Some(h) = hostname() {
            if !h.contains('.') {
                names.push(format!("{h}.local"));
            }
            names.push(h);
        }
        names.extend(addresses.iter().map(Ipv4Addr::to_string));
        names.extend(extra.iter().cloned());
        let mut a = Self::with_hosts(names);
        a.addresses = addresses;
        a
    }

    /// Open to the network, answering to `names` (and the loopback names)
    pub fn with_hosts(names: impl IntoIterator<Item = String>) -> Self {
        let mut hosts: Vec<String> = Vec::new();
        for n in names {
            let n = n.trim().trim_end_matches('.').to_ascii_lowercase();
            if !n.is_empty() && !LOOPBACK.contains(&n.as_str()) && !hosts.contains(&n) {
                hosts.push(n);
            }
        }
        Self { lan: true, hosts, addresses: Vec::new() }
    }

    pub fn is_lan(&self) -> bool {
        self.lan
    }

    /// Where to listen
    pub fn bind_ip(&self) -> Ipv4Addr {
        if self.lan { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST }
    }

    /// This machine's network addresses (for the startup message)
    pub fn addresses(&self) -> &[Ipv4Addr] {
        &self.addresses
    }

    fn knows(&self, name: &str) -> bool {
        LOOPBACK.contains(&name) || (self.lan && self.hosts.iter().any(|h| h == name))
    }

    /// The request's `Host` names this machine
    pub fn host_allowed(&self, head: &[u8]) -> bool {
        host_header(head).is_some_and(|h| self.knows(&h))
    }

    /// An action's `Origin` is this page: `http://<a known name>:<port>`
    pub fn origin_allowed(&self, origin: &str, port: u16) -> bool {
        let Some(rest) = origin.strip_prefix("http://") else { return false };
        let rest = rest.to_ascii_lowercase();
        let Some((name, p)) = rest.rsplit_once(':') else { return false };
        p.parse::<u16>().ok() == Some(port) && self.knows(name)
    }
}

/// A connection from this address may be served (on the local network only)
pub fn peer_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let [o0, o1, ..] = a.octets();
            a.is_loopback() || a.is_private() || a.is_link_local() || (o0 == 100 && (o1 & 0xc0) == 64)
        }
        IpAddr::V6(a) => {
            let s0 = a.segments()[0];
            a.is_loopback()
                || (s0 & 0xfe00) == 0xfc00
                || (s0 & 0xffc0) == 0xfe80
                || a.to_ipv4_mapped().is_some_and(|v4| peer_allowed(IpAddr::V4(v4)))
        }
    }
}

/// The `Host` header's name without the port, lowercase; an IPv6 address in brackets
fn host_header(head: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(head);
    let host = text.split("\r\n").skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("host").then(|| v.trim().to_ascii_lowercase())
    })?;
    Some(match host.strip_prefix('[') {
        Some(rest) => format!("[{}]", rest.split(']').next().unwrap_or_default()),
        None => host.split(':').next().unwrap_or_default().trim_end_matches('.').to_owned(),
    })
}

fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname writes at most that many bytes
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = std::str::from_utf8(&buf[..end]).ok()?.trim().to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

/// The IPv4 addresses of this machine's interfaces
fn interface_ipv4s() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, which is walked read-only and freed once
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut p = list;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() && i32::from((*a).sa_family) == libc::AF_INET {
                let sin = &*(a as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !out.contains(&ip) {
                    out.push(ip);
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(host: &str) -> Vec<u8> {
        format!("GET /api/status HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes()
    }

    #[test]
    fn only_the_local_network_may_connect() {
        for ok in [
            "127.0.0.1",
            "10.1.2.3",
            "172.20.0.5",
            "192.168.10.15",
            "169.254.1.1",
            "100.100.1.1",
            "::1",
            "fd7a:115c::1",
            "fe80::1",
            "::ffff:192.168.1.2",
        ] {
            assert!(peer_allowed(ok.parse().unwrap()), "{ok}");
        }
        for no in ["8.8.8.8", "172.32.0.1", "100.128.0.1", "1.1.1.1", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!peer_allowed(no.parse().unwrap()), "{no}");
        }
    }

    #[test]
    fn without_lan_only_loopback_names() {
        let a = Access::local();
        assert!(!a.is_lan());
        assert_eq!(a.bind_ip(), Ipv4Addr::LOCALHOST);
        for h in ["127.0.0.1:8202", "localhost:8202", "[::1]:8202", "LOCALHOST"] {
            assert!(a.host_allowed(&head(h)), "{h}");
        }
        assert!(!a.host_allowed(&head("192.168.10.100:8202")), "the network's name for it doesn't count");
        assert!(!a.host_allowed(b"GET / HTTP/1.1\r\n\r\n"), "no Host");
        assert!(a.origin_allowed("http://127.0.0.1:8202", 8202));
        assert!(!a.origin_allowed("http://192.168.10.100:8202", 8202));
    }

    #[test]
    fn on_the_lan_its_own_names_but_no_others() {
        let a = Access::with_hosts(["192.168.10.100".into(), "NAS.local.".into(), "nas".into(), "localhost".into()]);
        assert!(a.is_lan());
        assert_eq!(a.bind_ip(), Ipv4Addr::UNSPECIFIED);
        for h in ["192.168.10.100:8202", "nas.local", "NAS:8202", "127.0.0.1:8202"] {
            assert!(a.host_allowed(&head(h)), "{h}");
        }
        // DNS rebinding: a name that resolves to this machine but isn't one of its own
        for h in ["evil.example:8202", "192.168.10.101:8202", "nas.evil.example"] {
            assert!(!a.host_allowed(&head(h)), "{h}");
        }
        assert!(a.origin_allowed("http://192.168.10.100:8202", 8202));
        assert!(a.origin_allowed("http://NAS.local:8202", 8202));
        assert!(!a.origin_allowed("https://192.168.10.100:8202", 8202), "only what is served");
        assert!(!a.origin_allowed("http://192.168.10.100:3000", 8202), "another port");
        assert!(!a.origin_allowed("http://evil.example:8202", 8202));
        assert!(!a.origin_allowed("null", 8202));
    }

    #[test]
    fn this_machine_has_addresses() {
        assert!(interface_ipv4s().contains(&Ipv4Addr::LOCALHOST));
    }
}

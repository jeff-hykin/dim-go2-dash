// Make every live host on this computer's subnet answer ARP, so the neighbour table (arp.rs) lists it: what
// find-go2.sh did with `nmap -sn`, without root, on macOS and Linux alike. Each address gets one ping (an unprivileged
// ICMP datagram socket: always allowed on macOS, on Linux when net.ipv4.ping_group_range covers us) or, failing that,
// an empty UDP packet to the discard port; either way the kernel ARPs for the address first, and that is the point.
// Office Wi-Fi drops the LAN discovery multicast (231.1.1.1), so waiting longer on that finds nothing: this does.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::time::Instant;

use crate::lan::is_tunnel_iface;

/// The biggest subnet swept: a /16 (65 534 addresses). A wider interface subnet is swept as the /16 around this computer.
pub const WIDEST_PREFIX: u8 = 16;
/// The quick sweep (the default scan) covers the whole subnet up to this many addresses, else the /24 around this computer.
pub const QUICK_MAX_HOSTS: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subnet {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix.min(32))
    }
}

impl Subnet {
    pub fn new(ip: Ipv4Addr, prefix: u8) -> Subnet {
        let prefix = prefix.min(32);
        Subnet { network: Ipv4Addr::from(u32::from(ip) & mask(prefix)), prefix }
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & mask(self.prefix) == u32::from(self.network)
    }

    /// First and last usable address (no network/broadcast, except in a /31 or /32).
    fn range(&self) -> (u32, u32) {
        let base = u32::from(self.network) as u64;
        let size = 1u64 << (32 - self.prefix as u32);
        match self.prefix {
            32 => (base as u32, base as u32),
            31 => (base as u32, (base + 1) as u32),
            _ => ((base + 1) as u32, (base + size - 2) as u32),
        }
    }

    pub fn host_count(&self) -> u32 {
        let (first, last) = self.range();
        last - first + 1
    }

    pub fn hosts(&self) -> impl Iterator<Item = Ipv4Addr> {
        let (first, last) = self.range();
        (first..=last).map(Ipv4Addr::from)
    }

    /// This subnet, narrowed to the /`prefix` around `ip` when it is wider than that.
    pub fn at_most(&self, ip: Ipv4Addr, prefix: u8) -> Subnet {
        if self.prefix >= prefix {
            *self
        } else {
            Subnet::new(ip, prefix)
        }
    }
}

impl fmt::Display for Subnet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

/// Every address of `subnet` but `own`, the /24 around `own` first (a dog leased near us shows up in the first second).
pub fn sweep_order(subnet: Subnet, own: Ipv4Addr) -> Vec<Ipv4Addr> {
    let near = Subnet::new(own, 24.max(subnet.prefix));
    let (first, last) = near.range();
    let in_near = |ip: &Ipv4Addr| (first..=last).contains(&u32::from(*ip));
    let mut out: Vec<Ipv4Addr> = near.hosts().filter(|ip| *ip != own && subnet.contains(*ip)).collect();
    out.extend(subnet.hosts().filter(|ip| *ip != own && !in_near(ip)));
    out
}

// ── which network ──

/// Linux `/proc/net/route`: the interface of the default route (destination and mask 0) with the lowest metric.
pub fn parse_proc_net_route(text: &str) -> Option<String> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let default = fields.get(1) == Some(&"00000000") && fields.get(7) == Some(&"00000000");
            let metric: u32 = fields.get(6)?.parse().ok()?;
            default.then(|| (metric, fields[0].to_string()))
        })
        .min()
        .map(|(_, iface)| iface)
}

/// macOS `route -n get default`: its `interface: en0` line.
pub fn parse_route_get(text: &str) -> Option<String> {
    text.lines().find_map(|line| line.trim().strip_prefix("interface:").map(|iface| iface.trim().to_string()))
}

async fn default_iface() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        parse_proc_net_route(&tokio::fs::read_to_string("/proc/net/route").await.ok()?)
    }
    #[cfg(target_os = "macos")]
    {
        parse_route_get(&crate::arp::run(&["/sbin/route", "-n", "get", "default"]).await)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

fn is_private(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64 // + CGNAT, some hotspots use it
}

/// This computer's network: an interface, its IPv4 address and subnet.
#[derive(Clone, Debug)]
pub struct Lan {
    pub iface: String,
    pub ip: Ipv4Addr,
    pub subnet: Subnet,
}

/// The default route's interface when it is a real one; with a full-tunnel VPN on the default route, the first real
/// interface with a private address (the dog is on the Wi-Fi, not the VPN).
pub async fn local_lan() -> Option<Lan> {
    let default = default_iface().await;
    let mut lans: Vec<Lan> = if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .filter(|i| !is_tunnel_iface(&i.name))
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() && !v4.ip.is_link_local() => {
                Some(Lan { iface: i.name, ip: v4.ip, subnet: Subnet::new(v4.ip, v4.prefixlen) })
            }
            _ => None,
        })
        .collect();
    lans.sort_by_key(|lan| (Some(&lan.iface) != default.as_ref(), !is_private(lan.ip)));
    lans.into_iter().next()
}

// ── the probe ──

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Icmp,
    Udp,
}

impl Method {
    pub fn label(&self) -> &'static str {
        match self {
            Method::Icmp => "ping",
            Method::Udp => "UDP touch",
        }
    }
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = bytes.chunks(2).map(|pair| u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]) as u32).sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn echo_request(ident: u16, seq: u16) -> Vec<u8> {
    let mut packet = vec![8, 0, 0, 0];
    packet.extend_from_slice(&ident.to_be_bytes());
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(b"find-go2");
    let sum = checksum(&packet);
    packet[2..4].copy_from_slice(&sum.to_be_bytes());
    packet
}

/// An ICMP datagram socket's packet is an echo reply: Linux hands over the ICMP message, macOS the IP packet around it.
fn is_echo_reply(data: &[u8]) -> bool {
    let icmp = if data.first().map(|b| b >> 4) == Some(4) && data.len() >= 20 { &data[((data[0] & 0x0f) as usize * 4).min(data.len())..] } else { data };
    icmp.first() == Some(&0)
}

struct Prober {
    socket: Socket,
    method: Method,
}

impl Prober {
    fn open(iface: Option<&str>) -> io::Result<Prober> {
        let (socket, method) = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::ICMPV4)) {
            Ok(socket) => (socket, Method::Icmp),
            // Linux outside ping_group_range: EACCES
            Err(_) => (Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?, Method::Udp),
        };
        socket.set_nonblocking(true)?;
        if let Some(iface) = iface {
            crate::lan::bind_to_iface(&socket, iface);
        }
        Ok(Prober { socket, method })
    }

    fn send(&self, ip: Ipv4Addr, seq: u16) -> io::Result<()> {
        let result = match self.method {
            Method::Icmp => self.socket.send_to(&echo_request(0x6f32, seq), &SockAddr::from(SocketAddrV4::new(ip, 0))),
            Method::Udp => self.socket.send_to(b"", &SockAddr::from(SocketAddrV4::new(ip, 9))),
        };
        result.map(|_| ())
    }

    fn drain_replies(&self, alive: &mut HashSet<Ipv4Addr>) {
        if self.method != Method::Icmp {
            return;
        }
        let mut buf = [MaybeUninit::<u8>::uninit(); 1500];
        while let Ok((count, from)) = self.socket.recv_from(&mut buf) {
            let data: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, count) };
            if let (true, Some(from)) = (is_echo_reply(data), from.as_socket_ipv4()) {
                alive.insert(*from.ip());
            }
        }
    }
}

fn busy(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock || err.raw_os_error() == Some(libc::ENOBUFS)
}

/// Probes per second. Linux holds each unanswered address in the neighbour table ~3 s (3 solicits, 1 s apart) and
/// refuses new ones past gc_thresh3 (1024 by default), dropping the probe: so stay within that. macOS has no such cap.
fn probe_rate() -> usize {
    #[cfg(target_os = "linux")]
    {
        let read = |path: &str| std::fs::read_to_string(path).ok().and_then(|t| t.trim().parse::<usize>().ok());
        let cap = read("/proc/sys/net/ipv4/neigh/default/gc_thresh3").unwrap_or(1024);
        let used = std::fs::read_to_string("/proc/net/arp").map(|t| t.lines().count().saturating_sub(1)).unwrap_or(0);
        (cap.saturating_sub(used + 128) * 7 / 30).clamp(100, 2000)
    }
    #[cfg(not(target_os = "linux"))]
    {
        1000
    }
}

pub struct Progress {
    pub sent: usize,
    pub total: usize,
    pub alive: usize,
}

pub struct Swept {
    pub method: Method,
    pub sent: usize,
    pub alive: HashSet<Ipv4Addr>,
    pub cancelled: bool,
    pub rate: usize,
}

/// Probe every target once, paced, then wait a moment for the stragglers' ARP. `cancel` stops it between batches.
pub async fn sweep(targets: &[Ipv4Addr], iface: Option<&str>, cancel: &AtomicBool, mut on_progress: impl FnMut(Progress)) -> io::Result<Swept> {
    let prober = Prober::open(iface)?;
    let rate = probe_rate();
    let mut alive = HashSet::new();
    let mut sent = 0;
    let mut last_report = Instant::now();
    let mut cancelled = false;
    // paced by the clock, not by sleeps: macOS coalesces a background process's short timers (a 50 ms sleep can take
    // 300 ms), so a fixed batch per sleep ran ~6x slower than `rate`
    let begun = Instant::now();
    while sent < targets.len() {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let due = ((begun.elapsed().as_secs_f64() * rate as f64) as usize + 1).min(targets.len());
        while sent < due {
            for _attempt in 0..5 {
                match prober.send(targets[sent], sent as u16) {
                    Err(err) if busy(&err) => tokio::time::sleep(Duration::from_millis(20)).await,
                    _ => break, // sent, or this address is unreachable/down: either way, next
                }
            }
            sent += 1;
        }
        prober.drain_replies(&mut alive);
        if last_report.elapsed() >= Duration::from_millis(250) {
            on_progress(Progress { sent, total: targets.len(), alive: alive.len() });
            last_report = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // the last addresses' ARP takes up to a second to resolve
    let settle = Instant::now() + Duration::from_millis(if cancelled { 0 } else { 1500 });
    while Instant::now() < settle {
        prober.drain_replies(&mut alive);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // touch the hosts that answered once more: a big sweep can push their entries out of a full table
    if !cancelled && alive.len() <= 4096 {
        for (seq, ip) in alive.iter().enumerate() {
            let _ = prober.send(*ip, seq as u16);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    on_progress(Progress { sent, total: targets.len(), alive: alive.len() });
    Ok(Swept { method: prober.method, sent, alive, cancelled, rate })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> Ipv4Addr {
        text.parse().unwrap()
    }

    #[test]
    fn subnet_math() {
        let office = Subnet::new(ip("10.10.176.251"), 17);
        assert_eq!(office.to_string(), "10.10.128.0/17");
        assert_eq!(office.host_count(), 32766);
        assert!(office.contains(ip("10.10.255.254")) && !office.contains(ip("10.11.0.1")));
        assert_eq!(office.hosts().next(), Some(ip("10.10.128.1")));
        assert_eq!(office.hosts().last(), Some(ip("10.10.255.254")));
        assert_eq!(office.at_most(ip("10.10.176.251"), 24).to_string(), "10.10.176.0/24");
        assert_eq!(office.at_most(ip("10.10.176.251"), 16), office);
        let home = Subnet::new(ip("192.168.1.20"), 24);
        assert_eq!(home.host_count(), 254);
        assert_eq!(Subnet::new(ip("10.0.0.0"), 8).at_most(ip("10.3.4.5"), WIDEST_PREFIX).to_string(), "10.3.0.0/16");
        assert_eq!(Subnet::new(ip("10.0.0.7"), 31).host_count(), 2);
        assert_eq!(Subnet::new(ip("10.0.0.7"), 32).hosts().collect::<Vec<_>>(), vec![ip("10.0.0.7")]);
    }

    #[test]
    fn sweep_order_starts_near_us() {
        let order = sweep_order(Subnet::new(ip("10.10.176.251"), 17), ip("10.10.176.251"));
        assert_eq!(order.len(), 32765);
        assert_eq!(order[0], ip("10.10.176.1"));
        assert_eq!(order[253], ip("10.10.128.1")); // 253 of the /24 (not us), then the rest from the start
        assert!(!order.contains(&ip("10.10.176.251")));
        assert_eq!(order.iter().collect::<HashSet<_>>().len(), order.len());
        let small = sweep_order(Subnet::new(ip("192.168.1.20"), 26), ip("192.168.1.20"));
        assert_eq!(small.len(), 61);
    }

    #[test]
    fn default_route_parsers() {
        let proc_route = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
tailscale0\t00000000\t00000000\t0001\t0\t0\t900\t00000000\t0\t0\t0
wlp0s20f3\t00000000\t0180A0A0\t0003\t0\t0\t600\t00000000\t0\t0\t0
wlp0s20f3\t00800A0A\t00000000\t0001\t0\t0\t600\t0080FFFF\t0\t0\t0
";
        assert_eq!(parse_proc_net_route(proc_route).as_deref(), Some("wlp0s20f3"));
        assert_eq!(parse_proc_net_route("Iface\tDestination\n"), None);
        let route_get = "   route to: default\ndestination: default\n       mask: default\n    gateway: 10.10.128.1\n  interface: en0\n      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>\n";
        assert_eq!(parse_route_get(route_get).as_deref(), Some("en0"));
    }

    #[test]
    fn icmp_packets() {
        let packet = echo_request(0x1234, 7);
        assert_eq!(checksum(&packet), 0); // a correct checksum sums to zero
        let mut reply = packet.clone();
        reply[0] = 0;
        assert!(is_echo_reply(&reply));
        let mut with_ip_header = vec![0x45u8; 1];
        with_ip_header.extend([0u8; 19]);
        with_ip_header.extend(&reply);
        assert!(is_echo_reply(&with_ip_header));
        assert!(!is_echo_reply(&packet));
    }
}

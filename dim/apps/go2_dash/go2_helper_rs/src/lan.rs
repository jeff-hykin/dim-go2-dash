// LAN discovery for Unitree Go2 robots. Faithful port of dimos' landiscovery.py
// plus the go2_helper macOS broadcast variant.
//
// Go2s answer a UDP probe (multicast group 231.1.1.1 port 10131) with JSON
// containing their serial + IP; replies land on port 10134. We pin the send/recv
// to each real interface so a VPN tun device can't silently swallow the probe.

use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

pub const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(231, 1, 1, 1);
pub const QUERY_PORT: u16 = 10131;
pub const REPLY_PORT: u16 = 10134;

fn query_payload() -> Vec<u8> {
    br#"{"name": "unitree_dapengche"}"#.to_vec()
}

#[derive(Clone)]
pub struct LanDevice {
    pub serial: String,
    pub ip: String,
    pub mac: Option<String>,
}

const SKIP_PREFIXES: [&str; 11] = [
    "lo", "tailscale", "wg", "tun", "utun", "docker", "br-", "veth", "awdl", "llw", "bridge",
];

/// (name, ipv4, broadcast) for real, non-tunnel IPv4 interfaces. Replaces psutil.
pub fn wifi_ifaces() -> Vec<(String, Ipv4Addr, Option<Ipv4Addr>)> {
    let mut out = Vec::new();
    let interfaces = match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(_) => return out,
    };
    for interface in interfaces {
        if SKIP_PREFIXES.iter().any(|p| interface.name.starts_with(p))
            || interface.name.starts_with("Meta")
        {
            continue;
        }
        if let if_addrs::IfAddr::V4(v4) = interface.addr {
            if v4.ip.is_loopback() {
                continue;
            }
            out.push((interface.name, v4.ip, v4.broadcast));
        }
    }
    out
}

#[cfg(target_os = "macos")]
const IP_BOUND_IF: libc::c_int = 25;

/// macOS: pin outgoing packets to a specific interface (IP_BOUND_IF). No-op elsewhere.
#[cfg(target_os = "macos")]
fn bind_to_iface(socket: &Socket, name: &str) {
    use std::os::unix::io::AsRawFd;
    let cname = match std::ffi::CString::new(name) {
        Ok(cname) => cname,
        Err(_) => return,
    };
    let index = unsafe { libc::if_nametoindex(cname.as_ptr()) };
    if index == 0 {
        return;
    }
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            IP_BOUND_IF,
            &index as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_uint>() as libc::socklen_t,
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn bind_to_iface(_socket: &Socket, _name: &str) {}

/// Probe one interface for Go2s. Sends to the multicast group (always) and the
/// directed broadcast (when `use_broadcast`), then collects replies until timeout.
fn probe_iface(
    name: &str,
    ipv4: Ipv4Addr,
    broadcast: Option<Ipv4Addr>,
    timeout: Duration,
    use_broadcast: bool,
) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    let socket = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(socket) => socket,
        Err(_) => return found,
    };
    let _ = socket.set_reuse_address(true);
    if use_broadcast {
        let _ = socket.set_broadcast(true);
    }
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, REPLY_PORT);
    if socket.bind(&SockAddr::from(bind_addr)).is_err() {
        return found;
    }
    bind_to_iface(&socket, name);
    let _ = socket.set_multicast_if_v4(&ipv4);
    let _ = socket.join_multicast_v4(&MULTICAST_GROUP, &ipv4);

    let payload = query_payload();
    let mut targets: Vec<Ipv4Addr> = Vec::new();
    if use_broadcast {
        if let Some(broadcast) = broadcast {
            targets.push(broadcast);
        }
    }
    targets.push(MULTICAST_GROUP);
    let mut sent = false;
    for target in &targets {
        let dest = SocketAddrV4::new(*target, QUERY_PORT);
        if socket.send_to(&payload, &SockAddr::from(dest)).is_ok() {
            sent = true;
        }
    }
    if !sent {
        return found;
    }

    let _ = socket.set_read_timeout(Some(timeout));
    let deadline = Instant::now() + timeout;
    let mut buf = [MaybeUninit::<u8>::uninit(); 2048];
    while Instant::now() < deadline {
        let (count, addr) = match socket.recv_from(&mut buf) {
            Ok(result) => result,
            Err(_) => break,
        };
        let data: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, count) };
        let value: serde_json::Value = match serde_json::from_slice(data) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let serial = match value.get("sn").and_then(|v| v.as_str()) {
            Some(serial) => serial.to_string(),
            None => continue,
        };
        let ip = value
            .get("ip")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| addr.as_socket_ipv4().map(|s| s.ip().to_string()))
            .unwrap_or_default();
        if !found.iter().any(|(s, _)| s == &serial) {
            found.push((serial, ip));
        }
    }
    found
}

/// Cross-platform multicast discovery across every non-tunnel interface.
/// Mirrors dimos' `discover`.
pub fn discover_multicast(timeout: Duration) -> Vec<LanDevice> {
    let mut devices: Vec<LanDevice> = Vec::new();
    for (name, ipv4, broadcast) in wifi_ifaces() {
        for (serial, ip) in probe_iface(&name, ipv4, broadcast, timeout, false) {
            if !devices.iter().any(|d| d.serial == serial) {
                let mac = resolve_mac(&ip);
                devices.push(LanDevice { serial, ip, mac });
            }
        }
    }
    devices
}

/// macOS broadcast discovery (covers the full-tunnel-VPN case). Mirrors
/// `discover_lan_macos` in go2_helper.py.
pub fn discover_broadcast(timeout: Duration) -> Vec<LanDevice> {
    let mut devices: Vec<LanDevice> = Vec::new();
    for (name, ipv4, broadcast) in wifi_ifaces() {
        for (serial, ip) in probe_iface(&name, ipv4, broadcast, timeout, true) {
            if !devices.iter().any(|d| d.serial == serial) {
                devices.push(LanDevice {
                    serial,
                    ip,
                    mac: None,
                });
            }
        }
    }
    devices
}

/// Linux: resolve an IP's MAC via /proc/net/arp. Returns None on macOS.
pub fn resolve_mac(ip: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/net/arp").ok()?;
        for line in text.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 && parts[0] == ip && parts[3] != "00:00:00:00:00:00" {
                return Some(parts[3].to_uppercase());
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = ip;
        None
    }
}

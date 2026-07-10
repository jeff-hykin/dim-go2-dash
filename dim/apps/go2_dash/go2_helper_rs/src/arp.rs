// ARP fallback (Layer 2) LAN discovery, for when the multicast/broadcast probe
// is blocked (e.g. a full-tunnel VPN kill-switch). The OS ARP cache still knows
// IP<->MAC for hosts it overheard; we match Go2s by their Wi-Fi MAC OUI and
// surface their IPs — flagged UNVERIFIED (ARP gives no serial), then confirm each
// with a non-disruptive TCP probe to a Go2 signaling port.

use std::net::Ipv4Addr;
use std::time::Duration;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::lan::wifi_ifaces;

const GO2_OUIS: [&str; 1] = ["94:ba:06"];
const GO2_SIGNALING_PORTS: [u16; 2] = [9991, 8081];
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

fn norm_mac(mac: &str) -> String {
    let parts: Vec<String> = mac
        .split(':')
        .map(|part| match u8::from_str_radix(part, 16) {
            Ok(byte) => format!("{byte:02x}"),
            Err(_) => part.to_lowercase(),
        })
        .collect();
    parts.join(":")
}

fn looks_like_mac(token: &str) -> bool {
    let parts: Vec<&str> = token.split(':').collect();
    parts.len() == 6 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn parse_ipv4(token: &str) -> Option<Ipv4Addr> {
    token.trim_matches(|c| c == '(' || c == ')').parse().ok()
}

/// Read the OS ARP/neighbour table and return Go2 (ip, mac) pairs. Handles
/// macOS `arp -an`, Linux `/proc/net/arp`, and `ip neigh` line formats.
pub async fn arp_scan() -> Vec<(String, String)> {
    let text = read_arp_table().await;
    let mut hits: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let ip = tokens.iter().find_map(|t| parse_ipv4(t));
        let mac = tokens.iter().find(|t| looks_like_mac(t));
        if let (Some(ip), Some(mac)) = (ip, mac) {
            let normalized = norm_mac(mac);
            if GO2_OUIS.iter().any(|oui| normalized.starts_with(oui)) {
                let ip = ip.to_string();
                if !hits.iter().any(|(existing, _)| existing == &ip) {
                    hits.push((ip, normalized));
                }
            }
        }
    }
    hits
}

async fn read_arp_table() -> String {
    #[cfg(target_os = "macos")]
    {
        run(&["arp", "-an"]).await
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = tokio::fs::read_to_string("/proc/net/arp").await {
            return text;
        }
        run(&["ip", "neigh"]).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        String::new()
    }
}

#[allow(dead_code)]
async fn run(argv: &[&str]) -> String {
    match tokio::process::Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .await
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// Nudge the OS ARP cache: touch every host on each Wi-Fi /24 so live hosts show
/// up in the table a moment later. L3 sends fail under a VPN firewall but the
/// kernel still ARPs (L2) for live hosts.
pub fn arp_sweep() {
    for (_name, ipv4, _broadcast) in wifi_ifaces() {
        let socket = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
            Ok(socket) => socket,
            Err(_) => continue,
        };
        let _ = socket.set_nonblocking(true);
        let octets = ipv4.octets();
        for host in 1u8..=254 {
            let target = Ipv4Addr::new(octets[0], octets[1], octets[2], host);
            let dest = std::net::SocketAddrV4::new(target, 9);
            let _ = socket.send_to(b"", &SockAddr::from(dest));
        }
    }
}

/// Non-disruptive liveness + identity check: is a Go2 signaling port answering?
/// Never sends an SDP offer, so it can't disturb an existing control session.
pub async fn go2_alive(ip: &str) -> bool {
    for port in GO2_SIGNALING_PORTS {
        let address = format!("{ip}:{port}");
        if let Ok(Ok(stream)) = timeout(PROBE_TIMEOUT, TcpStream::connect(&address)).await {
            drop(stream);
            return true;
        }
    }
    false
}

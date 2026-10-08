// The OS neighbour (ARP) table, and how a Go2 is recognized in it. Office Wi-Fi drops the multicast group LAN
// discovery uses (231.1.1.1), so there the ARP table is the only reliable way to a dog's IP: sweep.rs makes every live
// host answer ARP, then the table is read here and joined to Bluetooth by MAC (a Go2's Wi-Fi MAC is its Bluetooth MAC
// with the last byte minus one).

use std::net::Ipv4Addr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::timeout;

/// Unitree's MAC prefixes (lowercase, zero-padded).
pub const UNITREE_OUIS: [&str; 4] = ["c8:fe:0f", "78:22:88", "94:ba:06", "fc:23:cd"];
const GO2_SIGNALING_PORTS: [u16; 2] = [9991, 8081];
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Lowercase, zero-padded, colon-separated ("2:9D:48:e4:91:35" → "02:9d:48:e4:91:35"); None unless six hex bytes.
pub fn norm_mac(mac: &str) -> Option<String> {
    let parts: Vec<&str> = mac.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut bytes = Vec::with_capacity(6);
    for part in parts {
        if part.is_empty() || part.len() > 2 {
            return None;
        }
        bytes.push(u8::from_str_radix(part, 16).ok()?);
    }
    if bytes.iter().all(|b| *b == 0) {
        return None; // /proc/net/arp's incomplete entry
    }
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"))
}

/// True when a (normalized) MAC has a Unitree prefix: how a dog with no other identity is spotted.
pub fn is_unitree_oui(mac: &str) -> bool {
    UNITREE_OUIS.iter().any(|oui| mac.starts_with(oui))
}

/// A Go2's Wi-Fi MAC from its Bluetooth MAC: the last byte minus one (BLE c8:fe:0f:f7:f8:bc → Wi-Fi …:bb).
pub fn ble_to_wifi_mac(ble_mac: &str) -> Option<String> {
    let mac = norm_mac(ble_mac)?;
    let (head, last) = mac.rsplit_once(':')?;
    let last = u8::from_str_radix(last, 16).ok()?.wrapping_sub(1);
    Some(format!("{head}:{last:02x}"))
}

/// macOS `arp -an`: `? (10.10.128.1) at a8:9c:6c:9c:56:f5 on en0 ifscope [ethernet]`; `(incomplete)` rows are skipped.
pub fn parse_arp_an(text: &str) -> Vec<(Ipv4Addr, String)> {
    text.lines()
        .filter_map(|line| {
            let ip = line.split_once('(')?.1.split_once(')')?.0.parse().ok()?;
            let mac = norm_mac(line.split_once(" at ")?.1.split_whitespace().next()?)?;
            Some((ip, mac))
        })
        .collect()
}

/// Linux `ip neigh`: `10.0.0.5 dev wlan0 lladdr aa:bb:cc:dd:ee:ff REACHABLE`; FAILED / INCOMPLETE rows have no lladdr.
pub fn parse_ip_neigh(text: &str) -> Vec<(Ipv4Addr, String)> {
    text.lines()
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.last() == Some(&"FAILED") {
                return None;
            }
            let ip = tokens.first()?.parse().ok()?;
            let at = tokens.iter().position(|t| *t == "lladdr")?;
            Some((ip, norm_mac(tokens.get(at + 1)?)?))
        })
        .collect()
}

/// Linux `/proc/net/arp`: `IP address  HW type  Flags  HW address  Mask  Device`; flags 0x0 (incomplete) skipped.
pub fn parse_proc_net_arp(text: &str) -> Vec<(Ipv4Addr, String)> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let flags = u32::from_str_radix(tokens.get(2)?.trim_start_matches("0x"), 16).ok()?;
            if flags & 0x2 == 0 {
                return None; // ATF_COM: not resolved
            }
            Some((tokens.first()?.parse().ok()?, norm_mac(tokens.get(3)?)?))
        })
        .collect()
}

/// Every resolved (ip, mac) in this computer's neighbour table. Linux reads /proc/net/arp (no subprocess, works
/// without iproute2); macOS has no such file, so it asks `/usr/sbin/arp -an`.
pub async fn read_neighbors() -> Vec<(Ipv4Addr, String)> {
    #[cfg(target_os = "linux")]
    {
        match tokio::fs::read_to_string("/proc/net/arp").await {
            Ok(text) => parse_proc_net_arp(&text),
            Err(_) => parse_ip_neigh(&run(&["ip", "-4", "neigh"]).await),
        }
    }
    #[cfg(target_os = "macos")]
    {
        parse_arp_an(&run(&["/usr/sbin/arp", "-an"]).await)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

#[allow(dead_code)]
pub(crate) async fn run(argv: &[&str]) -> String {
    match tokio::process::Command::new(argv[0]).args(&argv[1..]).output().await {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// Is a Go2 signaling port answering there? Opens and closes a TCP connection; sends nothing, so it can't disturb a
/// session.
pub async fn go2_alive(ip: &str) -> bool {
    for port in GO2_SIGNALING_PORTS {
        if let Ok(Ok(stream)) = timeout(PROBE_TIMEOUT, TcpStream::connect((ip, port))).await {
            drop(stream);
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> Ipv4Addr {
        text.parse().unwrap()
    }

    #[test]
    fn mac_offset_join() {
        assert_eq!(ble_to_wifi_mac("C8:FE:0F:F7:F8:BC").as_deref(), Some("c8:fe:0f:f7:f8:bb"));
        assert_eq!(ble_to_wifi_mac("c8:fe:f:1:2:0").as_deref(), Some("c8:fe:0f:01:02:ff")); // wraps, like the script's & 0xff
        assert_eq!(ble_to_wifi_mac("00:00:00:00:00:00"), None); // CoreBluetooth's "no address"
        assert_eq!(ble_to_wifi_mac("5E1B2C3D-0000-4000-8000-000000000000"), None); // a macOS peripheral UUID
    }

    #[test]
    fn oui_filter() {
        for mac in ["c8:fe:0f:f7:f8:bb", "78:22:88:00:00:01", "94:ba:06:aa:bb:cc", "fc:23:cd:12:34:56"] {
            assert!(is_unitree_oui(mac), "{mac}");
        }
        assert!(!is_unitree_oui("a8:9c:6c:9c:56:f5"));
        assert!(!is_unitree_oui("c8:fe:0e:f7:f8:bb"));
        assert!(is_unitree_oui(&norm_mac("C8:FE:F:1:2:3").unwrap())); // macOS drops leading zeros
    }

    #[test]
    fn arp_an_parser() {
        let text = "\
? (10.10.128.1) at a8:9c:6c:9c:56:f5 on en0 ifscope [ethernet]
? (10.10.133.235) at 2:9d:48:e4:91:35 on en0 ifscope [ethernet]
? (10.10.200.7) at (incomplete) on en0 ifscope [ethernet]
? (10.10.220.15) at c8:fe:f:f7:f8:bb on en0 ifscope [ethernet]
mdns.mcast.net (224.0.0.251) at 1:0:5e:0:0:fb on en0 ifscope permanent [ethernet]
";
        assert_eq!(
            parse_arp_an(text),
            vec![
                (ip("10.10.128.1"), "a8:9c:6c:9c:56:f5".to_string()),
                (ip("10.10.133.235"), "02:9d:48:e4:91:35".to_string()),
                (ip("10.10.220.15"), "c8:fe:0f:f7:f8:bb".to_string()),
                (ip("224.0.0.251"), "01:00:5e:00:00:fb".to_string()),
            ]
        );
    }

    #[test]
    fn ip_neigh_parser() {
        let text = "\
10.10.220.15 dev wlp0s20f3 lladdr c8:fe:0f:f7:f8:bb REACHABLE
10.10.128.1 dev wlp0s20f3 lladdr a8:9c:6c:9c:56:f5 STALE
10.10.200.7 dev wlp0s20f3 FAILED
10.10.200.8 dev wlp0s20f3 INCOMPLETE
10.10.200.9 dev wlp0s20f3 lladdr 00:11:22:33:44:55 FAILED
fe80::1 dev wlp0s20f3 lladdr a8:9c:6c:9c:56:f5 router STALE
";
        assert_eq!(
            parse_ip_neigh(text),
            vec![(ip("10.10.220.15"), "c8:fe:0f:f7:f8:bb".to_string()), (ip("10.10.128.1"), "a8:9c:6c:9c:56:f5".to_string())]
        );
    }

    #[test]
    fn proc_net_arp_parser() {
        let text = "\
IP address       HW type     Flags       HW address            Mask     Device
10.10.220.15     0x1         0x2         c8:fe:0f:f7:f8:bb     *        wlp0s20f3
10.10.200.7      0x1         0x0         00:00:00:00:00:00     *        wlp0s20f3
10.10.128.1      0x1         0x2         A8:9C:6C:9C:56:F5     *        wlp0s20f3
";
        assert_eq!(
            parse_proc_net_arp(text),
            vec![(ip("10.10.220.15"), "c8:fe:0f:f7:f8:bb".to_string()), (ip("10.10.128.1"), "a8:9c:6c:9c:56:f5".to_string())]
        );
    }
}

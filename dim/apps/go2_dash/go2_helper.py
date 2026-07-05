# Copyright 2026 Dimensional Inc.
# Licensed under the Apache License, Version 2.0.
"""go2_dash helper — JSON-over-stdio bridge to dimos' own go2tool internals.

Runs inside the dimos venv (the dashboard backend shells into it). Reuses the
exact discovery + provisioning code behind `dimos go2tool`:
  - discover_ble / discover_lan  → stream nearby Go2s (BLE name+serial, LAN ip)
  - provision_wifi               → push wifi creds to a Go2 over Bluetooth

Protocol (newline-delimited JSON both ways):
  stdin  commands : {"type":"scan","timeout":7}
                    {"type":"connect","mac":..,"ssid":..,"password":..,"country":"US"}
  stdout events   : {"type":"ready"}
                    {"type":"scan_start"} / {"type":"scan_done","count":n}
                    {"type":"device", serial, name, ble_mac, ip, lan_mac}
                    {"type":"progress","msg":..}
                    {"type":"connect_result","ok":bool,"serial":..,"error":..}
                    {"type":"warn","msg":..}
"""

from __future__ import annotations

import asyncio
import json
import socket
import struct
import sys
import time


def emit(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


# macOS LAN discovery. dimos' discover_lan sends the Go2 probe to the multicast
# group 231.1.1.1 — but on macOS that group routes to lo0 (and the VPN owns the
# default route), so the send dies with "No route to host" and nothing is found.
# Directed/limited broadcast goes out the real interface fine, and the Go2 answers
# it, so on darwin we probe via broadcast (pinned to each NIC with IP_BOUND_IF)
# instead. (Linux multicast works, so we keep dimos' path there.)
IP_BOUND_IF = 25  # <netinet/in.h> on macOS


def _wifi_ifaces():
    """(name, ipv4, broadcast) for real, non-tunnel IPv4 interfaces."""
    import psutil

    skip = ("lo", "tailscale", "wg", "tun", "utun", "docker", "br-", "veth", "awdl", "llw", "bridge", "Meta")
    for name, addrs in psutil.net_if_addrs().items():
        if name.startswith(skip):
            continue
        for addr in addrs:
            if addr.family == socket.AF_INET and not addr.address.startswith("127."):
                yield name, addr.address, getattr(addr, "broadcast", None)
                break


def _probe_broadcast(name, ipv4, broadcast, group, query_port, reply_port, payload, timeout):
    """Send the probe out one NIC via broadcast (+ multicast best-effort); collect {serial: ip}."""
    found: dict[str, str] = {}
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    try:
        sock.bind(("", reply_port))
    except OSError:
        sock.close()
        return found
    try:
        sock.setsockopt(socket.IPPROTO_IP, IP_BOUND_IF, struct.pack("I", socket.if_nametoindex(name)))
    except OSError:
        pass
    try:
        sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(ipv4))
        sock.setsockopt(
            socket.IPPROTO_IP,
            socket.IP_ADD_MEMBERSHIP,
            struct.pack("4s4s", socket.inet_aton(group), socket.inet_aton(ipv4)),
        )
    except OSError:
        pass
    sent = False
    for target in filter(None, [broadcast, group]):
        try:
            sock.sendto(payload, (target, query_port))
            sent = True
        except OSError:
            pass
    if not sent:
        sock.close()
        return found
    sock.settimeout(timeout)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            data, addr = sock.recvfrom(2048)
        except (TimeoutError, OSError):
            break
        try:
            msg = json.loads(data.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        serial = msg.get("sn")
        if serial:
            found[serial] = msg.get("ip", addr[0])
    sock.close()
    return found


async def discover_lan_macos(tick: float, timeout: float):
    """Stream Go2 LAN discoveries on macOS via broadcast. Mirrors dimos' discover_lan."""
    from dimos.robot.unitree.go2.cli.landiscovery import (
        MULTICAST_GROUP,
        QUERY_PAYLOAD,
        QUERY_PORT,
        REPLY_PORT,
        Go2Device,
    )

    loop = asyncio.get_running_loop()
    while True:
        ifaces = list(_wifi_ifaces())

        def run():
            out: dict[str, Go2Device] = {}
            for name, ipv4, broadcast in ifaces:
                hits = _probe_broadcast(
                    name, ipv4, broadcast, MULTICAST_GROUP, QUERY_PORT, REPLY_PORT, QUERY_PAYLOAD, timeout
                )
                for serial, ip in hits.items():
                    out.setdefault(serial, Go2Device(serial=serial, ip=ip, iface=name))
            return out

        for device in (await loop.run_in_executor(None, run)).values():
            yield device
        await asyncio.sleep(max(0.0, tick - timeout))


# ── ARP fallback (Layer 2; works under a VPN's L3 kill-switch firewall) ───────
# When the multicast LAN probe is blocked (e.g. a full-tunnel VPN), the OS ARP
# cache still knows the IP↔MAC of LAN devices it has overheard. We match Go2s by
# their Wi-Fi MAC OUI and surface their IPs — flagged UNVERIFIED, since ARP gives
# no serial to tie an IP back to a specific dog. No sudo needed.
GO2_OUIS = {"94:ba:06"}  # observed Go2 Wi-Fi OUI(s); extend as more are seen


def _norm_mac(mac: str) -> str:
    try:
        return ":".join(f"{int(part, 16):02x}" for part in mac.split(":"))
    except ValueError:
        return mac.lower()


def _arp_sweep() -> None:
    # Nudge the OS ARP cache: touch every host on each Wi-Fi /24. The L3 sends fail
    # under a VPN firewall, but the kernel still ARPs (L2) for live hosts, so they
    # show up in the table a moment later. Entries otherwise age out.
    import ipaddress

    for name, ipv4, _broadcast in _wifi_ifaces():
        try:
            net = ipaddress.ip_network(ipv4 + "/24", strict=False)
        except ValueError:
            continue
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            sock.setsockopt(socket.IPPROTO_IP, IP_BOUND_IF, struct.pack("I", socket.if_nametoindex(name)))
        except OSError:
            pass
        sock.setblocking(False)
        for host in net.hosts():
            try:
                sock.sendto(b"", (str(host), 9))
            except OSError:
                pass
        sock.close()


def arp_scan() -> "list[tuple[str, str]]":
    import re
    import subprocess

    text = ""
    try:
        if sys.platform == "darwin":
            text = subprocess.run(["arp", "-an"], capture_output=True, text=True, timeout=4).stdout
        else:
            try:
                with open("/proc/net/arp") as fh:
                    text = fh.read()
            except OSError:
                text = subprocess.run(["ip", "neigh"], capture_output=True, text=True, timeout=4).stdout
    except (OSError, subprocess.SubprocessError):
        return []
    hits, seen = [], set()
    for line in text.splitlines():
        m = re.search(r"(\d{1,3}(?:\.\d{1,3}){3}).*?(([0-9a-fA-F]{1,2}:){5}[0-9a-fA-F]{1,2})", line)
        if not m:
            continue
        ip, mac = m.group(1), _norm_mac(m.group(2))
        if mac[:8] in GO2_OUIS and ip not in seen:
            seen.add(ip)
            hits.append((ip, mac))
    return hits


# ARP only knows an IP↔MAC the kernel once overheard — the entry can be stale (dog
# powered off / left the network) or the IP may have been reused by some other
# device. Before surfacing an unverified ARP card we TCP-connect to a Go2 LAN
# signaling port: it proves the host is alive AND actually a Go2, and because we
# never send an SDP offer it can't disturb an existing control session the way a
# real WebRTC connect (which the robot answers with "reject"/RobotBusyError) would.
GO2_SIGNALING_PORTS = (9991, 8081)
PROBE_TIMEOUT_SECONDS = 1.5


async def _probe_port(ip: str, port: int) -> bool:
    try:
        _reader, writer = await asyncio.wait_for(
            asyncio.open_connection(ip, port), timeout=PROBE_TIMEOUT_SECONDS
        )
    except (OSError, asyncio.TimeoutError):
        return False
    writer.close()
    try:
        await writer.wait_closed()
    except (OSError, asyncio.TimeoutError):
        pass
    return True


async def _go2_alive(ip: str) -> bool:
    """Non-disruptive liveness + identity check: is a Go2 signaling port answering?"""
    return any(await asyncio.gather(*(_probe_port(ip, port) for port in GO2_SIGNALING_PORTS)))


async def stdin_lines():
    """Async iterator over stdin lines (POSIX read-pipe, non-blocking)."""
    loop = asyncio.get_event_loop()
    reader = asyncio.StreamReader()
    await loop.connect_read_pipe(lambda: asyncio.StreamReaderProtocol(reader), sys.stdin)
    while True:
        raw = await reader.readline()
        if not raw:
            return
        yield raw.decode(errors="replace").strip()


async def do_scan(timeout: float) -> None:
    """Run BLE + LAN discovery concurrently for `timeout`s, merging by serial."""
    from dimos.robot.unitree.go2.cli.ble import discover_ble
    from dimos.robot.unitree.go2.cli.landiscovery import discover_lan

    emit({"type": "scan_start"})
    merged: dict[str, dict] = {}
    mac_key: dict[str, str] = {}  # ble_mac → the key currently holding that robot
    ip_key: dict[str, str] = {}   # ip → key (for an ip confirmed by BLE/LAN)

    def _blank(serial):
        return {"serial": serial, "name": None, "ble_mac": None, "ip": None, "lan_mac": None, "arp_only": False}

    def upsert(serial=None, name=None, ble_mac=None, ip=None, lan_mac=None) -> None:
        # Key by serial when known (BLE + LAN rows for one robot collapse into one
        # card). Before a serial is recovered we key by BLE address; once the
        # serial shows up we MUST migrate that address-only card onto the serial
        # key — otherwise the same dog appears twice (one with an id, one without).
        if serial:
            key = "s:" + serial
        elif ble_mac and mac_key.get(ble_mac, "").startswith("s:"):
            key = mac_key[ble_mac]  # already promoted to a serial card; reuse it
        else:
            key = "b:" + (ble_mac or "")

        # Fold a prior address-only card into the serial card and drop the stale one.
        if serial and ble_mac:
            old_key = mac_key.get(ble_mac)
            if old_key and old_key != key and old_key in merged:
                old = merged.pop(old_key)
                base = merged.setdefault(key, _blank(serial))
                for fld in ("name", "ble_mac", "ip", "lan_mac"):
                    if not base.get(fld) and old.get(fld):
                        base[fld] = old[fld]
                # the dropped card's frontend key was its ble_mac (it had no serial)
                emit({"type": "drop", "key": old_key.split(":", 1)[1]})

        rec = merged.setdefault(key, _blank(serial))
        if serial:
            rec["serial"] = serial
        if name:
            rec["name"] = name
        if ble_mac:
            rec["ble_mac"] = ble_mac
        if ip:
            rec["ip"] = ip
            ip_key[ip] = key
            arp_k = "a:" + ip
            if arp_k != key and arp_k in merged:  # a real sighting supersedes the ARP guess
                merged.pop(arp_k)
                emit({"type": "drop", "key": ip})
        if lan_mac:
            rec["lan_mac"] = lan_mac
        if ble_mac:
            mac_key[ble_mac] = key
        emit({"type": "device", **rec})

    def arp_upsert(ip, mac) -> None:
        if ip in ip_key:  # already known via BLE/LAN with a real identity — just attach the mac
            rec = merged.get(ip_key[ip])
            if rec and not rec.get("lan_mac"):
                rec["lan_mac"] = mac
                emit({"type": "device", **rec})
            return
        key = "a:" + ip
        existing = merged.get(key)
        if existing and existing.get("lan_mac") == mac:
            return  # unchanged — don't spam the panel
        merged[key] = {"serial": None, "name": None, "ble_mac": None, "ip": ip, "lan_mac": mac, "arp_only": True}
        emit({"type": "device", **merged[key]})

    async def ble_task() -> None:
        try:
            async for device in discover_ble():
                upsert(serial=device.serial, name=device.name, ble_mac=device.address)
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001 — surface, don't crash the process
            emit({"type": "warn", "msg": f"ble: {exc}"})

    async def lan_task() -> None:
        try:
            stream = discover_lan_macos(tick=2.0, timeout=1.5) if sys.platform == "darwin" else discover_lan(tick=2.0)
            async for device in stream:
                upsert(serial=device.serial, ip=device.ip, lan_mac=getattr(device, "mac", None))
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001
            emit({"type": "warn", "msg": f"lan: {exc}"})

    def arp_drop(ip) -> None:
        # A previously-shown unverified card stopped answering — remove it.
        key = "a:" + ip
        if key in merged:
            merged.pop(key)
            emit({"type": "drop", "key": ip})

    async def arp_task() -> None:
        loop = asyncio.get_running_loop()
        try:
            await loop.run_in_executor(None, _arp_sweep)  # repopulate aged-out entries
            await asyncio.sleep(1.0)
            while True:
                hits = await loop.run_in_executor(None, arp_scan)
                # An IP already confirmed by BLE/LAN needs no probe — just attach its mac.
                # For the rest (unverified), probe concurrently and only show responders.
                unverified = [(ip, mac) for ip, mac in hits if ip not in ip_key]
                alive = dict(
                    zip(
                        (ip for ip, _ in unverified),
                        await asyncio.gather(*(_go2_alive(ip) for ip, _ in unverified)),
                    )
                )
                for ip, mac in hits:
                    if ip in ip_key:
                        arp_upsert(ip, mac)
                    elif alive.get(ip):
                        arp_upsert(ip, mac)
                    else:
                        arp_drop(ip)
                await asyncio.sleep(2.0)
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001
            emit({"type": "warn", "msg": f"arp: {exc}"})

    tasks = [
        asyncio.create_task(ble_task()),
        asyncio.create_task(lan_task()),
        asyncio.create_task(arp_task()),
    ]
    try:
        await asyncio.sleep(timeout)
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
    emit({"type": "scan_done", "count": len(merged)})


async def do_connect(cmd: dict) -> None:
    from dimos.robot.unitree.go2.cli.ble import provision_wifi, retry

    mac = cmd.get("mac")
    ssid = cmd.get("ssid")
    password = cmd.get("password", "")
    country = cmd.get("country", "US")
    if not mac or not ssid:
        emit({"type": "connect_result", "ok": False, "error": "mac and ssid are required"})
        return

    emit({"type": "progress", "msg": f"Connecting {mac} → “{ssid}” …"})

    def on_error(attempt: int, exc: BaseException) -> None:
        emit({"type": "progress", "msg": f"attempt {attempt} failed: {exc}"})

    try:
        serial = await retry(
            lambda: provision_wifi(
                mac,
                ssid,
                password,
                country,
                on_progress=lambda message: emit({"type": "progress", "msg": message}),
            ),
            attempts=int(cmd.get("retries", 3)),
            on_error=on_error,
        )
        emit({"type": "connect_result", "ok": True, "serial": serial})
    except Exception as exc:  # noqa: BLE001
        emit({"type": "connect_result", "ok": False, "error": str(exc)})


async def main() -> None:
    emit({"type": "ready"})
    scan_task: asyncio.Task | None = None

    async for line in stdin_lines():
        if not line:
            continue
        try:
            cmd = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = cmd.get("type")
        if kind == "scan":
            if scan_task and not scan_task.done():
                continue  # a scan is already running — ignore re-trigger
            scan_task = asyncio.create_task(do_scan(float(cmd.get("timeout", 7))))
        elif kind == "connect":
            asyncio.create_task(do_connect(cmd))


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except (KeyboardInterrupt, asyncio.CancelledError):
        pass

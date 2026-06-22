# Copyright 2026 Dimensional Inc.
# Licensed under the Apache License, Version 2.0.
"""go2_dash sidecar — JSON-over-stdio bridge to dimos' own go2tool internals.

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
import sys


def emit(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


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

    def upsert(serial=None, name=None, ble_mac=None, ip=None, lan_mac=None) -> None:
        # Key by serial when known (BLE + LAN rows for one robot collapse into
        # one card); fall back to BLE address before a serial is recovered.
        key = ("s:" + serial) if serial else ("b:" + (ble_mac or ""))
        rec = merged.setdefault(
            key,
            {"serial": serial, "name": None, "ble_mac": None, "ip": None, "lan_mac": None},
        )
        if serial:
            rec["serial"] = serial
        if name:
            rec["name"] = name
        if ble_mac:
            rec["ble_mac"] = ble_mac
        if ip:
            rec["ip"] = ip
        if lan_mac:
            rec["lan_mac"] = lan_mac
        emit({"type": "device", **rec})

    async def ble_task() -> None:
        try:
            async for device in discover_ble():
                upsert(serial=device.serial, name=device.name, ble_mac=device.address)
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001 — surface, don't crash the sidecar
            emit({"type": "warn", "msg": f"ble: {exc}"})

    async def lan_task() -> None:
        try:
            async for device in discover_lan(tick=2.0):
                upsert(serial=device.serial, ip=device.ip, lan_mac=device.mac)
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001
            emit({"type": "warn", "msg": f"lan: {exc}"})

    tasks = [asyncio.create_task(ble_task()), asyncio.create_task(lan_task())]
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

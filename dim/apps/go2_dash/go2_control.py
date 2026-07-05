# Copyright 2026 Dimensional Inc.
# Licensed under the Apache License, Version 2.0.
"""go2_control — the Python communicator for live control of ONE Go2 over WebRTC.

Runs inside the dimos venv (the dashboard backend shells into it, one process per
open dog). Connects with dimos' UnitreeWebRTCConnection — the Go2's WebRTC link
carries the same Unitree sport/DDS requests the robot's own app uses — and bridges
three things to the browser dashboard over newline-JSON stdio:

  - movement : a velocity vector (forward / strafe / turn), sent as a Twist
  - actions  : sport-mode commands (stand, sit, hello, …) by name
  - camera   : the live video track, downscaled and re-encoded as JPEG frames

Protocol (newline-delimited JSON):
  argv            : go2_control.py <ip> [aes_128_key]
  stdin commands  : {"type":"move","forward":..,"strafe":..,"turn":..}   # each -1..1
                    {"type":"action","name":"stand"}                      # sit/hello/…
                    {"type":"stop"}
  stdout events   : {"type":"connecting"} / {"type":"ready"} / {"type":"error","error":..}
                    {"type":"video"}                                      # first frame seen
                    {"type":"frame","data":"<base64 jpeg>","w":..,"h":..}
                    {"type":"reconnecting"}                                # link stalled, retrying
                    {"type":"action_result","name":..,"ok":bool}

If the video link stalls (the dog drops off Wi-Fi or the WebRTC track dies), a
watchdog tears the connection down and rebuilds it in place — emitting
{"type":"reconnecting"} then {"type":"ready"} again — so the process survives a
transient disconnect instead of exiting.

stdout carries ONLY this protocol — any library chatter is redirected to stderr so
it can never corrupt the stream.
"""

from __future__ import annotations

import base64
import io
import json
import os
import sys
import threading
import time
from typing import Any

# Real stdout for the protocol; everything else (dimos/webrtc logging, stray
# prints) goes to stderr so it can't interleave with our JSON.
_protocol = os.fdopen(os.dup(1), "w", buffering=1)
sys.stdout = sys.stderr

_emit_lock = threading.Lock()


def emit(obj: dict[str, Any]) -> None:
    with _emit_lock:
        _protocol.write(json.dumps(obj) + "\n")
        _protocol.flush()


# Velocity envelope: a normalized -1..1 axis from the UI maps to these maxima.
MAX_FORWARD = 0.6  # m/s
MAX_LATERAL = 0.4  # m/s
MAX_YAW = 0.8  # rad/s
# Camera relay: keep the app-bus light — cap width and frame rate, lossy JPEG.
FRAME_MAX_WIDTH = 640
FRAME_MIN_INTERVAL = 1.0 / 12  # s between emitted frames
JPEG_QUALITY = 50
# Reconnect watchdog: once frames have started, this long without ANY frame means
# the link stalled — tear the connection down and rebuild it.
STALL_SECONDS = 5.0
WATCHDOG_INTERVAL = 1.0  # s between liveness checks
RECONNECT_BACKOFF = 2.0  # s between failed rebuild attempts
# After standing up, wait this long before BalanceStand so joystick control latches
# (BalanceStand issued while the dog is still rising doesn't stick). Matches dimos' startup.
STAND_SETTLE_SECONDS = 3.0


class Go2Controller:
    def __init__(self, ip: str, aes_128_key: str | None) -> None:
        self.ip = ip
        self.aes_128_key = aes_128_key
        self.connection = self._build_connection()
        # WIRELESS_CONTROLLER (the move channel) only listens once the robot is in
        # BalanceStand; track whether we've enabled it so the first drive works.
        self._balanced = False
        self._video_subscription = None
        self._last_frame_at = 0.0   # throttles emitted frames
        self._last_rx = 0.0         # last frame actually received (liveness)
        self._seen_frame = False
        self._conn_lock = threading.Lock()
        self._closing = False
        self._reconnecting = False
        self._watchdog_thread: threading.Thread | None = None

    def _build_connection(self) -> Any:
        from dimos.robot.unitree.connection import UnitreeWebRTCConnection

        return UnitreeWebRTCConnection(ip=self.ip, aes_128_key=self.aes_128_key)

    def start_camera(self) -> None:
        from PIL import Image as PILImage

        def on_frame(image: Any) -> None:
            now = time.monotonic()
            self._last_rx = now  # liveness: every received frame, before throttling
            if now - self._last_frame_at < FRAME_MIN_INTERVAL:
                return
            self._last_frame_at = now
            try:
                frame = PILImage.fromarray(image.data)  # RGB uint8 (H, W, 3)
                if frame.width > FRAME_MAX_WIDTH:
                    height = round(frame.height * FRAME_MAX_WIDTH / frame.width)
                    frame = frame.resize((FRAME_MAX_WIDTH, height))
                buffer = io.BytesIO()
                frame.save(buffer, format="JPEG", quality=JPEG_QUALITY)
            except Exception as exc:  # noqa: BLE001 — a bad frame shouldn't kill the feed
                emit({"type": "warn", "msg": f"frame: {exc}"})
                return
            if not self._seen_frame:
                self._seen_frame = True
                emit({"type": "video"})
            emit(
                {
                    "type": "frame",
                    "data": base64.b64encode(buffer.getvalue()).decode("ascii"),
                    "w": frame.width,
                    "h": frame.height,
                }
            )

        if self._video_subscription is not None:
            try:
                self._video_subscription.dispose()
            except Exception:  # noqa: BLE001
                pass
        self._video_subscription = self.connection.video_stream().subscribe(
            on_next=on_frame,
            on_error=lambda exc: emit({"type": "warn", "msg": f"video: {exc}"}),
        )

    def start_watchdog(self) -> None:
        if self._watchdog_thread is not None:
            return
        self._last_rx = time.monotonic()
        self._watchdog_thread = threading.Thread(target=self._watchdog, daemon=True)
        self._watchdog_thread.start()

    def _watchdog(self) -> None:
        while not self._closing:
            time.sleep(WATCHDOG_INTERVAL)
            if self._closing or self._reconnecting:
                continue
            # only judge liveness once frames have actually started flowing
            if self._seen_frame and (time.monotonic() - self._last_rx) > STALL_SECONDS:
                self._reconnect()

    def _reconnect(self) -> None:
        self._reconnecting = True
        emit({"type": "reconnecting"})
        while not self._closing:
            try:
                with self._conn_lock:
                    old = self.connection
                    self.connection = self._build_connection()
                    self._balanced = False
                self._teardown(old)
                self.start_camera()
                self._last_rx = time.monotonic()
                emit({"type": "ready"})
                self._reconnecting = False
                return
            except Exception as exc:  # noqa: BLE001 — keep retrying until closed
                emit({"type": "warn", "msg": f"reconnect: {exc}"})
                time.sleep(RECONNECT_BACKOFF)
        self._reconnecting = False

    @staticmethod
    def _teardown(connection: Any) -> None:
        try:
            connection.stop()
        except Exception:  # noqa: BLE001
            pass

    def move(self, forward: float, strafe: float, turn: float) -> None:
        from dimos.msgs.geometry_msgs.Twist import Twist

        if self._reconnecting:
            return  # link is down; drop the command instead of erroring
        twist = Twist(
            (forward * MAX_FORWARD, strafe * MAX_LATERAL, 0.0),
            (0.0, 0.0, turn * MAX_YAW),
        )
        with self._conn_lock:
            self.connection.move(twist)

    def action(self, name: str) -> bool:
        from unitree_webrtc_connect.constants import RTC_TOPIC, SPORT_CMD

        if self._reconnecting:
            return False
        with self._conn_lock:
            # "stand" just gets the dog upright and holding posture — not drivable.
            if name == "stand":
                self.connection.standup()
                self._balanced = False
                return True
            # "walk" enters the drivable BalanceStand mode. Stand up first and let the
            # robot settle: BalanceStand issued mid-rise doesn't latch joystick control,
            # so the drive keys would do nothing (this mirrors dimos' own GO2 startup).
            if name == "walk":
                self.connection.standup()
                time.sleep(STAND_SETTLE_SECONDS)
                self.connection.balance_stand()
                self._balanced = True
                return True
            api_id = SPORT_CMD.get(name)
            if api_id is None:
                return False
            ok = bool(
                self.connection.publish_request(RTC_TOPIC["SPORT_MOD"], {"api_id": api_id})
            )
        if name == "BalanceStand":
            self._balanced = True
        return ok

    def stop(self) -> None:
        try:
            self.move(0.0, 0.0, 0.0)
        except Exception:  # noqa: BLE001
            pass

    def close(self) -> None:
        self._closing = True
        if self._video_subscription is not None:
            try:
                self._video_subscription.dispose()
            except Exception:  # noqa: BLE001
                pass
        try:
            self.connection.stop()
        except Exception:  # noqa: BLE001
            pass


def main() -> None:
    if len(sys.argv) < 2 or not sys.argv[1]:
        emit({"type": "error", "error": "usage: go2_control.py <ip> [aes_128_key]"})
        return
    ip = sys.argv[1]
    aes_128_key = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2] else None

    emit({"type": "connecting"})
    try:
        controller = Go2Controller(ip, aes_128_key)
    except Exception as exc:  # noqa: BLE001 — surface the reason, don't crash silently
        emit({"type": "error", "error": str(exc)})
        return
    emit({"type": "ready"})
    controller.start_camera()
    controller.start_watchdog()

    try:
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                command = json.loads(line)
            except json.JSONDecodeError:
                continue
            kind = command.get("type")
            if kind == "move":
                controller.move(
                    float(command.get("forward", 0.0)),
                    float(command.get("strafe", 0.0)),
                    float(command.get("turn", 0.0)),
                )
            elif kind == "stop":
                controller.stop()
            elif kind == "action":
                name = command.get("name", "")
                ok = controller.action(name)
                emit({"type": "action_result", "name": name, "ok": ok})
    finally:
        controller.close()


if __name__ == "__main__":
    try:
        main()
    except (KeyboardInterrupt, EOFError):
        pass

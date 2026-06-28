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
                    {"type":"action_result","name":..,"ok":bool}

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


class Go2Controller:
    def __init__(self, ip: str, aes_128_key: str | None) -> None:
        from dimos.robot.unitree.connection import UnitreeWebRTCConnection

        self.connection = UnitreeWebRTCConnection(ip=ip, aes_128_key=aes_128_key)
        # WIRELESS_CONTROLLER (the move channel) only listens once the robot is in
        # BalanceStand; track whether we've enabled it so the first drive works.
        self._balanced = False
        self._video_subscription = None
        self._last_frame_at = 0.0
        self._seen_frame = False

    def start_camera(self) -> None:
        from PIL import Image as PILImage

        def on_frame(image: Any) -> None:
            now = time.monotonic()
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

        self._video_subscription = self.connection.video_stream().subscribe(
            on_next=on_frame,
            on_error=lambda exc: emit({"type": "warn", "msg": f"video: {exc}"}),
        )

    def move(self, forward: float, strafe: float, turn: float) -> None:
        from dimos.msgs.geometry_msgs.Twist import Twist

        twist = Twist(
            (forward * MAX_FORWARD, strafe * MAX_LATERAL, 0.0),
            (0.0, 0.0, turn * MAX_YAW),
        )
        self.connection.move(twist)

    def action(self, name: str) -> bool:
        from unitree_webrtc_connect.constants import RTC_TOPIC, SPORT_CMD

        # "stand" is a convenience: get up AND enter BalanceStand so driving works.
        if name == "stand":
            self.connection.standup()
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

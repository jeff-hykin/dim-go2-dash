# Go2 Ctrl (dim-go2-dash)

A [dimOS Desktop](https://github.com/dimensionalOS/dimos-desktop) app for **Unitree Go2** robot dogs:

- **Discover** nearby Go2s over Bluetooth (BLE) and on the local network (LAN + ARP). A robot seen both ways is one
  card with its name, serial and IP.
- **Connect to Wi-Fi**: send a Go2 Wi-Fi credentials over Bluetooth so it joins your network.
- **Drive** one live: camera, stand / sit / jump / dance / …, and keyboard or d-pad walking, over WebRTC.

| Discover | Connect to Wi-Fi |
| --- | --- |
| ![Discovering nearby Go2s](docs/discover.png) | ![Sending Wi-Fi credentials to a Go2](docs/connect-wifi.png) |

## Every action is an endpoint

The backend does everything (scans, provisioning, the robot's WebRTC session); the page and Desktop's agent call the
same HTTP endpoints, listed in `dimos.yaml` (`agent:`) and served as `agent.json`. A few:

| | |
| --- | --- |
| `POST api/scan` | scan (Bluetooth + LAN), answers with the robots found |
| `POST api/robots/{key}/wifi` | put a robot on Wi-Fi over Bluetooth |
| `POST api/drive/connect` | open the live session to a robot |
| `POST api/drive/jump`, `…/stand`, `…/sit`, `…/dance1`, … | robot commands |
| `POST api/drive/move` | walk at a velocity for a while |
| `GET api/state` | everything at once |

Robot-moving and Wi-Fi-changing endpoints take `dryRun: true` (check and say what would be sent, send nothing);
`POST api/drive/connect` with `dryRun: true` opens a simulated session. `GO2_DASH_MOCK=1` simulates every robot,
Bluetooth and cloud interaction.

## Layout

```
backend/    Rust (Bluetooth via CoreBluetooth/BlueZ, WebRTC to the robot), axum server → dimos-app-server
  src/routes.rs     every endpoint        src/drive.rs      live session + commands
  src/app.rs        state, scan, Wi-Fi    src/robot_rtc.rs  the Go2 WebRTC handshake
  src/ble.rs …      discovery/provisioning (BLE, LAN, ARP)   src/video.rs  camera → pages
  tests/routes.rs   each route, happy path + error (mock app)
frontend/   TypeScript + Vite + React
flake.nix   nix build .#dimosApp → bin/dimos-app-server
```

Develop: `cd backend && GO2_DASH_MOCK=1 cargo run -- --port 8787 --frontend ../frontend/dist` and
`cd frontend && npm run dev`. Checks: `cargo test`, `npm run typecheck`, `deno task check-endpoints`
(`--write` regenerates `dimos.yaml`'s `agent:`).

## Install

```sh
dimos-desktop install https://github.com/jeff-hykin/dim-go2-dash
```

Licensed under Apache-2.0.

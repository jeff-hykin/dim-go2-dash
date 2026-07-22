// go2_dash — backend half (runs in the Deno dashboard process).
//
// Discovery + wifi provisioning for Unitree Go2 robots. BLE scanning and the
// wifi handshake both need native Bluetooth (CoreBluetooth / BlueZ), so this
// can't be pure Deno — we `nix run` the standalone Rust helper in ./go2_helper_rs,
// which speaks newline-JSON over stdio. We pipe that to/from the browser panel
// over the app-bus. Live control, by contrast, runs entirely in the browser over
// WebRTC — see the signaling relay below.
//
// We also keep a small persisted map of user-given names, keyed by the robot's
// Bluetooth id (its serial / BLE address), so a dog you renamed stays named
// across scans and restarts.

import { TextLineStream } from "https://deno.land/std@0.224.0/streams/text_line_stream.ts"
import { DimAppBackend } from "https://esm.sh/gh/jeff-hykin/dim-app@v0.3.0/backend.js"

// The discovery/provisioning helper is a standalone Rust binary (BLE via
// CoreBluetooth/BlueZ, LAN + ARP over UDP). We build+run it on demand with
// `nix run` so it works on macOS and Linux without a dimos venv.
const HELPER_DIR = new URL("./go2_helper_rs", import.meta.url).pathname
const RESTART_MS = 3000
// Live control now runs in the browser (unitree_go2_webrtc_js). The browser
// can't reach the robot's plain-HTTP signaling port directly (no CORS headers),
// so the panel relays the WebRTC handshake POSTs through here over the app-bus;
// once the peer connection is up, video/data/control flow browser <-> robot and
// never touch this process again.
const SIGNALING_PORTS = [9991]
const RELAY_PATH_OK = /^\/con_[A-Za-z0-9_]*$/
const RELAY_HOST_OK = /^[A-Za-z0-9.\-]+$/
const RELAY_TIMEOUT_MS = 4000
const HOME = Deno.env.get("HOME") || "."
const NAMES_DIR = `${HOME}/.local/share/dim`
const NAMES_FILE = `${NAMES_DIR}/go2_dash_names.json`

const dimApp = new DimAppBackend()

let child = null
let writer = null
let ready = false
// A scan requested before the helper finished building. Flushed once the helper
// emits "ready" (see the stdout loop), so the first-ever scan isn't lost to the
// `nix run` compile.
let pendingScan = null
// Discovered robots, keyed by serial||ble_mac (raw, before name overlay).
const devices = new Map()
// User-given names, keyed the same way. Persisted to NAMES_FILE.
let names = {}
// SSID of the wifi network THIS machine is on (best-effort). The panel autofills
// it and warns when you'd provision a dog onto a different one — a dog on another
// network won't be reachable / discoverable from here.
//   status: "ok"       — real SSID in hostSsid
//           "redacted" — macOS hid it (the dashboard lacks Location Services perm);
//                        it literally returns the string "<redacted>"
//           "unknown"  — no wifi / couldn't read it
let hostSsid = ""
let hostSsidStatus = "unknown"

function deviceKey(device) {
    return device.serial || device.ble_mac || device.ip || JSON.stringify(device)
}

async function runCmd(cmd, args) {
    try {
        const out = await new Deno.Command(cmd, { args, stdout: "piped", stderr: "null" }).output()
        if (!out.success) return ""
        return new TextDecoder().decode(out.stdout)
    } catch {
        return "" // command missing / not permitted — treat as "unknown"
    }
}

// Best-effort current-wifi SSID, mac + linux. Returns "" if it can't tell.
async function detectSsid() {
    const os = Deno.build.os
    if (os === "darwin") {
        // Find the Wi-Fi device (usually en0).
        const ports = await runCmd("networksetup", ["-listallhardwareports"])
        const dev = (ports.match(/Hardware Port:\s*Wi-Fi[\s\S]*?Device:\s*(\w+)/) || [])[1] || "en0"
        // Modern macOS gates `networksetup -getairportnetwork` behind Location
        // Services, but `ipconfig getsummary` still prints "SSID : <name>".
        const summary = await runCmd("ipconfig", ["getsummary", dev])
        const fromSummary = (summary.match(/\bSSID\s*:\s*(.+)/) || [])[1]
        if (fromSummary) return fromSummary.trim()
        const ns = await runCmd("networksetup", ["-getairportnetwork", dev])
        const fromNs = (ns.match(/Current Wi-Fi Network:\s*(.+)/) || [])[1]
        return fromNs ? fromNs.trim() : ""
    }
    if (os === "linux") {
        // NetworkManager first (most desktops), then the wireless-tools fallback.
        const nm = await runCmd("nmcli", ["-t", "-f", "active,ssid", "dev", "wifi"])
        for (const line of nm.split("\n")) {
            if (line.startsWith("yes:")) return line.slice(4).trim()
        }
        const iw = await runCmd("iwgetid", ["-r"])
        return iw.trim()
    }
    return ""
}

async function refreshSsid() {
    let raw = ""
    try { raw = await detectSsid() } catch { raw = "" }
    if (!raw) {
        hostSsid = ""
        hostSsidStatus = "unknown"
    } else if (/redacted/i.test(raw)) {
        // macOS privacy: hidden behind Location Services. Treat as "we can't tell"
        // so we never autofill or warn off a bogus name.
        hostSsid = ""
        hostSsidStatus = "redacted"
    } else {
        hostSsid = raw
        hostSsidStatus = "ok"
    }
}

// macOS only: dimos' LAN discovery multicasts its probe to 231.1.1.1, but that
// group has no route by default — it lands on lo0 (or a full-tunnel VPN owns the
// default route), so the send dies with "No route to host" and the multicast path
// finds nothing (go2_helper falls back to broadcast + ARP). Pinning a host route
// for the group onto the Wi-Fi NIC lets the multicast probe actually reach the
// dogs. That needs root, so it goes through the desktop password modal via
// dimApp.sudo.run; if the user cancels we just keep using broadcast/ARP.
const MULTICAST_GROUP = "231.1.1.1"
let multicastRouteReady = false

async function wifiDevice() {
    const ports = await runCmd("networksetup", ["-listallhardwareports"])
    return (ports.match(/Hardware Port:\s*Wi-Fi[\s\S]*?Device:\s*(\w+)/) || [])[1] || "en0"
}

// Which interface a packet to `host` currently leaves on (no sudo — read-only).
async function routeIface(host) {
    const out = await runCmd("route", ["-n", "get", host])
    return (out.match(/interface:\s*(\w+)/) || [])[1] || null
}

async function ensureMulticastRoute() {
    if (multicastRouteReady || Deno.build.os !== "darwin") {
        return
    }
    const dev = await wifiDevice()
    // Only prompt for sudo if the group is NOT already routed out the Wi-Fi NIC.
    // It already is when there's no VPN, or when a prior session added the route
    // (routes persist until reboot) — in those cases discovery works untouched
    // and we never pop the password modal.
    if ((await routeIface(MULTICAST_GROUP)) === dev) {
        multicastRouteReady = true
        return
    }
    try {
        const res = await dimApp.sudo.run(["route", "-n", "add", "-host", MULTICAST_GROUP, "-interface", dev])
        if (res.cancelled) {
            // User declined — leave it un-added so a later scan can re-prompt;
            // discovery still works via the broadcast/ARP fallbacks.
            dimApp.send("go2", { type: "warn", msg: "Skipped LAN multicast route — using broadcast/ARP discovery only." })
            return
        }
        multicastRouteReady = true
    } catch (err) {
        dimApp.send("go2", { type: "warn", msg: `Could not add LAN multicast route: ${err.message}` })
    }
}

// Overlay the saved custom name (if any) onto a device record.
function named(device) {
    const name = names[deviceKey(device)]
    return name ? { ...device, customName: name } : device
}

async function loadNames() {
    try {
        names = JSON.parse(await Deno.readTextFile(NAMES_FILE))
    } catch {
        names = {}
    }
}

async function saveNames() {
    try {
        await Deno.mkdir(NAMES_DIR, { recursive: true })
        await Deno.writeTextFile(NAMES_FILE, JSON.stringify(names, null, 2))
    } catch (err) {
        console.error(`go2_dash: could not save names — ${err.message}`)
    }
}

function snapshot() {
    dimApp.send("go2", {
        type: "snapshot",
        ready,
        hostSsid,
        hostSsidStatus,
        devices: [...devices.values()].map(named),
    })
}

// Find the `nix` binary. A GUI-launched dashboard may not inherit the shell PATH
// that has nix on it, so fall back to the usual install locations.
let cachedNixBin
async function resolveNixBin() {
    if (cachedNixBin !== undefined) return cachedNixBin
    const candidates = [
        "nix",
        `${HOME}/.nix-profile/bin/nix`,
        "/nix/var/nix/profiles/default/bin/nix",
        "/run/current-system/sw/bin/nix",
        "/opt/homebrew/bin/nix",
        "/usr/local/bin/nix",
        `${HOME}/Commands/nix`,
    ]
    for (const bin of candidates) {
        try {
            const out = await new Deno.Command(bin, { args: ["--version"], stdout: "null", stderr: "null" }).output()
            if (out.success) { cachedNixBin = bin; return bin }
        } catch { /* not here — try the next candidate */ }
    }
    cachedNixBin = null
    return null
}

async function start() {
    const nix = await resolveNixBin()
    if (!nix) {
        // No nix — the helper can't be built. Tell the panel so it shows a clear message.
        ready = false
        snapshot()
        console.error("go2_dash: `nix` not found on PATH — cannot build the Go2 helper")
        if (pendingScan) {
            pendingScan = null
            dimApp.send("go2", { type: "build_error", msg: "`nix` not found — can't build the Go2 BLE helper. Install nix, then press Scan again." })
        }
        setTimeout(start, RESTART_MS)
        return
    }
    try {
        // `nix run` builds the helper on first launch (cached thereafter), then
        // execs it with stdio forwarded. The first build can take a minute; the
        // child stays alive during it, so the restart loop won't re-trigger.
        child = new Deno.Command(nix, {
            args: [
                "run",
                "--extra-experimental-features", "nix-command flakes",
                "-L", // print the builder's own compile logs so we can stream build progress to the panel
                `path:${HELPER_DIR}`,
            ],
            stdin: "piped",
            stdout: "piped",
            stderr: "piped", // read build progress/errors and forward them to the panel
        }).spawn()
    } catch (err) {
        ready = false
        snapshot()
        console.error(`go2_dash: could not start helper — ${err.message}`)
        if (pendingScan) {
            pendingScan = null
            dimApp.send("go2", { type: "build_error", msg: `Could not start the Go2 helper: ${err.message}` })
        }
        setTimeout(start, RESTART_MS)
        return
    }
    writer = child.stdin.getWriter()

    // Forward the helper's stderr. With `-L`, the first launch streams the Rust
    // build log here; until the helper reports "ready" we relay each line to the
    // panel so a pressed Scan shows live compile progress instead of timing out.
    ;(async () => {
        const errLines = child.stderr
            .pipeThrough(new TextDecoderStream())
            .pipeThrough(new TextLineStream())
        for await (const line of errLines) {
            const text = line.trim()
            if (!text) continue
            console.error(`go2_helper: ${text}`)
            if (!ready) dimApp.send("go2", { type: "build", line: text })
        }
    })()

    ;(async () => {
        const lines = child.stdout
            .pipeThrough(new TextDecoderStream())
            .pipeThrough(new TextLineStream())
        for await (const line of lines) {
            if (!line.trim()) continue
            let event
            try { event = JSON.parse(line) } catch { continue }
            if (event.type === "ready") {
                ready = true
                snapshot() // let a panel that opened mid-build learn the helper is up
                if (pendingScan) { const p = pendingScan; pendingScan = null; runScan(p) } // flush a scan deferred during the build
            }
            if (event.type === "device") {
                devices.set(deviceKey(event), event)
                dimApp.send("go2", named(event)) // overlay saved name
                continue
            }
            if (event.type === "drop") {
                devices.delete(event.key) // a serial-less dup folded into its serial card
                dimApp.send("go2", event)
                continue
            }
            dimApp.send("go2", event)
        }
    })()

    child.status.then(() => {
        ready = false
        writer = null
        child = null
        snapshot()
        setTimeout(start, RESTART_MS)
    })
}

function sendToHelper(obj) {
    if (!writer) return
    writer.write(new TextEncoder().encode(JSON.stringify(obj) + "\n"))
        .catch(() => { /* helper gone — status flips on exit */ })
}

// ── WebRTC signaling relay (for the in-browser controller) ───────────────────
// The panel's UnitreeConnection runs the whole WebRTC handshake in the browser,
// but the two signaling POSTs (con_notify / con_ing_*) can't go browser->robot
// directly — the robot's plain-HTTP port sends no CORS headers. The panel sends
// them here over tag "relay" {id, ip, path, body}; we forward to the robot and
// answer on tag "relay_result" {id, ok, status, body}. Once the peer connection
// is up, video/data/control flow browser <-> robot and never touch this process.
// Restricting the path to /con_* keeps this from becoming an open proxy.
async function doRelay(req) {
    const id = req && req.id
    const ip = req && req.ip
    const path = req && req.path
    if (!ip || !path || !RELAY_HOST_OK.test(ip) || !RELAY_PATH_OK.test(path)) {
        dimApp.send("relay_result", { id, ok: false, status: 400, error: "bad ip/path" })
        return
    }
    let lastErr = ""
    for (const port of SIGNALING_PORTS) {
        try {
            const resp = await fetch(`http://${ip}:${port}${path}`, {
                method: "POST",
                body: req.body ?? undefined, // con_notify carries no body
                headers: { "Content-Type": "application/json" },
                signal: AbortSignal.timeout(RELAY_TIMEOUT_MS),
            })
            dimApp.send("relay_result", { id, ok: resp.ok, status: resp.status, body: await resp.text() })
            return
        } catch (err) {
            lastErr = err.message
        }
    }
    dimApp.send("relay_result", { id, ok: false, status: 502, error: `robot ${ip} unreachable: ${lastErr}` })
}

// Run a scan now: a re-scan re-discovers from scratch, so clear the cache, refresh
// the SSID (the machine may have hopped networks), add the LAN multicast route
// (once, behind the password modal) so dimos' native multicast discovery can reach
// the dogs, THEN tell the helper to scan. Only call once the helper is `ready`.
function runScan(payload) {
    devices.clear()
    refreshSsid().then(snapshot)
    ensureMulticastRoute().finally(() => {
        sendToHelper({ type: "scan", timeout: (payload && payload.timeout) || 7 })
    })
}

dimApp.onReceive((kind, payload) => {
    if (kind === "scan") {
        if (ready) {
            runScan(payload)
        } else {
            // Helper still building (first-run `nix run` compile, ~1min). Defer the
            // scan and tell the panel we're building so it shows progress instead of
            // firing its 4s no-response error; it runs the moment the helper is up.
            pendingScan = payload || {}
            dimApp.send("go2", { type: "building" })
        }
    } else if (kind === "connect") {
        sendToHelper({ type: "connect", ...(payload || {}) })
    } else if (kind === "cancel") {
        sendToHelper({ type: "cancel" })
    } else if (kind === "rename") {
        const key = payload && payload.key
        if (!key) return
        const name = (payload.name || "").trim()
        if (name) names[key] = name
        else delete names[key]
        saveNames()
        dimApp.send("go2", { type: "renamed", key, customName: name || null })
    } else if (kind === "relay") {
        doRelay(payload || {})
    } else if (kind === "hello") {
        refreshSsid().then(snapshot) // bring a freshly-opened panel up to date
    }
})

await loadNames()
await refreshSsid()
start()

// go2_dash — backend half (runs in the Deno dashboard process).
//
// Discovery + wifi provisioning for Unitree Go2 robots. BLE scanning and the
// wifi handshake both go through CoreBluetooth (bleak), so this can't be pure
// Deno — we shell into the dimos venv and run go2_helper.py, which reuses the
// exact code behind `dimos go2tool` and speaks newline-JSON over stdio. We pipe
// that to/from the browser panel over the app-bus.
//
// We also keep a small persisted map of user-given names, keyed by the robot's
// Bluetooth id (its serial / BLE address), so a dog you renamed stays named
// across scans and restarts.

import { TextLineStream } from "https://deno.land/std@0.224.0/streams/text_line_stream.ts"
import { DimAppBackend, dimContext } from "https://esm.sh/gh/jeff-hykin/dim-app@v0.3.0/backend.js"

const HELPER_SCRIPT = new URL("./go2_helper.py", import.meta.url).pathname
const CONTROL_SCRIPT = new URL("./go2_control.py", import.meta.url).pathname
const RESTART_MS = 3000
const HOME = Deno.env.get("HOME") || "."
const NAMES_DIR = `${HOME}/.local/share/dim`
const NAMES_FILE = `${NAMES_DIR}/go2_dash_names.json`

const dimApp = new DimAppBackend()
const ctx = dimContext()

let child = null
let writer = null
let ready = false
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
        hasPython: !!ctx.python,
        hostSsid,
        hostSsidStatus,
        devices: [...devices.values()].map(named),
    })
}

async function start() {
    if (!ctx.python) {
        // No dimos venv — tell the panel so it can show a clear message.
        ready = false
        snapshot()
        setTimeout(start, RESTART_MS)
        return
    }
    try {
        child = new Deno.Command(ctx.python, {
            args: [HELPER_SCRIPT],
            cwd: ctx.dimosDir || undefined,
            stdin: "piped",
            stdout: "piped",
            stderr: "null", // dimos logs LAN multicast warnings here — ignore
            env: { PYTHONUNBUFFERED: "1" },
        }).spawn()
    } catch (err) {
        ready = false
        snapshot()
        console.error(`go2_dash: could not start helper — ${err.message}`)
        setTimeout(start, RESTART_MS)
        return
    }
    writer = child.stdin.getWriter()

    ;(async () => {
        const lines = child.stdout
            .pipeThrough(new TextDecoderStream())
            .pipeThrough(new TextLineStream())
        for await (const line of lines) {
            if (!line.trim()) continue
            let event
            try { event = JSON.parse(line) } catch { continue }
            if (event.type === "ready") ready = true
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

// ── live control communicator (go2_control.py) ──────────────────────────────
// One process per open dog: connects to a single robot by ip over WebRTC and
// relays movement / actions / camera. We keep at most one open at a time (the
// dashboard shows one dog), and forward its events to the panel on tag "go2ctl".
let controlChild = null
let controlWriter = null
let controlIp = null

function stopControl() {
    controlIp = null
    controlWriter = null
    const child = controlChild
    controlChild = null
    if (child) {
        try { child.kill() } catch { /* already gone */ }
    }
}

function startControl(ip, aesKey) {
    if (!ip) return
    stopControl()
    if (!ctx.python) {
        dimApp.send("go2ctl", { type: "error", error: "The dimos venv isn't available." })
        return
    }
    controlIp = ip
    try {
        controlChild = new Deno.Command(ctx.python, {
            args: [CONTROL_SCRIPT, ip, aesKey || ""],
            cwd: ctx.dimosDir || undefined,
            stdin: "piped",
            stdout: "piped",
            stderr: "null",
            env: { PYTHONUNBUFFERED: "1" },
        }).spawn()
    } catch (err) {
        controlChild = null
        controlIp = null
        dimApp.send("go2ctl", { type: "error", error: err.message })
        return
    }
    controlWriter = controlChild.stdin.getWriter()
    const child = controlChild

    ;(async () => {
        const lines = child.stdout
            .pipeThrough(new TextDecoderStream())
            .pipeThrough(new TextLineStream())
        for await (const line of lines) {
            if (!line.trim()) continue
            let event
            try { event = JSON.parse(line) } catch { continue }
            dimApp.send("go2ctl", event)
        }
    })()

    child.status.then(() => {
        // Only announce a close if this is still the active child (not a restart).
        if (controlChild === child) {
            controlChild = null
            controlWriter = null
            controlIp = null
            dimApp.send("go2ctl", { type: "closed" })
        }
    })
}

function sendToControl(obj) {
    if (!controlWriter) return
    controlWriter.write(new TextEncoder().encode(JSON.stringify(obj) + "\n"))
        .catch(() => { /* helper gone — "closed" fires on exit */ })
}

dimApp.onReceive((kind, payload) => {
    if (kind === "scan") {
        // A re-scan re-discovers from scratch; clear the cache so stale rows go.
        devices.clear()
        refreshSsid().then(snapshot) // the machine may have hopped networks
        // Add the multicast route (once, behind the password modal) so dimos'
        // native multicast LAN discovery can reach the dogs, THEN start the scan.
        ensureMulticastRoute().finally(() => {
            sendToHelper({ type: "scan", timeout: (payload && payload.timeout) || 7 })
        })
    } else if (kind === "connect") {
        sendToHelper({ type: "connect", ...(payload || {}) })
    } else if (kind === "launch") {
        // Launch a dimos blueprint via the dashboard's own API (we run inside
        // that same Deno process). Assumes the python bridge is already up. The
        // robot ip is forwarded for the eventual per-robot launch API — the
        // current /api/launch ignores it, which is harmless.
        const name = payload && payload.name
        if (!name) return
        const ip = payload && payload.ip
        const port = Deno.env.get("DIM_DASHBOARD_PORT") || "1024"
        const dhost = Deno.env.get("DIM_DASHBOARD_HOST") || "127.0.0.1"
        ;(async () => {
            try {
                const res = await fetch(`http://${dhost}:${port}/api/launch`, {
                    method: "POST",
                    headers: { "content-type": "application/json" },
                    body: JSON.stringify({ name, ip }),
                })
                const data = await res.json().catch(() => ({}))
                dimApp.send("go2", { type: "launch_result", name, ok: !!data.ok, pid: data.pid, error: data.error })
            } catch (err) {
                dimApp.send("go2", { type: "launch_result", name, ok: false, error: err.message })
            }
        })()
    } else if (kind === "rename") {
        const key = payload && payload.key
        if (!key) return
        const name = (payload.name || "").trim()
        if (name) names[key] = name
        else delete names[key]
        saveNames()
        dimApp.send("go2", { type: "renamed", key, customName: name || null })
    } else if (kind === "open_control") {
        startControl(payload && payload.ip, payload && payload.aesKey)
    } else if (kind === "close_control") {
        stopControl()
    } else if (kind === "move") {
        sendToControl({ type: "move", ...(payload || {}) })
    } else if (kind === "action") {
        sendToControl({ type: "action", name: payload && payload.name })
    } else if (kind === "hello") {
        refreshSsid().then(snapshot) // bring a freshly-opened panel up to date
    }
})

await loadNames()
await refreshSsid()
start()

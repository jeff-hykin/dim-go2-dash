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
import { DimAppBackend, dimContext } from "https://esm.sh/gh/jeff-hykin/dim-app@v0.1.0/backend.js"

const SIDECAR = new URL("./go2_helper.py", import.meta.url).pathname
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

function deviceKey(device) {
    return device.serial || device.ble_mac || JSON.stringify(device)
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
            args: [SIDECAR],
            cwd: ctx.dimosDir || undefined,
            stdin: "piped",
            stdout: "piped",
            stderr: "null", // dimos logs LAN multicast warnings here — ignore
            env: { PYTHONUNBUFFERED: "1" },
        }).spawn()
    } catch (err) {
        ready = false
        snapshot()
        console.error(`go2_dash: could not start sidecar — ${err.message}`)
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

function sendToSidecar(obj) {
    if (!writer) return
    writer.write(new TextEncoder().encode(JSON.stringify(obj) + "\n"))
        .catch(() => { /* sidecar gone — status flips on exit */ })
}

dimApp.onReceive((kind, payload) => {
    if (kind === "scan") {
        // A re-scan re-discovers from scratch; clear the cache so stale rows go.
        devices.clear()
        sendToSidecar({ type: "scan", timeout: (payload && payload.timeout) || 7 })
    } else if (kind === "connect") {
        sendToSidecar({ type: "connect", ...(payload || {}) })
    } else if (kind === "rename") {
        const key = payload && payload.key
        if (!key) return
        const name = (payload.name || "").trim()
        if (name) names[key] = name
        else delete names[key]
        saveNames()
        dimApp.send("go2", { type: "renamed", key, customName: name || null })
    } else if (kind === "hello") {
        snapshot() // bring a freshly-opened panel up to date
    }
})

await loadNames()
start()

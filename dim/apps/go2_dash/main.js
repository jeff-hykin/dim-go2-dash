// go2_dash — backend half (runs in the Deno dashboard process).
//
// Discovery + wifi provisioning for Unitree Go2 robots. BLE scanning and the
// wifi handshake both go through CoreBluetooth (bleak), so this can't be pure
// Deno — we shell into the dimos venv and run go2_helper.py, which reuses the
// exact code behind `dimos go2tool` and speaks newline-JSON over stdio. We pipe
// that to/from the browser panel over the app-bus.

import { TextLineStream } from "https://deno.land/std@0.224.0/streams/text_line_stream.ts"
import { DimAppBackend, dimContext } from "https://esm.sh/gh/jeff-hykin/dim-app@v0.1.0/backend.js"

const SIDECAR = new URL("./go2_helper.py", import.meta.url).pathname
const RESTART_MS = 3000

const dimApp = new DimAppBackend()
const ctx = dimContext()

let child = null
let writer = null
let ready = false
// Cache discovered robots (keyed by serial||ble_mac) so a freshly-opened tab
// gets the current list without waiting for a fresh scan.
const devices = new Map()

function deviceKey(device) {
    return device.serial || device.ble_mac || JSON.stringify(device)
}

function snapshot() {
    dimApp.send("go2", {
        type: "snapshot",
        ready,
        hasPython: !!ctx.python,
        devices: [...devices.values()],
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
            if (event.type === "device") devices.set(deviceKey(event), event)
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
    } else if (kind === "hello") {
        snapshot() // bring a freshly-opened panel up to date
    }
})

start()

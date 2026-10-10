// What the backend says (GET api/state), kept current by its events (zenoh topic `events`, through
// Desktop's relay; re-read after the zenoh-gateway connection comes back). The page never computes robot or
// session state itself: whatever the UI or the agent changed arrives here the same way.
import { useEffect, useState } from "react"
import { call, events } from "./api.ts"

export type Robot = {
    key: string
    name: string
    customName: string | null
    bleName: string | null
    serial: string | null
    bleId: string | null
    ip: string | null
    ipSource: "scan" | "remembered" | null
    /** its Wi-Fi MAC */
    lanMac: string | null
    /** its Bluetooth MAC (Linux only: macOS hides it) */
    bleMac: string | null
    /** how its IP was found: Bluetooth MAC joined to ARP, LAN discovery, a Unitree MAC only, a guess, or not on this network */
    matched: "ble+arp" | "ble+lan" | "lan" | "oui" | "guess" | "ble"
    arpOnly: boolean
    hasAesKey: boolean
    canProvisionWifi: boolean
}

/** The ARP sweep (discovery.rs `sweep` events): what it covers and how far it got */
export type Sweep = {
    status: "running" | "done" | "cancelled" | "error"
    phase?: "known" | "subnet" | "widen" | "done"
    iface?: string
    ip?: string
    subnet?: string
    swept?: string | null
    partial?: boolean
    fullHosts?: number
    sent?: number
    total?: number
    alive?: number
    method?: string | null
    note?: string
    error?: string
}

export type Scan = { scanning: boolean; lastCount: number | null; notice: string | null; sweep?: Sweep | null }

export type Wifi = {
    status: "idle" | "running" | "ok" | "error" | "cancelled" | "dry-run"
    robot?: string
    ssid?: string
    country?: string
    log?: string[]
    serial?: string | null
    error?: string
    warning?: string | null
    dryRun?: boolean
}

export type CommandRecord = { name: string; label: string; at: number; sent: boolean; dryRun: boolean }

export type Drive =
    | { active: false }
    | {
        active: true
        robot: string | null
        ip: string
        name: string
        dryRun: boolean
        status: "connecting" | "ready" | "reconnecting" | "error"
        error: string | null
        mode: "resting" | "rising" | "stand" | "pose"
        velocity: { forward: number; strafe: number; turn: number; run: boolean }
        moving: boolean
        video: boolean
        lastCommand: CommandRecord | null
        startedAt: number
    }

export type Account = {
    email: string
    lastPull: number | null
    pulling: boolean
    error: string | null
    robots: { sn: string; alias: string; hasKey: boolean }[]
}

export type Command = { name: string; label: string; description: string }

export type Network = { ssid: string | null; ssidStatus: "ok" | "redacted" | "unknown"; mock: boolean }

export type SetupStep = "welcome" | "find" | "wifi" | "address" | "launch" | "done"

export type SetupState = {
    step: SetupStep
    mode: "robot" | "replay" | null
    robot: {
        key: string
        name: string
        ip: string | null
        serial: string | null
        bleId: string | null
        hasAesKey: boolean
    } | null
}

/** Desktop's launch (docs/api.md `Launch`), as api/launch passes it on */
export type Launch = {
    blueprint: string
    phase: "starting" | "running" | "stopped" | "failed"
    error: string | null
    overrides: Record<string, unknown>
    steps?: { label: string; state: "done" | "now" | "todo" | "failed"; detail: string | null }[]
    problems?: { level: string; text: string; fix: string | null }[]
    output?: string
    mock?: boolean
}

/** The recording in progress (GET api/record) */
export type RecordState =
    | { active: false }
    | {
        active: true
        file: string
        path: string
        startedAt: number
        seconds: number
        messages: number
        bytes: number
        skipped?: number
        streams?: { topic: string; bytesPerSecond: number }[]
        dropped: number
        topics: Record<string, number>
        robot: string
        dryRun: boolean
    }

/** Desktop's upload of a recording, as this app follows it */
export type Upload = {
    id?: string
    state: "queued" | "uploading" | "done" | "failed" | "cancelled" | "offline" | "signin" | "waiting"
    phase?: string | null
    bytesDone?: number | null
    bytesTotal?: number | null
    etaSeconds?: number | null
    error?: string | null
    link?: string | null
    auto?: boolean
    attempts?: number
    retryAt?: number
}

export type Recording = {
    file: string
    name: string
    path: string
    /** Desktop's id (its path under the recordings folder), for Recordings' #/replay/<id> */
    id: string | null
    bytes: number
    startedAt: number
    seconds: number | null
    robot: string | null
    recording: boolean
    recovered: boolean
    mock: boolean
    upload: Upload | null
}

export type Settings = { autoUpload: boolean }

/** AP mode: this computer's link to a Go2's own hotspot (GET api/hotspot) */
export type HotspotState = {
    status: "idle" | "joining" | "linked" | "restoring" | "error"
    ssid: string | null
    previous: string | null
    error: string | null
    ip: string
    platform: string
    canScan: boolean
    saved: string[]
    /** the page's own: the network this computer is on now, from the last scan */
    current?: string | null
}

export type State = {
    robots: Robot[]
    scan: Scan
    wifi: Wifi
    drive: Drive
    network: Network
    accounts: Account[]
    commands: Command[]
    setup: SetupState
    record: RecordState
    recordings: Recording[]
    settings: Settings
    hotspot: HotspotState
}

/** how often the page re-reads the state while the zenoh-gateway connection is down */
const POLL_MS = 1000

/** The backend's state, or an error string when it can't be reached. `onCommand` sees every robot command, whoever sent it. */
export function useBackend(onCommand: (command: CommandRecord) => void): [State | null, string | null] {
    const [state, setState] = useState<State | null>(null)
    const [error, setError] = useState<string | null>(null)
    useEffect(() => {
        const load = () =>
            call<State>("GET", "api/state").then(
                (s) => {
                    setState(s)
                    setError(null)
                },
                (e) => setError(e.message),
            )
        load()
        // without the zenoh-gateway (it's down, or can't connect on this machine) the page polls instead of going stale
        let live = false
        const poll = setInterval(() => live || load(), POLL_MS)
        const stop = events((event) => {
            if (event.type === "command") {
                onCommand(event.command as CommandRecord)
                return
            }
            const slice = {
                robots: "robots",
                scan: "scan",
                wifi: "wifi",
                drive: "drive",
                network: "network",
                accounts: "accounts",
                setup: "setup",
                record: "record",
                recordings: "recordings",
                settings: "settings",
                hotspot: "hotspot",
            }[
                event.type as string
            ] as keyof State | undefined
            if (slice) {
                setState((s) => (s ? { ...s, [slice]: event[slice] } : s))
            }
        }, () => {
            live = true
            load()
        }, () => {
            live = false
        })
        return () => {
            clearInterval(poll)
            stop()
        }
    }, [])
    return [state, error]
}

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
    lanMac: string | null
    arpOnly: boolean
    hasAesKey: boolean
    canProvisionWifi: boolean
}

export type Scan = { scanning: boolean; lastCount: number | null; notice: string | null }

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

export type State = {
    robots: Robot[]
    scan: Scan
    wifi: Wifi
    drive: Drive
    network: Network
    accounts: Account[]
    commands: Command[]
    setup: SetupState
}

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
        return events((event) => {
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
            }[
                event.type as string
            ] as keyof State | undefined
            if (slice) {
                setState((s) => (s ? { ...s, [slice]: event[slice] } : s))
            }
        }, load)
    }, [])
    return [state, error]
}

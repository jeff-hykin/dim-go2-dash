// The live-control stage: the robot's camera, its commands, and the d-pad / keyboard driver. Every press is an
// endpoint call (api/drive/*); the mode, status and last command shown come back from the backend, so a command the
// agent sends flashes here too.
import { useEffect, useRef, useState } from "react"
import { call } from "./api.ts"
import {
    activePad,
    type Axes,
    GAMEPAD_BINDINGS,
    GamepadDriver,
    type GamepadStatus,
    joySample,
    noPad,
    type PadLike,
} from "./gamepad.ts"
import { Icon } from "./icons.tsx"
import type { Command, Drive, RecordState } from "./state.ts"

type Active = Extract<Drive, { active: true }>

// keyboard → logical axis key (w/s fwd-back, a/d turn, q/e strafe)
const KEYMAP: Record<string, string> = {
    keyw: "w",
    arrowup: "w",
    keys: "s",
    arrowdown: "s",
    keya: "a",
    arrowleft: "a",
    keyd: "d",
    arrowright: "d",
    keyq: "q",
    keye: "e",
}
/** a held key re-sends its move this often, each lasting a little longer so the stream has no gaps */
const MOVE_TICK_MS = 150
const MOVE_HOLD_MS = 400

const PILL_TONE: Record<string, string> = { connecting: "info", reconnecting: "info", ready: "ok", error: "danger" }
const MODE_TONE: Record<string, string> = { stand: "ok", pose: "ok", rising: "info" }
const STATUS_LABEL: Record<string, string> = {
    connecting: "Connecting",
    ready: "Live",
    reconnecting: "Reconnecting",
    error: "Error",
}
const MODE_LABEL: Record<string, string> = {
    rising: "Standing up…",
    stand: "Standing",
    pose: "Pose",
    resting: "Resting",
}

/** The camera: a receive-only WebRTC peer with the backend, which forwards the robot's track. */
function useCamera(drive: Active, video: React.RefObject<HTMLVideoElement | null>): boolean {
    const [live, setLive] = useState(false)
    const want = drive.status === "ready" && drive.video
    useEffect(() => {
        setLive(false)
        if (!want) {
            return
        }
        let stopped = false
        let pc: RTCPeerConnection | null = null
        let retry: ReturnType<typeof setTimeout> | undefined
        const open = async () => {
            pc?.close()
            pc = new RTCPeerConnection({ iceServers: [] })
            pc.addTransceiver("video", { direction: "recvonly" })
            pc.ontrack = (event) => {
                if (video.current) {
                    video.current.srcObject = event.streams[0] ?? new MediaStream([event.track])
                    video.current.play?.().catch(() => {})
                }
            }
            pc.onconnectionstatechange = () => {
                if (pc?.connectionState === "failed" && !stopped) {
                    retry = setTimeout(open, 1500)
                }
            }
            await pc.setLocalDescription(await pc.createOffer())
            await new Promise<void>((resolve) => {
                if (pc!.iceGatheringState === "complete") {
                    return resolve()
                }
                pc!.addEventListener("icegatheringstatechange", () => pc!.iceGatheringState === "complete" && resolve())
                setTimeout(resolve, 2000)
            })
            try {
                const answer = await call<{ sdp: string }>("POST", "api/drive/video", { sdp: pc.localDescription!.sdp })
                if (!stopped) {
                    await pc.setRemoteDescription({ type: "answer", sdp: answer.sdp })
                }
            } catch {
                if (!stopped) {
                    retry = setTimeout(open, 1000)
                }
            }
        }
        open()
        return () => {
            stopped = true
            clearTimeout(retry)
            pc?.close()
        }
    }, [want, drive.startedAt])
    useEffect(() => {
        const element = video.current
        if (!element) {
            return
        }
        const onFrame = () => setLive(true)
        element.addEventListener("loadeddata", onFrame)
        return () => element.removeEventListener("loadeddata", onFrame)
    }, [video.current])
    return live && want
}

/** The page has the user's focus: this document, or Desktop's shell around it (a Steam Deck user never clicks in). */
function pageHasFocus(): boolean {
    if (document.hasFocus()) {
        return true
    }
    try {
        return globalThis.top !== globalThis.self && !!globalThis.top?.document.hasFocus()
    } catch {
        return false
    }
}

const PAD_POLL_MS = 30
/** Joy samples go to the recording when they change, at most this often (Stash's cockpit: 15 Hz) */
const JOY_MIN_MS = 66
const ZERO_AXES: Axes = { forward: 0, strafe: 0, turn: 0 }

/** The gamepad (gamepad.ts has the rules): polled while one is connected; zeroed on blur, a hidden page, disconnect. */
function useGamepad(handlers: {
    stop: () => void
    sitDown: () => void
    setBoost: (boost: boolean) => void
}): [GamepadStatus, Axes] {
    const [status, setStatus] = useState<GamepadStatus>(noPad())
    const [axes, setAxes] = useState<Axes>(ZERO_AXES)
    const latest = useRef(handlers)
    latest.current = handlers
    useEffect(() => {
        if (typeof navigator === "undefined" || !navigator.getGamepads) {
            return
        }
        const driver = new GamepadDriver({
            setAxes,
            setBoost: (boost) => latest.current.setBoost(boost),
            stop: () => latest.current.stop(),
            sitDown: () => latest.current.sitDown(),
        }, setStatus)
        const pads = () => [...navigator.getGamepads()] as (PadLike | null)[]
        let lastJoy = ""
        let lastJoyAt = 0
        const poll = () => {
            const now = performance.now()
            if (document.hidden || !pageHasFocus()) {
                driver.release()
            } else {
                driver.poll(pads(), now)
            }
            // the raw pad for the recording (never velocities): on change, ≤ 15 Hz;
            const pad = activePad(pads())
            // sent whether or not a recording runs (it may be started from elsewhere); the backend keeps them only while one does
            if (pad && now - lastJoyAt >= JOY_MIN_MS) {
                const sample = joySample(pad)
                const key = JSON.stringify(sample)
                if (key !== lastJoy) {
                    lastJoy = key
                    lastJoyAt = now
                    call("POST", "api/drive/joy", sample).catch(() => {})
                }
            }
        }
        const timer = setInterval(poll, PAD_POLL_MS)
        const release = () => driver.release()
        const hidden = () => document.hidden && driver.release()
        addEventListener("blur", release)
        document.addEventListener("visibilitychange", hidden)
        return () => {
            clearInterval(timer)
            removeEventListener("blur", release)
            document.removeEventListener("visibilitychange", hidden)
            driver.release()
        }
    }, [])
    return [status, axes]
}

function clock(seconds: number): string {
    const s = Math.floor(seconds)
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`
}

export function megabytes(bytes: number): string {
    return bytes >= 1e9 ? `${(bytes / 1e9).toFixed(2)} GB` : `${(bytes / 1e6).toFixed(bytes >= 1e7 ? 0 : 1)} MB`
}

/** Record / stop, with the time and size while it runs. */
function RecordButton({ record, onToast }: { record: RecordState; onToast: (text: string) => void }) {
    const [busy, setBusy] = useState(false)
    const [, tick] = useState(0)
    useEffect(() => {
        if (!record.active) {
            return
        }
        const timer = setInterval(() => tick((n) => n + 1), 1000)
        return () => clearInterval(timer)
    }, [record.active])
    const toggle = () => {
        setBusy(true)
        call("POST", record.active ? "api/record/stop" : "api/record/start")
            .then((r) => !record.active || onToast(`Saved ${(r as { file?: string })?.file ?? "the recording"}`))
            .catch((e) => onToast(e.message))
            .finally(() => setBusy(false))
    }
    const seconds = record.active ? (Date.now() - record.startedAt) / 1000 : 0
    return (
        <button
            type="button"
            className={`rec-btn dim-btn sm${record.active ? " on" : ""}`}
            disabled={busy}
            title={record.active
                ? `Recording to ${record.file} — click to stop and save`
                : "Record this session (camera, lidar, odometry, IMU, battery, gamepad, commands) to an mcap"}
            onClick={toggle}
        >
            <span className="rec-dot" />
            {record.active ? `${clock(seconds)} · ${megabytes(record.bytes)}` : busy ? "Starting…" : "Record"}
        </button>
    )
}

export function Control(props: {
    drive: Active
    commands: Command[]
    record: RecordState
    keyboardActive: boolean
    flash: { name: string; ok: boolean; n: number } | null
    onFlash: (name: string, ok: boolean) => void
    onToast: (text: string) => void
    onSignIn: () => void
    onRecordings: () => void
}) {
    const { drive, commands, record, keyboardActive, flash, onFlash, onToast, onSignIn, onRecordings } = props
    const video = useRef<HTMLVideoElement>(null)
    const live = useCamera(drive, video)
    const [pressed, setPressed] = useState<Set<string>>(new Set())
    const [boost, setBoost] = useState(false)
    const [searching, setSearching] = useState(false)
    const [query, setQuery] = useState("")
    const searchInput = useRef<HTMLInputElement>(null)
    const standing = drive.mode === "stand" || drive.mode === "pose"
    const ready = drive.status === "ready"

    const doCommand = (name: string) => {
        if (!ready) {
            return
        }
        call("POST", `api/drive/${name}`).catch((e) => {
            onFlash(name, false)
            onToast(e.message)
        })
    }

    const driveAllowed = () => {
        if (standing) {
            return true
        }
        onToast('Robot isn’t standing — press "Stand" to drive.')
        return false
    }

    const press = (key: string) => {
        if (!driveAllowed()) {
            return
        }
        setPressed((p) => (p.has(key) ? p : new Set(p).add(key)))
    }
    const release = (key: string) =>
        setPressed((p) => {
            if (!p.has(key)) {
                return p
            }
            const next = new Set(p)
            next.delete(key)
            return next
        })

    const sitDown = () => {
        setPressed(new Set())
        call("POST", "api/drive/sit-down").then(
            () => onToast("Sitting down (stop, then StandDown)"),
            (e) => onToast(e.message),
        )
    }
    const stopNow = () => {
        setPressed(new Set())
        call("POST", "api/drive/stop").catch(() => {})
    }
    const [pad, padAxes] = useGamepad({ stop: stopNow, sitDown, setBoost })
    const padMoving = !!(padAxes.forward || padAxes.strafe || padAxes.turn) && standing && ready
    const clamp = (v: number) => Math.max(-1, Math.min(1, v))
    const vector = {
        forward: clamp((pressed.has("w") ? 1 : 0) - (pressed.has("s") ? 1 : 0) + (padMoving ? padAxes.forward : 0)),
        strafe: clamp((pressed.has("q") ? 1 : 0) - (pressed.has("e") ? 1 : 0) + (padMoving ? padAxes.strafe : 0)),
        turn: clamp((pressed.has("a") ? 1 : 0) - (pressed.has("d") ? 1 : 0) + (padMoving ? padAxes.turn : 0)),
    }
    const anyAxis = !!(vector.forward || vector.strafe || vector.turn)

    // held keys → a steady stream of short moves; letting go → one stop
    const vectorRef = useRef(vector)
    vectorRef.current = vector
    const boostRef = useRef(boost)
    boostRef.current = boost
    useEffect(() => {
        if (!anyAxis || !ready) {
            return
        }
        const send = () =>
            call("POST", "api/drive/move", { ...vectorRef.current, run: boostRef.current, durationMs: MOVE_HOLD_MS })
                .catch((e) => onToast(e.message))
        send()
        const timer = setInterval(send, MOVE_TICK_MS)
        return () => {
            clearInterval(timer)
            call("POST", "api/drive/stop").catch(() => {})
        }
    }, [anyAxis, ready])

    // keyboard driving, while the setup panel is slid away
    useEffect(() => {
        if (!keyboardActive) {
            return
        }
        const codeKey = (e: KeyboardEvent) =>
            KEYMAP[(e.code || "").toLowerCase()] || KEYMAP[(e.key || "").toLowerCase()]
        const down = (e: KeyboardEvent) => {
            const t = e.target as HTMLElement | null
            if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA")) {
                return // don't drive while typing
            }
            if (e.altKey || e.ctrlKey || e.metaKey) {
                return // modified combos (e.g. alt+arrows = switch apps) belong to the shell
            }
            if (e.key === "/" && !e.repeat) {
                e.preventDefault()
                setSearching(true)
                setTimeout(() => searchInput.current?.focus(), 0)
                return
            }
            if (e.key === "Shift") {
                setBoost(true)
                return
            }
            const k = codeKey(e)
            if (!k) {
                return
            }
            e.preventDefault()
            press(k)
        }
        const up = (e: KeyboardEvent) => {
            if (e.key === "Shift") {
                setBoost(false)
                return
            }
            const k = codeKey(e)
            if (k) {
                release(k)
            }
        }
        // drop held keys if focus leaves the window (no stuck throttle)
        const blur = () => {
            setPressed(new Set())
            setBoost(false)
        }
        globalThis.addEventListener("keydown", down)
        globalThis.addEventListener("keyup", up)
        globalThis.addEventListener("blur", blur)
        return () => {
            globalThis.removeEventListener("keydown", down)
            globalThis.removeEventListener("keyup", up)
            globalThis.removeEventListener("blur", blur)
        }
    }, [keyboardActive, standing])
    useEffect(() => {
        if (!keyboardActive) {
            setPressed(new Set())
            setBoost(false)
        }
    }, [keyboardActive])

    const visible = commands.filter((c) =>
        !query || `${c.label} ${c.name}`.toLowerCase().includes(query.trim().toLowerCase())
    )
    const closeSearch = () => {
        setSearching(false)
        setQuery("")
        searchInput.current?.blur()
    }
    const placeholder = drive.dryRun
        ? "Dry run — nothing is sent to a robot, and there's no camera."
        : ready
        ? "Waiting for video…"
        : drive.status === "reconnecting"
        ? `Reconnecting to ${drive.name}…`
        : `Connecting to ${drive.name}…`
    const dpad: [string, string, string, string][] = [
        ["q", "sl", "rotate-left", "Strafe left"],
        ["w", "fwd", "arrow-up", "Forward"],
        ["e", "sr", "rotate-right", "Strafe right"],
        ["a", "left", "arrow-left", "Turn left"],
        ["s", "back", "arrow-down", "Backward"],
        ["d", "right", "arrow-right", "Turn right"],
    ]
    const shown = anyAxis ? vector : drive.velocity
    return (
        <div className="ctl">
            <div className={`cam-wrap${live ? " live" : ""}`}>
                <video ref={video} autoPlay muted playsInline />
                <div className="cam-ph">
                    <div className="glyph">
                        <Icon name="camera" size={40} />
                    </div>
                    <div>{placeholder}</div>
                </div>
                <div className="ctl-top">
                    <span className="nm">{drive.name}</span>
                    <span className="ctl-ip dim-mono">{drive.ip}</span>
                    <span className={`ctl-pill dim-badge ${PILL_TONE[drive.status] ?? ""}`}>
                        <span className="dot" />
                        {STATUS_LABEL[drive.status] ?? drive.status}
                    </span>
                    {drive.dryRun && <span className="dim-badge warn">Dry run</span>}
                    <PadChip pad={pad} linked={ready} />
                    <span className="spacer" />
                    <RecordButton record={record} onToast={onToast} />
                    <button
                        type="button"
                        className="dim-btn sm"
                        title="This app's recordings: upload, rename, open"
                        onClick={onRecordings}
                    >
                        <Icon name="folder" size={14} />
                        Recordings
                    </button>
                    <button
                        type="button"
                        className="ctl-close dim-btn sm"
                        title="Disconnect"
                        onClick={() => call("POST", "api/drive/disconnect").catch(() => {})}
                    >
                        <Icon name="close" size={14} />
                        Disconnect
                    </button>
                </div>
                {drive.status === "error" && (
                    <div className="ctl-err dim-alert danger">
                        {drive.error}
                        {/AES|data2=3/i.test(drive.error ?? "") && (
                            <>
                                {" "}This dog needs its AES key from Unitree's cloud.
                                <button type="button" className="dim-btn sm ctl-signin" onClick={onSignIn}>
                                    Sign in to Unitree…
                                </button>
                            </>
                        )}
                    </div>
                )}
                <div className={`vel dim-panel glass dim-mono${anyAxis || drive.moving ? " on" : ""}`}>
                    fwd {shown.forward.toFixed(2)} · str {shown.strafe.toFixed(2)} · yaw {shown.turn.toFixed(2)}
                    {boost ? "  ·  run" : "  ·  shift = run"}
                </div>
                <div className="dpad">
                    {dpad.map(([key, cls, icon, title]) => (
                        <button
                            type="button"
                            key={key}
                            className={`dbtn dim-btn icon ${cls}${pressed.has(key) ? " held" : ""}`}
                            title={title}
                            onPointerDown={(e) => {
                                e.preventDefault()
                                e.currentTarget.setPointerCapture?.(e.pointerId)
                                press(key)
                            }}
                            onPointerUp={() => release(key)}
                            onPointerCancel={() => release(key)}
                        >
                            <Icon name={icon} size={18} />
                        </button>
                    ))}
                </div>
                <div className="cmd-dock">
                    <span className={`mode-badge dim-badge ${MODE_TONE[drive.mode] ?? ""}`} title="Current dog mode">
                        Mode: {MODE_LABEL[drive.mode] ?? "—"}
                    </span>
                    <div className={`cmd-bar${searching ? " searching" : ""}`}>
                        <div className="cmd-search dim-panel glass">
                            <span className="ico">
                                <Icon name="search" size={13} />
                            </span>
                            <input
                                ref={searchInput}
                                className="dim-input"
                                type="text"
                                placeholder="Search commands…"
                                autoComplete="off"
                                spellCheck={false}
                                value={query}
                                onChange={(e) => setQuery(e.target.value)}
                                onKeyDown={(e) => {
                                    if (e.key === "Escape") {
                                        e.preventDefault()
                                        e.stopPropagation()
                                        closeSearch()
                                    } else if (e.key === "Enter") {
                                        e.preventDefault()
                                        if (visible[0]) {
                                            doCommand(visible[0].name)
                                        }
                                        closeSearch()
                                    }
                                }}
                            />
                        </div>
                        <div className="cmd-scroll">
                            {commands.map((c, i) => {
                                const flashing = flash && flash.name === c.name ? (flash.ok ? " ok" : " err") : ""
                                const hidden = !visible.includes(c) ? " nomatch" : ""
                                return (
                                    <button
                                        type="button"
                                        key={`${c.name}-${flash?.name === c.name ? flash.n : 0}`}
                                        className={`act dim-btn sm${i === 0 ? " primary" : ""}${flashing}${hidden}`}
                                        title={c.description}
                                        onClick={() => doCommand(c.name)}
                                    >
                                        {c.label}
                                    </button>
                                )
                            })}
                        </div>
                    </div>
                </div>
                <button
                    type="button"
                    className={`sit-down dim-btn sm${pad.sitHold > 0 ? " holding" : ""}`}
                    style={{ "--hold": pad.sitHold } as React.CSSProperties}
                    disabled={!ready}
                    title="Sit the dog down safely without closing the app (low battery, bad link): stop, then lie down. Gamepad: hold B for 1 s"
                    onClick={sitDown}
                >
                    <Icon name="power" size={13} />
                    Sit down
                </button>
            </div>
        </div>
    )
}

function PadChip({ pad, linked }: { pad: GamepadStatus; linked: boolean }) {
    if (!pad.connected) {
        return null
    }
    // the link to the dog dropped (reconnecting): the pad drives nothing until it's back
    const [tone, label] = !linked
        ? ["danger", "Disengaged"]
        : pad.stopped
        ? ["warn", "Stopped — press A"]
        : pad.ready
        ? ["ok", "Gamepad"]
        : ["warn", "Center the sticks"]
    return (
        <span
            className={`pad-chip dim-badge ${tone}`}
            title={`Gamepad: ${pad.id}\n${GAMEPAD_BINDINGS.map(([b, a]) => `${b}: ${a}`).join("\n")}`}
        >
            <Icon name="gamepad" size={12} />
            {label}
        </span>
    )
}

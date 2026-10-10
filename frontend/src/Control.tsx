import { ErrorNotice } from "./errors.tsx"
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
import { RecordOptions } from "./RecordOptions.tsx"
import { Icon } from "./icons.tsx"
import { store, stored } from "./util.ts"
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
/** whether the on-screen driving buttons are shown (localStorage) */
const SHOW_CONTROLS_KEY = "go2dash.showControls"

/** how long the WebRTC video gets to start before the page also tries the WebSocket stream */
const WS_FALLBACK_AFTER_MS = 4000

/**
 * The camera over a WebSocket (api/drive/video.ws: the robot's H.264 access units, unchanged) decoded with WebCodecs
 * onto `canvas`, for when the WebRTC video won't connect. Starts once `want` has lasted WS_FALLBACK_AFTER_MS without the
 * WebRTC video going live; returns whether frames are being drawn.
 */
function useWsVideo(want: boolean, webrtcLive: boolean, canvas: React.RefObject<HTMLCanvasElement | null>): boolean {
    const [waited, setWaited] = useState(false)
    const [drawing, setDrawing] = useState(false)
    useEffect(() => {
        setWaited(false)
        if (!want) {
            return
        }
        const timer = setTimeout(() => setWaited(true), WS_FALLBACK_AFTER_MS)
        return () => clearTimeout(timer)
    }, [want])
    const active = want && waited && !webrtcLive && typeof VideoDecoder !== "undefined"
    useEffect(() => {
        setDrawing(false)
        if (!active) {
            return
        }
        let stopped = false
        let socket: WebSocket | null = null
        let decoder: VideoDecoder | null = null
        let retry: ReturnType<typeof setTimeout> | undefined
        let needKey = true
        const newDecoder = () => {
            if (decoder && decoder.state !== "closed") {
                decoder.close()
            }
            needKey = true
            decoder = new VideoDecoder({
                output: (frame) => {
                    const target = canvas.current
                    if (target) {
                        if (target.width !== frame.displayWidth || target.height !== frame.displayHeight) {
                            target.width = frame.displayWidth
                            target.height = frame.displayHeight
                        }
                        target.getContext("2d")?.drawImage(frame, 0, 0)
                        setDrawing(true)
                    }
                    frame.close()
                },
                // a bad frame: start again from the next keyframe
                error: () => !stopped && newDecoder(),
            })
            // no `description`: the chunks are Annex B
            decoder.configure({ codec: "avc1.42e01f", optimizeForLatency: true })
        }
        const open = () => {
            newDecoder()
            const url = new URL("api/drive/video.ws", location.href)
            url.protocol = url.protocol === "https:" ? "wss:" : "ws:"
            socket = new WebSocket(url)
            socket.binaryType = "arraybuffer"
            socket.onmessage = (event) => {
                const bytes = new Uint8Array(event.data as ArrayBuffer)
                const key = bytes[0] === 1
                if (needKey && !key) {
                    return
                }
                needKey = false
                const timestamp = Number(new DataView(bytes.buffer, 1, 8).getBigUint64(0, true))
                try {
                    decoder?.decode(
                        new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp, data: bytes.subarray(9) }),
                    )
                } catch {
                    newDecoder()
                }
            }
            socket.onclose = () => {
                setDrawing(false)
                if (!stopped) {
                    retry = setTimeout(open, 1000)
                }
            }
        }
        open()
        return () => {
            stopped = true
            clearTimeout(retry)
            socket?.close()
            if (decoder && decoder.state !== "closed") {
                decoder.close()
            }
        }
    }, [active])
    return drawing
}

/** The camera's live state and, while it isn't live, which step it's stuck on (shown in the placeholder). */
function useCamera(
    drive: Active,
    video: React.RefObject<HTMLVideoElement | null>,
    canvas: React.RefObject<HTMLCanvasElement | null>,
): { live: boolean; status: string; ws: boolean } {
    const [live, setLive] = useState(false)
    const [status, setStatus] = useState("")
    // not waiting for the `video` flag: its event can be lost (the Wi-Fi switching to the dog's hotspot drops the
    // page's zenoh link); until the robot's track exists the backend says so and this retries
    const want = drive.status === "ready"
    useEffect(() => {
        setLive(false)
        if (!want) {
            setStatus("")
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
                const state = pc?.connectionState
                setStatus(
                    state === "connected"
                        ? "Video link up; waiting for the first frame…"
                        : `Video link: ${state}${state === "failed" ? " (retrying)" : ""}`,
                )
                if (pc?.connectionState === "failed" && !stopped) {
                    retry = setTimeout(open, 1500)
                }
            }
            setStatus("Opening the video link…")
            await pc.setLocalDescription(await pc.createOffer())
            await new Promise<void>((resolve) => {
                if (pc!.iceGatheringState === "complete") {
                    return resolve()
                }
                pc!.addEventListener("icegatheringstatechange", () => pc!.iceGatheringState === "complete" && resolve())
                setTimeout(resolve, 2000)
            })
            try {
                const answer = await call<{ sdp: string }>(
                    "POST",
                    "api/drive/video",
                    { sdp: pc.localDescription!.sdp },
                    { quiet: true },
                )
                if (!stopped) {
                    await pc.setRemoteDescription({ type: "answer", sdp: answer.sdp })
                }
            } catch (e) {
                if (!stopped) {
                    setStatus(`Video request failed: ${(e as Error).message} (retrying)`)
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
    const ws = useWsVideo(!!want, live && !!want, canvas)
    return { live: (live && !!want) || ws, status, ws }
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
    setBoost: (boost: number) => void
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
    const [optionsOpen, setOptionsOpen] = useState(false)
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
            .catch((e) => onToast(e.message))
            .finally(() => setBusy(false))
    }
    const seconds = record.active ? (Date.now() - record.startedAt) / 1000 : 0
    return (
        <div className="go2-record-control">
            <div className="rec-split">
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
                    {record.active
                        ? `Stop · ${clock(seconds)} · ${megabytes(record.bytes)}`
                        : busy
                        ? "Starting…"
                        : "Record"}
                </button>
                <button
                    type="button"
                    className="rec-more-btn dim-btn sm"
                    aria-label="Recording options"
                    title="Recording options"
                    aria-expanded={optionsOpen}
                    onClick={() => setOptionsOpen(!optionsOpen)}
                >
                    ⋯
                </button>
            </div>
            {optionsOpen && <RecordOptions record={record} onError={onToast} onClose={() => setOptionsOpen(false)} />}
        </div>
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
    /** shown in the top bar after the robot's name (the hotspot's way back) */
    topSlot?: React.ReactNode
}) {
    const { drive, commands, record, keyboardActive, flash, onFlash, onToast, onSignIn } = props
    const video = useRef<HTMLVideoElement>(null)
    const canvas = useRef<HTMLCanvasElement>(null)
    const [showControls, setShowControls] = useState<boolean>(() => stored(SHOW_CONTROLS_KEY, false))
    useEffect(() => store(SHOW_CONTROLS_KEY, showControls), [showControls])
    const { live, status: cameraStatus, ws } = useCamera(drive, video, canvas)
    const [pressed, setPressed] = useState<Set<string>>(new Set())
    const [boost, setBoost] = useState(0)
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
            call("POST", "api/drive/move", { ...vectorRef.current, boost: boostRef.current, durationMs: MOVE_HOLD_MS })
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
                setBoost(1)
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
                setBoost(0)
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
            setBoost(0)
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
            setBoost(0)
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
        ? cameraStatus || "Waiting for video…"
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
                <video
                    ref={video}
                    autoPlay
                    muted
                    playsInline
                    disablePictureInPicture
                    style={ws ? { display: "none" } : undefined}
                />
                <canvas ref={canvas} className="cam-canvas" style={ws ? undefined : { display: "none" }} />
                <div className="cam-ph">
                    <div className="glyph">
                        <Icon name="camera" size={40} />
                    </div>
                    <div>{placeholder}</div>
                </div>
                {/* three flex regions: who and how (left), the hotspot's way back (centered), Record and Disconnect (right) */}
                <div className="ctl-top">
                    <div className="ctl-top-side">
                        <div className="ctl-id">
                            <span className="nm">{drive.name}</span>
                            <span className="ctl-ip dim-mono">{drive.ip}</span>
                        </div>
                        {/* the link's state only when it isn't simply live (connecting, reconnecting, error) */}
                        {drive.status !== "ready" && (
                            <span className={`ctl-pill dim-badge ${PILL_TONE[drive.status] ?? ""}`}>
                                <span className="dot" />
                                {STATUS_LABEL[drive.status] ?? drive.status}
                            </span>
                        )}
                        {drive.dryRun && <span className="dim-badge warn">Dry run</span>}
                        <PadChip pad={pad} linked={ready} />
                    </div>
                    <div className="ctl-top-center">{props.topSlot}</div>
                    <div className="ctl-top-side end">
                        <RecordButton record={record} onToast={onToast} />
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
                </div>
                <ErrorNotice message={drive.status === "error" ? drive.error : null} />
                <div className={`vel dim-panel glass dim-mono${anyAxis || drive.moving ? " on" : ""}`}>
                    fwd {shown.forward.toFixed(2)} · str {shown.strafe.toFixed(2)} · yaw {shown.turn.toFixed(2)}
                    {boost ? `  ·  boost ${Math.round(boost * 100)}%` : "  ·  shift / LT = boost"}
                </div>
                {showControls && (
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
                )}
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
                {/* on a touchscreen the on-screen arrows are easy to hit by accident: hidden until asked for */}
                <button
                    type="button"
                    className="controls-toggle dim-btn sm"
                    title="Show or hide the on-screen arrow and turn buttons (keyboard and gamepad work either way)"
                    onClick={() => setShowControls((shown) => !shown)}
                >
                    <Icon name={showControls ? "close" : "gamepad"} size={13} />
                    {showControls ? "Hide controls" : "Show controls"}
                </button>
            </div>
        </div>
    )
}

function PadChip({ pad, linked }: { pad: GamepadStatus; linked: boolean }) {
    // a browser lists a pad only after a button press on this page (a reload hides it again until then)
    if (!pad.connected) {
        return (
            <span
                className="pad-chip dim-badge"
                title="No gamepad seen yet: press any gamepad button (a browser lists a pad only after a press on this page)"
            >
                <Icon name="gamepad" size={12} />
                No gamepad
            </span>
        )
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

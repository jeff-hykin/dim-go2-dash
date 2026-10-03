// The live-control stage: the robot's camera, its commands, and the d-pad / keyboard driver. Every press is an
// endpoint call (api/drive/*); the mode, status and last command shown come back from the backend, so a command the
// agent sends flashes here too.
import { useEffect, useRef, useState } from "react"
import { call } from "./api.ts"
import { Icon } from "./icons.tsx"
import type { Command, Drive } from "./state.ts"

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

export function Control(props: {
    drive: Active
    commands: Command[]
    keyboardActive: boolean
    flash: { name: string; ok: boolean; n: number } | null
    onFlash: (name: string, ok: boolean) => void
    onToast: (text: string) => void
    onSignIn: () => void
}) {
    const { drive, commands, keyboardActive, flash, onFlash, onToast, onSignIn } = props
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

    const vector = {
        forward: (pressed.has("w") ? 1 : 0) - (pressed.has("s") ? 1 : 0),
        strafe: (pressed.has("q") ? 1 : 0) - (pressed.has("e") ? 1 : 0),
        turn: (pressed.has("a") ? 1 : 0) - (pressed.has("d") ? 1 : 0),
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
                    <span className="spacer" />
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
            </div>
        </div>
    )
}

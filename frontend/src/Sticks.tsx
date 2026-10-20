// On-screen touch sticks for phones: MOVE (left: forward / strafe) and TURN (right: yaw), each its own finger, so
// both work at once. They hand Control the same -1..1 axes a gamepad does (padAxes), and spring back to 0 on release.
import { useEffect, useRef, useState } from "react"
import { type Axes, padAxes } from "./gamepad.ts"
import { stickOffset } from "./sticks.ts"

type Knob = { x: number; y: number }
const CENTRE: Knob = { x: 0, y: 0 }

function Stick({ label, axis, onChange, onGrab }: {
    label: string
    /** "both": a two-axis stick; "x": only side to side counts */
    axis: "both" | "x"
    onChange: (knob: Knob) => void
    /** a finger landed: false refuses it (the dog isn't standing) */
    onGrab: () => boolean
}) {
    const base = useRef<HTMLDivElement>(null)
    const finger = useRef<number | null>(null)
    const [knob, setKnob] = useState<Knob>(CENTRE)
    const [held, setHeld] = useState(false)
    const latest = useRef(onChange)
    latest.current = onChange
    const set = (next: Knob) => {
        setKnob(next)
        latest.current(next)
    }
    const release = () => {
        finger.current = null
        setHeld(false)
        set(CENTRE)
    }
    // non-passive: no page scroll, pinch zoom or long-press menu from a stick
    useEffect(() => {
        const el = base.current
        if (!el) {
            return
        }
        const stop = (e: Event) => e.preventDefault()
        el.addEventListener("touchstart", stop, { passive: false })
        el.addEventListener("touchmove", stop, { passive: false })
        el.addEventListener("contextmenu", stop)
        return () => {
            el.removeEventListener("touchstart", stop)
            el.removeEventListener("touchmove", stop)
            el.removeEventListener("contextmenu", stop)
        }
    }, [])
    // the page losing focus or going hidden lets go; so does unmounting
    useEffect(() => {
        const letGo = () => finger.current != null && release()
        const hidden = () => document.hidden && letGo()
        globalThis.addEventListener("blur", letGo)
        document.addEventListener("visibilitychange", hidden)
        return () => {
            globalThis.removeEventListener("blur", letGo)
            document.removeEventListener("visibilitychange", hidden)
            latest.current(CENTRE)
        }
    }, [])
    const follow = (e: React.PointerEvent) => {
        const el = base.current
        if (!el || finger.current !== e.pointerId) {
            return
        }
        const box = el.getBoundingClientRect()
        const offset = stickOffset(
            e.clientX - (box.left + box.width / 2),
            e.clientY - (box.top + box.height / 2),
            box.width / 2,
        )
        set(axis === "x" ? { x: offset.x, y: 0 } : offset)
    }
    const lift = (e: React.PointerEvent) => finger.current === e.pointerId && release()
    return (
        <div
            ref={base}
            className={`stick${held ? " held" : ""}`}
            data-ui={`control.stick_${label.toLowerCase()}`}
            onPointerDown={(e) => {
                if (finger.current != null || !onGrab()) {
                    return
                }
                e.preventDefault()
                finger.current = e.pointerId
                setHeld(true)
                e.currentTarget.setPointerCapture(e.pointerId)
                follow(e)
            }}
            onPointerMove={follow}
            onPointerUp={lift}
            onPointerCancel={lift}
            onLostPointerCapture={lift}
        >
            <div className={`stick-ring${axis === "x" ? " x-only" : ""}`} />
            <div
                className="stick-knob"
                style={{
                    transform:
                        `translate(calc(${knob.x} * var(--stick-travel)), calc(${knob.y} * var(--stick-travel)))`,
                }}
            />
            <span className="stick-label">{label}</span>
        </div>
    )
}

/** MOVE and TURN, with FAST (boost) between them. */
export function Sticks({ onAxes, onBoost, boost, onGrab }: {
    onAxes: (axes: Axes) => void
    onBoost: (on: boolean) => void
    boost: boolean
    onGrab: () => boolean
}) {
    const move = useRef<Knob>(CENTRE)
    const turn = useRef<Knob>(CENTRE)
    const update = () => onAxes(padAxes({ lx: move.current.x, ly: move.current.y, rx: turn.current.x }))
    return (
        <div className="sticks">
            <Stick
                label="MOVE"
                axis="both"
                onGrab={onGrab}
                onChange={(k) => {
                    move.current = k
                    update()
                }}
            />
            <button
                type="button"
                data-ui="control.fast"
                className={`stick-fast dim-btn sm${boost ? " on" : ""}`}
                aria-pressed={boost}
                title="Fast: up to 1.5× the speed (as shift or a gamepad trigger)"
                onClick={() => onBoost(!boost)}
            >
                FAST
            </button>
            <Stick
                label="TURN"
                axis="x"
                onGrab={onGrab}
                onChange={(k) => {
                    turn.current = k
                    update()
                }}
            />
        </div>
    )
}

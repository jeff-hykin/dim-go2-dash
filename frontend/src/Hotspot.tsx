import { ErrorNotice } from "./errors.tsx"
// AP mode: Go2s hosting their own Wi-Fi hotspot, as cards in the Robots panel; "Connect via hotspot" asks first (this
// computer leaves its Wi-Fi and loses its internet), then the backend switches the Wi-Fi and opens the drive session at
// 192.168.12.1 (backend/src/hotspot.rs). While linked, a banner offers the way back to the previous network.
import { useEffect, useState } from "react"
import { call } from "./api.ts"
import { Icon } from "./icons.tsx"
import type { HotspotState, Robot } from "./state.ts"

export type ScannedHotspot = { ssid: string; signal: number | null; security: string; current: boolean }
export type HotspotScan = { canScan: boolean; hotspots: ScannedHotspot[]; current: string | null; note?: string }

/** The confirmation, the password when it needs one, and the switch. */
function ConnectDialog({ ssid, state, robots, onClose }: {
    ssid: string
    state: HotspotState
    robots: Robot[]
    onClose: () => void
}) {
    const [password, setPassword] = useState("")
    const [robot, setRobot] = useState("")
    const [busy, setBusy] = useState(false)
    const [error, setError] = useState<string | null>(null)
    const [needsPassword, setNeedsPassword] = useState(!state.saved.includes(ssid))
    const leaving = state.current ?? "the Wi-Fi it's on"
    const go = () => {
        setBusy(true)
        setError(null)
        call("POST", "api/hotspot/connect", { ssid, password: password || undefined, robot: robot || undefined })
            .then(onClose, (e) => {
                setError(e.message)
                if (/password/i.test(e.message)) {
                    setNeedsPassword(true)
                }
            })
            .finally(() => setBusy(false))
    }
    return (
        <div className="rec-layer" onClick={busy ? undefined : onClose}>
            <div
                className="hs-dialog dim-panel"
                role="dialog"
                aria-label="Connect via hotspot"
                onClick={(e) => e.stopPropagation()}
            >
                <div className="hs-title">Connect via {ssid}?</div>
                <p>
                    Your computer will leave its current Wi-Fi (<b>{leaving}</b>) and join{" "}
                    <b>{ssid}</b>. You'll lose internet (uploads pause) but can control the dog. Switch back from here
                    anytime.
                </p>
                {needsPassword && (
                    <label className="hs-field">
                        Hotspot password (set in the Unitree app's AP mode; saved on this computer)
                        <input
                            className="dim-input"
                            type="password"
                            autoFocus
                            value={password}
                            onChange={(e) => setPassword(e.target.value)}
                            onKeyDown={(e) => e.key === "Enter" && go()}
                        />
                    </label>
                )}
                {robots.length > 0 && (
                    <label className="hs-field">
                        Which dog is it? (for its name and AES key)
                        <select className="dim-input" value={robot} onChange={(e) => setRobot(e.target.value)}>
                            <option value="">Not sure</option>
                            {robots.map((r) => <option key={r.key} value={r.key}>{r.name}</option>)}
                        </select>
                    </label>
                )}
                <ErrorNotice message={error} />
                <div className="hs-actions">
                    <button type="button" className="dim-btn" disabled={busy} onClick={onClose}>Cancel</button>
                    <button
                        type="button"
                        className="dim-btn primary"
                        disabled={busy || (needsPassword && password.length < 8)}
                        onClick={go}
                    >
                        {busy ? "Switching Wi-Fi…" : `Switch to ${ssid}`}
                    </button>
                </div>
            </div>
        </div>
    )
}

/** The Go2 hotspot cards (and, where Wi-Fi names can't be listed, a field for one). */
export function Hotspots({ scan, state, robots, onScan }: {
    scan: HotspotScan | null
    state: HotspotState
    robots: Robot[]
    onScan: () => void
}) {
    const [asking, setAsking] = useState<string | null>(null)
    const [typed, setTyped] = useState("")
    const linked = state.status === "linked" ? state.ssid : null
    return (
        <>
            {scan?.hotspots.map((h) => (
                <div className="dog hs-card" key={h.ssid}>
                    <div className="row">
                        <span className="av hs-av" title="A Go2 hosting its own Wi-Fi (AP mode)">
                            <Icon name="signal" size={14} />
                        </span>
                        <div className="meta">
                            <div className="nm">Go2 hotspot · {h.ssid}</div>
                            <div className="sub">
                                {h.signal != null ? `signal ${h.signal}%` : "signal ?"}
                                {h.security ? ` · ${h.security}` : " · open"}
                            </div>
                        </div>
                        <div className="acts">
                            {linked === h.ssid
                                ? <span className="dim-badge ok">connected</span>
                                : (
                                    <button type="button" className="dim-btn sm" onClick={() => setAsking(h.ssid)}>
                                        Connect via hotspot
                                    </button>
                                )}
                        </div>
                    </div>
                </div>
            ))}
            {scan && !scan.canScan && (
                <div className="hs-manual">
                    <div className="hs-note">{scan.note}</div>
                    <div className="hs-row">
                        <input
                            className="dim-input"
                            placeholder="Go2 hotspot name, e.g. GO2-A1B2C3"
                            value={typed}
                            onChange={(e) => setTyped(e.target.value)}
                            onKeyDown={(e) => e.key === "Enter" && typed.trim() && setAsking(typed.trim())}
                        />
                        <button
                            type="button"
                            className="dim-btn sm"
                            disabled={!typed.trim()}
                            onClick={() => setAsking(typed.trim())}
                        >
                            Connect
                        </button>
                    </div>
                </div>
            )}
            {scan && scan.canScan && scan.hotspots.length === 0 && (
                <div className="hs-note">
                    No Go2 hotspots nearby (AP mode).{" "}
                    <button type="button" className="rec-link" onClick={onScan}>Scan Wi-Fi again</button>
                </div>
            )}
            {asking && (
                <ConnectDialog
                    ssid={asking}
                    state={{ ...state, current: scan?.current ?? state.previous ?? null }}
                    robots={robots}
                    onClose={() => setAsking(null)}
                />
            )}
        </>
    )
}

/** While on a Go2's hotspot: say so (no internet) and offer the way back. */
export function HotspotBanner({ state }: { state: HotspotState }) {
    const [error, setError] = useState<string | null>(null)
    useEffect(() => setError(null), [state.status])
    if (state.status === "idle") {
        return null
    }
    const back = () => call("POST", "api/hotspot/restore").catch((e) => setError(e.message))
    const text = state.status === "joining"
        ? `Switching this computer's Wi-Fi to ${state.ssid}…`
        : state.status === "restoring"
        ? `Switching back to ${state.previous}…`
        : state.status === "error"
        ? "Hotspot connection failed"
        : `Connected through ${state.ssid} (no internet)`
    return (
        <div className={`hs-banner dim-alert ${state.status === "error" ? "danger" : "warn"}`} role="status">
            <Icon name="signal" size={14} />
            <span>{text}</span>
            <ErrorNotice message={error ?? (state.status === "error" ? state.error : null)} />
            {(state.status === "linked" || state.status === "error") && (
                <button type="button" className="dim-btn sm" onClick={back}>
                    {state.previous ? `Switch back to ${state.previous}` : "Leave the hotspot"}
                </button>
            )}
        </div>
    )
}

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
export type HotspotAsk = { ssid: string; robot?: string }

/** The dog's name in its hotspot's SSID: "Go2_60968_83d1a1fa" → "Go2_60968" (G1s too); null for other names. */
export function dogNameFromSsid(ssid: string): string | null {
    const found = /^(go2|g1)[_-]([0-9a-z]{3,})/i.exec(ssid)
    return found ? `${found[1].toLowerCase() === "g1" ? "G1" : "Go2"}_${found[2]}` : null
}

/** The dog a hotspot belongs to: a Go2's hotspot is named after its Bluetooth name (Go2_60968 → Go2_60968_83d1a1fa). */
export function robotForHotspot(ssid: string, robots: Robot[]): Robot | undefined {
    const name = ssid.toLowerCase()
    return robots.find((r) => {
        const ble = r.bleName?.toLowerCase()
        return !!ble && (name === ble || name.startsWith(`${ble}_`))
    })
}

/** The confirmation, the password when it needs one, and the switch. */
export function ConnectDialog({ ssid, robot: forRobot, state, robots, onClose }: {
    ssid: string
    robot?: string
    state: HotspotState
    robots: Robot[]
    onClose: () => void
}) {
    const [password, setPassword] = useState("")
    const [dogTyped, setDogTyped] = useState(robots.find((r) => r.key === forRobot)?.name ?? "")
    const [busy, setBusy] = useState(false)
    const [error, setError] = useState<string | null>(null)
    const [needsPassword, setNeedsPassword] = useState(!state.saved.includes(ssid))
    const leaving = state.current ?? "the Wi-Fi it's on"
    // a Go2's hotspot names its dog: no need to ask which one
    const dogName = dogNameFromSsid(ssid)
    const go = () => {
        setBusy(true)
        setError(null)
        const typed = dogTyped.trim()
        const robot = forRobot ?? robots.find((r) => r.name === typed || r.bleName === typed)?.key
        call("POST", "api/hotspot/connect", {
            ssid,
            password: password || undefined,
            robot: robot || undefined,
            name: (!dogName && typed) || undefined,
        })
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
                        Hotspot password (empty: Unitree's default, 12345678; whichever works is saved on this computer)
                        <input
                            className="dim-input"
                            type="password"
                            placeholder="12345678"
                            value={password}
                            onChange={(e) => setPassword(e.target.value)}
                            onKeyDown={(e) => e.key === "Enter" && go()}
                        />
                    </label>
                )}
                {dogName && (
                    <div className="hs-field">
                        Dog: <b>{dogName}</b> (from its Wi-Fi name; its AES key comes from the saved keys)
                    </div>
                )}
                {!dogName && (
                    <label className="hs-field">
                        Which dog is it? (its name finds its AES key; leave empty if unsure)
                        <input
                            className="dim-input"
                            list="hs-dogs"
                            placeholder="e.g. Go2_60968"
                            value={dogTyped}
                            onChange={(e) => setDogTyped(e.target.value)}
                            onKeyDown={(e) => e.key === "Enter" && go()}
                        />
                        <datalist id="hs-dogs">
                            {robots.map((r) => <option key={r.key} value={r.name} />)}
                        </datalist>
                    </label>
                )}
                <ErrorNotice message={error} />
                <div className="hs-actions">
                    <button type="button" className="dim-btn" disabled={busy} onClick={onClose}>Cancel</button>
                    <button
                        type="button"
                        className="dim-btn primary"
                        disabled={busy || (password.length > 0 && password.length < 8)}
                        onClick={go}
                    >
                        {busy ? "Switching Wi-Fi…" : `Switch to ${ssid}`}
                    </button>
                </div>
            </div>
        </div>
    )
}

/** The Go2 hotspot cards not shown on a known dog's card (and, where Wi-Fi names can't be listed, a field for one). */
export function Hotspots({ scan, state, robots, onScan, onAsk }: {
    scan: HotspotScan | null
    state: HotspotState
    robots: Robot[]
    onScan: () => void
    onAsk: (ask: HotspotAsk) => void
}) {
    const [typed, setTyped] = useState("")
    const linked = state.status === "linked" ? state.ssid : null
    const setAsking = (ssid: string) => onAsk({ ssid })
    const unclaimed = scan?.hotspots.filter((h) => !robotForHotspot(h.ssid, robots)) ?? []
    return (
        <>
            {unclaimed.map((h) => (
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
                    {scan.note ? `Wi-Fi scan failed: ${scan.note}.` : "No Go2 hotspots nearby (AP mode)."}{" "}
                    <button type="button" className="rec-link" onClick={onScan}>Scan Wi-Fi again</button>
                </div>
            )}
        </>
    )
}

/** While on a Go2's hotspot: say so (no internet) and offer the way back. */
export function HotspotBanner({ state, besidePanel }: { state: HotspotState; besidePanel?: boolean }) {
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
        <div
            className={`hs-banner dim-alert ${state.status === "error" ? "danger" : "warn"}${
                besidePanel ? " beside-panel" : ""
            }`}
            role="status"
        >
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

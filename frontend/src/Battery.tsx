// The batteries that end a session: the dog's (charge, volts, amps, warmest cell) and this computer's (the deck or
// laptop driving it), as one pane in the Control view. Tone follows the lower of the two.
import type { Drive, HostState } from "./state.ts"

type Active = Extract<Drive, { active: true }>

const LOW = 20
const WARN = 35

export function tone(percent: number | null | undefined): "ok" | "warn" | "danger" | "unknown" {
    return percent == null ? "unknown" : percent <= LOW ? "danger" : percent <= WARN ? "warn" : "ok"
}

/** "charging · 11.4 W", "on battery · 8 W", "plugged in", "full" */
export function hostDetail(host: HostState["battery"]): string {
    if (!host) {
        return "no battery"
    }
    const watts = host.watts != null ? ` · ${host.watts} W` : ""
    switch (host.status) {
        case "charging":
            return `charging${watts}`
        case "full":
            return "full"
        case "discharging":
            return `on battery${watts}`
        default:
            return host.plugged ? "plugged in" : "on battery"
    }
}

/** "28.4 V · 1.2 A out · 31 °C" from the dog's lowstate */
export function dogDetail(power: Active["power"]): string {
    if (!power) {
        return "waiting for the robot"
    }
    const parts: string[] = []
    if (power.voltage != null) {
        parts.push(`${power.voltage.toFixed(1)} V`)
    }
    if (power.current != null) {
        parts.push(`${Math.abs(power.current).toFixed(1)} A ${power.current < 0 ? "out" : "in"}`)
    }
    if (power.temperature != null) {
        parts.push(`${Math.round(power.temperature)} °C`)
    }
    return parts.join(" · ") || "—"
}

function Row({ label, percent, detail, charging }: { label: string; percent: number | null | undefined; detail: string; charging?: boolean }) {
    const t = tone(percent)
    return (
        <div className={`bt-row ${t}`} data-testid="battery-row">
            <span className="bt-name">{label}</span>
            <span className="bt-pct dim-mono">{percent == null ? "—" : `${percent}%`}{charging ? " ⚡" : ""}</span>
            <div className="bt-bar">
                <div className={`bt-fill ${t}`} style={{ width: `${Math.max(0, Math.min(100, percent ?? 0))}%` }} />
            </div>
            <span className="bt-detail">{detail}</span>
        </div>
    )
}

export function BatteryPane({ drive, host }: { drive: Active; host: HostState }) {
    const low = Math.min(drive.battery ?? 100, host.battery?.percent ?? 100)
    return (
        <div className={`batt-pane dim-panel glass ${tone(low)}`} data-testid="battery-pane" title="Batteries: the dog's and this computer's">
            <Row label={drive.name} percent={drive.battery} detail={dogDetail(drive.power)} charging={(drive.power?.current ?? 0) > 0} />
            <Row
                label={host.name ?? "This computer"}
                percent={host.battery?.percent}
                detail={hostDetail(host.battery)}
                charging={host.battery?.status === "charging"}
            />
            {low <= LOW && <div className="bt-low">Battery low: bring the dog back</div>}
        </div>
    )
}

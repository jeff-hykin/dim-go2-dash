// The Go2 Setup panel: drive a known IP, scan, the list of nearby dogs (rename, details, AES key, Wi-Fi), Unitree
// accounts and the "never gets an IP?" help. Every action is an endpoint call; the list itself is the backend's.
import { forwardRef, useEffect, useRef, useState } from "react"
import { call } from "./api.ts"
import { runCommand } from "./dim-app/source/shell.js"
import { Icon } from "./icons.tsx"
import type { Account, Drive, Network, Robot, Scan, Sweep, Wifi } from "./state.ts"
import { agoText, colorFor, copyText, store, stored, validIp } from "./util.ts"

const WIFI_LS = "go2dash.wifi" // last Wi-Fi that worked, to prefill the next dog's form
const LAST_IP_LS = "go2dash.lastManualIp"

type SavedWifi = { ssid?: string; password?: string; country?: string }

function Copyable(
    { value, className, children, title }: {
        value: string
        className: string
        children?: React.ReactNode
        title?: string
    },
) {
    const [copied, setCopied] = useState(false)
    return (
        <span
            className={`${className}${copied ? " copied" : ""}`}
            title={title ?? "Click to copy"}
            onClick={(e) => {
                e.stopPropagation()
                copyText(value).then((ok) => {
                    if (ok) {
                        setCopied(true)
                        setTimeout(() => setCopied(false), 1100)
                    }
                })
            }}
        >
            {children ?? value}
            <span className="cp">
                <Icon name={copied ? "check" : "copy"} size={11} />
            </span>
        </span>
    )
}

function DetailRow({ label, value }: { label: string; value: string | null }) {
    if (!value) {
        return null
    }
    return (
        <div className="d-row">
            <span className="d-k">{label}</span>
            <Copyable className="d-v copyable" value={value} />
        </div>
    )
}

/** An input that saves (PUT) on Enter or blur, and shows the backend's complaint by turning red. */
function SavedInput(
    props: {
        path: string
        field: string
        value: string
        placeholder: string
        maxLength?: number
        inputMode?: "decimal"
    },
) {
    const [text, setText] = useState(props.value)
    const [bad, setBad] = useState(false)
    useEffect(() => setText(props.value), [props.value])
    const commit = () => {
        if (text.trim() === props.value) {
            return
        }
        call("PUT", props.path, { [props.field]: text.trim() }).then(() => setBad(false), () => setBad(true))
    }
    return (
        <input
            className={`dim-input${bad ? " bad" : ""}`}
            value={text}
            placeholder={props.placeholder}
            maxLength={props.maxLength}
            spellCheck={false}
            autoComplete="off"
            inputMode={props.inputMode}
            onChange={(e) => setText(e.target.value)}
            onBlur={commit}
            onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
        />
    )
}

function WifiForm(props: { robot: Robot; wifi: Wifi; network: Network; onClose: () => void }) {
    const { robot, wifi, network, onClose } = props
    const saved = stored<SavedWifi>(WIFI_LS, {})
    // prefill: this computer's network (so the dog lands where we can reach it), else the last one used
    const initialSsid = (network.ssidStatus === "ok" ? network.ssid : "") || saved.ssid || ""
    const [ssid, setSsid] = useState(initialSsid)
    const [password, setPassword] = useState(saved.ssid && saved.ssid === initialSsid ? saved.password ?? "" : "")
    const [country, setCountry] = useState(saved.country || "US")
    const [error, setError] = useState<string | null>(null)
    const mine = wifi.robot === robot.key
    const busy = mine && wifi.status === "running"
    const canConnect = robot.canProvisionWifi
    const mismatch = network.ssidStatus === "ok" && network.ssid && ssid.trim() && ssid.trim() !== network.ssid
        ? `⚠ This computer is on “${network.ssid}”, not “${ssid.trim()}”. A dog put on “${ssid.trim()}” won't be reachable or get an IP here unless you also move this machine onto “${ssid.trim()}”.`
        : ""
    const connect = () => {
        if (!ssid.trim()) {
            setError("Enter a wifi SSID first.")
            return
        }
        setError(null)
        const attempt = { ssid: ssid.trim(), password, country: (country.trim() || "US").toUpperCase() }
        call("POST", `api/robots/${encodeURIComponent(robot.key)}/wifi`, attempt).then(
            () => store(WIFI_LS, attempt), // remember working creds for the next dog
            (e) => setError(e.message === "cancelled" ? null : e.message),
        )
    }
    const cancel = () => {
        if (busy) {
            call("POST", "api/wifi/cancel").catch(() => {})
        } else {
            if (mine) {
                call("DELETE", "api/wifi").catch(() => {})
            }
            onClose()
        }
    }
    const log = mine ? wifi.log ?? [] : []
    return (
        <div className="form" onClick={(e) => e.stopPropagation()}>
            {!canConnect && (
                <div className="log err">LAN-only sighting — no Bluetooth address to provision wifi over.</div>
            )}
            <div className="fld">
                <label className="dim-label">Wifi SSID — connect the dog to a network</label>
                <input
                    className="f-ssid dim-input"
                    placeholder="network name"
                    value={ssid}
                    disabled={!canConnect}
                    onChange={(e) => setSsid(e.target.value)}
                />
            </div>
            <div className={`f-warn dim-alert danger${mismatch ? " show" : ""}`}>{mismatch}</div>
            {network.ssidStatus === "redacted" && (
                <div className="f-hint">
                    macOS hid this Mac's Wi-Fi name (grant the dashboard Location Services to enable the same-network
                    check).
                </div>
            )}
            <div className="two">
                <div className="fld">
                    <label className="dim-label">Password</label>
                    <input
                        className="f-pass dim-input"
                        type="password"
                        placeholder="••••••"
                        value={password}
                        disabled={!canConnect}
                        onChange={(e) => setPassword(e.target.value)}
                    />
                </div>
                <div className="fld" style={{ flex: "0 0 76px" }}>
                    <label className="dim-label">Country</label>
                    <input
                        className="f-country dim-input"
                        value={country}
                        maxLength={2}
                        disabled={!canConnect}
                        onChange={(e) => setCountry(e.target.value)}
                    />
                </div>
            </div>
            <div className="actions">
                <button
                    type="button"
                    className="dim-btn primary sm f-go"
                    disabled={!canConnect || busy}
                    onClick={connect}
                >
                    {busy ? "Connecting…" : (
                        <>
                            <Icon name="signal" size={13} />
                            Connect
                        </>
                    )}
                </button>
                <button type="button" className="dim-btn sm f-cancel" onClick={cancel}>Cancel</button>
            </div>
            <div className="log f-log">
                {log.map((line, i) => (
                    <div key={i} className={line.startsWith("✓") ? "ok" : line === "Cancelled." ? "err" : ""}>
                        {line}
                    </div>
                ))}
                {mine && wifi.status === "error" && <div className="err">✗ {wifi.error}</div>}
                {error && <div className="err">{error}</div>}
            </div>
        </div>
    )
}

/** How a dog's IP was found, as a badge: the stronger the join, the calmer the tone. */
const MATCHED: Record<Robot["matched"], { label: string; tone: string; title: string }> = {
    "ble+arp": {
        label: "BLE + ARP",
        tone: "ok",
        title: "Its Wi-Fi MAC (its Bluetooth MAC, last byte − 1) is at this IP in the ARP table",
    },
    "ble+lan": {
        label: "BLE + LAN",
        tone: "ok",
        title: "Seen on Bluetooth and answered LAN discovery with its serial",
    },
    lan: { label: "LAN", tone: "ok", title: "Answered LAN discovery with its serial" },
    oui: {
        label: "OUI only",
        tone: "warn",
        title: "A Unitree MAC with a Go2 port open: best effort, no serial to tell which dog",
    },
    guess: {
        label: "guess",
        tone: "warn",
        title: "The only Go2 on Bluetooth and the only unclaimed Unitree MAC on the network (macOS hides Bluetooth " +
            "MACs, so they can't be joined): probably the same dog",
    },
    ble: { label: "BLE only", tone: "", title: "Seen on Bluetooth, not on this network" },
}

function MatchBadge({ robot }: { robot: Robot }) {
    const m = MATCHED[robot.matched] ?? MATCHED.ble
    return <span className={`match-tag dim-badge ${m.tone}`} title={m.title}>{m.label}</span>
}

/** Pick this dog and its IP as the one the app uses (the guide's robot: launches and Desktop's robot_ip follow it). */
function UseIp({ robot, using }: { robot: Robot; using: boolean }) {
    if (!robot.ip) {
        return null
    }
    if (using) {
        return <span className="use-ip using dim-badge ok" title="The robot IP this app uses">in use</span>
    }
    return (
        <button
            type="button"
            className="use-ip dim-btn ghost sm"
            title={`Use ${robot.ip} as the robot IP (picks this dog; connects to nothing)`}
            onClick={(e) => {
                e.stopPropagation()
                call("PUT", "api/setup", { robot: robot.key, ip: robot.ip }).catch(() => {})
            }}
        >
            Use this IP
        </button>
    )
}

/** The ARP sweep: progress while it runs (with Stop), then what it covered, and "sweep everything" when it didn't. */
export function SweepStatus(
    { sweep, scanning, onFullSweep }: { sweep: Sweep | null | undefined; scanning: boolean; onFullSweep: () => void },
) {
    if (!sweep) {
        return null
    }
    if (sweep.status === "error") {
        return <div className="sweep err">ARP sweep: {sweep.error}</div>
    }
    const where = `${sweep.iface} · ${sweep.subnet}`
    if (sweep.status === "running") {
        const pct = sweep.total ? Math.round(100 * (sweep.sent ?? 0) / sweep.total) : 0
        const what = sweep.phase === "known" ? "Re-pinging where dogs were seen" : `Sweeping ${sweep.swept}`
        return (
            <div className="sweep running" data-testid="sweep">
                <div className="sweep-row">
                    <span>
                        {what} — {(sweep.sent ?? 0).toLocaleString()}/{(sweep.total ?? 0).toLocaleString()} ·{" "}
                        {sweep.alive ?? 0} answered
                    </span>
                    <button
                        type="button"
                        className="dim-btn ghost sm"
                        onClick={() => call("POST", "api/scan/stop").catch(() => {})}
                    >
                        Stop
                    </button>
                </div>
                <div className="sweep-bar">
                    <div style={{ width: `${pct}%` }} />
                </div>
                <div className="sweep-sub">{where}</div>
            </div>
        )
    }
    return (
        <div className="sweep" data-testid="sweep">
            <div className="sweep-row">
                <span className="sweep-sub">
                    ARP: {sweep.note}
                    {sweep.method ? ` (${sweep.method})` : ""} · {where}
                </span>
                {sweep.partial && !scanning && (
                    <button
                        type="button"
                        className="dim-btn sm"
                        title="Ping every address of this network, so a dog anywhere on it shows up (read-only)"
                        onClick={onFullSweep}
                    >
                        Sweep all {(sweep.fullHosts ?? 0).toLocaleString()}
                    </button>
                )}
            </div>
        </div>
    )
}

export const RobotCard = forwardRef<HTMLDivElement, {
    robot: Robot
    using: boolean
    drive: Drive
    wifi: Wifi
    network: Network
    awaitingIp: boolean
    open: { menu: boolean; form: boolean; details: boolean; edit: boolean }
    setOpen: (what: "menu" | "form" | "details" | "edit" | null) => void
    onOpenAccounts: () => void
}>(function RobotCard(props, ref) {
    const { robot, using, drive, wifi, network, awaitingIp, open, setOpen, onOpenAccounts } = props
    const key = robot.key
    const driving = drive.active && drive.robot === key
    if (robot.arpOnly) {
        return (
            <div className="dog arp" data-key={key} ref={ref}>
                <div className="row">
                    <span className="av arp-av" title="Found via ARP — unverified">
                        <Icon name="warn" size={14} />
                    </span>
                    <div className="meta">
                        <div className="nm">
                            Possible Go2 <MatchBadge robot={robot} />
                        </div>
                        <div className="sub">
                            {robot.ip && <Copyable className="ip" value={robot.ip} title="Click to copy IP" />}
                            {robot.lanMac && <span className="serial">{robot.lanMac}</span>}
                        </div>
                    </div>
                    <UseIp robot={robot} using={using} />
                </div>
            </div>
        )
    }
    const stale = robot.ipSource === "remembered"
    const classes = ["dog"]
    if (stale) {
        classes.push("cached")
    }
    if (open.menu) {
        classes.push("menu-open", "active")
    }
    if (open.form) {
        classes.push("form-open", "active")
    }
    if (open.details) {
        classes.push("details-open", "active")
    }
    if (driving) {
        classes.push("driving", "active")
    }
    const rename = (name: string) => {
        setOpen(null)
        if (name.trim() !== (robot.customName ?? robot.name)) {
            call("PUT", `api/robots/${encodeURIComponent(key)}/name`, { name: name.trim() }).catch(() => {})
        }
    }
    const toggleDrive = () => {
        setOpen(null)
        if (driving) {
            call("POST", "api/drive/disconnect").catch(() => {})
        } else {
            call("POST", "api/drive/connect", { robot: key }).catch(() => {}) // the stage shows the error
        }
    }
    return (
        <div className={classes.join(" ")} data-key={key} ref={ref}>
            <div className="row">
                <span className="av" style={{ background: colorFor(robot.bleId || robot.serial || key) }} />
                <div className="meta">
                    {open.edit
                        ? (
                            <input
                                className="rename dim-input"
                                defaultValue={robot.name}
                                autoFocus
                                onFocus={(e) => e.currentTarget.select()}
                                onBlur={(e) => rename(e.currentTarget.value)}
                                onKeyDown={(e) => {
                                    if (e.key === "Enter") {
                                        e.currentTarget.blur()
                                    } else if (e.key === "Escape") {
                                        setOpen(null)
                                    }
                                }}
                            />
                        )
                        : (
                            <div className="nm">
                                {robot.name} <MatchBadge robot={robot} />
                            </div>
                        )}
                    <div className="sub">
                        {robot.ip && (
                            <Copyable
                                className={`ip${stale ? " cached" : ""}`}
                                value={robot.ip}
                                title={stale ? "Last known IP — waiting for fresh scan" : "Click to copy IP"}
                            />
                        )}
                        {!robot.ip && awaitingIp && (
                            <span className="waiting">
                                <span className="wd" />
                                waiting for IP…
                            </span>
                        )}
                        {robot.serial && <span className="serial">{robot.serial}</span>}
                        {!robot.ip && !awaitingIp && !robot.serial && "—"}
                    </div>
                </div>
                <UseIp robot={robot} using={using} />
                {robot.ip && (
                    <button
                        type="button"
                        className={`drive dim-btn sm icon${driving ? " primary" : ""}`}
                        title="Drive this dog live (camera + keyboard control)"
                        onClick={toggleDrive}
                    >
                        {driving ? "Disconnect" : (
                            <>
                                <Icon name="gamepad" size={14} />
                                Drive
                            </>
                        )}
                    </button>
                )}
                <button
                    type="button"
                    className="rowbtn kebab dim-btn ghost icon"
                    title="More"
                    onClick={() => setOpen(open.menu ? null : "menu")}
                >
                    <Icon name="more-horizontal" size={15} />
                </button>
            </div>
            {open.details && (
                <div className="details">
                    <DetailRow label="Bluetooth name" value={robot.bleName} />
                    <DetailRow label="Serial" value={robot.serial} />
                    <DetailRow label="Bluetooth ID" value={robot.bleId} />
                    {robot.ipSource === "scan"
                        ? <DetailRow label="IP address" value={robot.ip} />
                        : (
                            <div className="d-row">
                                <span className="d-k">IP address</span>
                                <SavedInput
                                    path={`api/robots/${encodeURIComponent(key)}/ip`}
                                    field="ip"
                                    value={robot.ip ?? ""}
                                    placeholder="not seen on this network — type it to drive anyway"
                                    inputMode="decimal"
                                />
                            </div>
                        )}
                    <DetailRow label="Wi-Fi MAC" value={robot.lanMac} />
                    <DetailRow label="Bluetooth MAC" value={robot.bleMac} />
                    <div className="d-row">
                        <span className="d-k">AES key</span>
                        <SavedInput
                            path={`api/robots/${encodeURIComponent(key)}/aes-key`}
                            field="aesKey"
                            value=""
                            placeholder={robot.hasAesKey
                                ? "saved — type to replace"
                                : "32 hex chars — needed on firmware ≥ 1.1.15"}
                            maxLength={32}
                        />
                    </div>
                    <button type="button" className="dim-btn sm mini d-fetch" onClick={onOpenAccounts}>
                        Fetch key from Unitree…
                    </button>
                </div>
            )}
            <div className="menu">
                <button type="button" className="mi-details" onClick={() => setOpen(open.details ? null : "details")}>
                    <span className="mi-ic">
                        <Icon name="info" size={13} />
                    </span>
                    Details
                </button>
                <button type="button" className="mi-rename" onClick={() => setOpen("edit")}>
                    <span className="mi-ic">
                        <Icon name="edit" size={13} />
                    </span>
                    Rename
                </button>
                <button
                    type="button"
                    className="mi-connect"
                    disabled={!robot.canProvisionWifi}
                    style={robot.canProvisionWifi ? undefined : { opacity: 0.45, cursor: "default" }}
                    onClick={() => setOpen("form")}
                >
                    <Icon name="signal" size={13} />
                    Connect to Wi-Fi…
                </button>
            </div>
            {open.form && <WifiForm robot={robot} wifi={wifi} network={network} onClose={() => setOpen(null)} />}
        </div>
    )
})

/** Save the AES keys this computer already has as a .json file (built in the page; the keys go nowhere else). */
async function downloadAesKeys(): Promise<string | null> {
    try {
        const data = await call<{ keys: unknown[] }>("GET", "api/aes-keys")
        if (!data.keys.length) {
            return "No AES keys saved yet."
        }
        const blob = new Blob([JSON.stringify(data, null, 4)], { type: "application/json" })
        const url = URL.createObjectURL(blob)
        const link = document.createElement("a")
        link.href = url
        link.download = `go2-aes-keys-${new Date().toISOString().slice(0, 10)}.json`
        document.body.appendChild(link)
        link.click()
        link.remove()
        setTimeout(() => URL.revokeObjectURL(url), 1000)
        return null
    } catch (error) {
        return `Couldn't read the keys: ${error instanceof Error ? error.message : error}`
    }
}

export function Accounts(
    { accounts, open, setOpen, flash }: {
        accounts: Account[]
        open: boolean
        setOpen: (open: boolean) => void
        flash: number
    },
) {
    const [email, setEmail] = useState("")
    const [password, setPassword] = useState("")
    const [error, setError] = useState<string | null>(null)
    const root = useRef<HTMLDivElement>(null)
    const emailInput = useRef<HTMLInputElement>(null)
    useEffect(() => {
        if (!flash || !root.current) {
            return
        }
        root.current.classList.remove("flash")
        void root.current.offsetWidth
        root.current.classList.add("flash")
        root.current.scrollIntoView({ block: "nearest", behavior: "smooth" })
        const pull = root.current.querySelector<HTMLButtonElement>(".a-pull")
        ;(accounts.length && pull ? pull : emailInput.current)?.focus()
    }, [flash])
    const keyed = new Set(accounts.flatMap((a) => a.robots.filter((r) => r.hasKey).map((r) => r.sn)))
    const add = () => {
        if (!email.trim() || !password) {
            return
        }
        setError(null)
        call("POST", "api/accounts", { email: email.trim(), password }).catch((e) => setError(e.message))
        setEmail("")
        setPassword("")
    }
    const onEnter = (e: React.KeyboardEvent) => {
        if (e.key === "Enter") {
            e.preventDefault()
            add()
        }
    }
    return (
        <div className={`p-accounts${open ? " open" : ""}`} ref={root}>
            <button type="button" className="help-toggle dim-btn ghost sm" onClick={() => setOpen(!open)}>
                <span className="hc">
                    <Icon name="chevron-right" size={12} />
                </span>
                <span>
                    {accounts.length
                        ? `Unitree accounts (${accounts.length}) · ${keyed.size} AES key${keyed.size === 1 ? "" : "s"}`
                        : "Unitree accounts (AES keys)"}
                </span>
            </button>
            <div className="acct-body">
                <div className="acct-hint">
                    Dogs on firmware ≥ 1.1.15 need a per-device AES key, which only Unitree's cloud hands out. Sign in
                    with the Go2/G1 app account each dog is paired to; <b>Pull</b>{" "}
                    tries every region and saves the key of every robot bound to it.
                </div>
                <div>
                    {accounts.length === 0 && (
                        <div className="acct-empty">No account yet — add the one your dogs are paired to.</div>
                    )}
                    {accounts.map((a) => {
                        const keyedHere = a.robots.filter((r) => r.hasKey).length
                        const sub = a.pulling
                            ? "Signing in and pulling keys…"
                            : a.error
                            ? `✗ ${a.error}`
                            : a.lastPull
                            ? `${keyedHere} key${keyedHere === 1 ? "" : "s"} · ${a.robots.length} robot${
                                a.robots.length === 1 ? "" : "s"
                            } bound · pulled ${agoText(a.lastPull)}`
                            : "Never pulled"
                        const names = a.robots.map((r) =>
                            (r.alias ? `${r.alias} (${r.sn})` : r.sn) + (r.hasKey ? "" : " — no key (old firmware)")
                        ).join("\n")
                        const path = `api/accounts/${encodeURIComponent(a.email)}`
                        return (
                            <div className="acct" key={a.email}>
                                <div className="a-main">
                                    <div className="a-email">{a.email}</div>
                                    <div className={`a-sub${a.error && !a.pulling ? " err" : ""}`} title={names}>
                                        {sub}
                                    </div>
                                </div>
                                <button
                                    type="button"
                                    className="dim-btn sm a-pull"
                                    disabled={a.pulling}
                                    onClick={() => call("POST", `${path}/pull`).catch(() => {})}
                                >
                                    {a.pulling ? "Pulling…" : "Pull"}
                                </button>
                                <button
                                    type="button"
                                    className="a-rm dim-btn ghost icon"
                                    title="Forget this account (keys already pulled stay)"
                                    onClick={() => call("DELETE", path).catch(() => {})}
                                >
                                    <Icon name="close" size={13} />
                                </button>
                            </div>
                        )
                    })}
                </div>
                <div className="form cloud">
                    <div className="fld">
                        <label className="dim-label">Unitree account email</label>
                        <input
                            ref={emailInput}
                            className="c-email dim-input"
                            autoComplete="off"
                            spellCheck={false}
                            placeholder="you@example.com"
                            value={email}
                            onChange={(e) => setEmail(e.target.value)}
                            onKeyDown={onEnter}
                        />
                    </div>
                    <div className="fld">
                        <label className="dim-label">Password</label>
                        <input
                            className="c-pass dim-input"
                            type="password"
                            placeholder="••••••"
                            value={password}
                            onChange={(e) => setPassword(e.target.value)}
                            onKeyDown={onEnter}
                        />
                    </div>
                    <div className="actions">
                        <button type="button" className="dim-btn primary sm" onClick={add}>Add &amp; pull</button>
                        <span className="acct-hint" style={{ margin: 0 }}>Saved on this computer only.</span>
                        <button
                            type="button"
                            className="dim-btn ghost sm a-download"
                            style={{ marginLeft: "auto" }}
                            title="Download the AES keys saved on this computer (name, serial, key) as a .json file"
                            onClick={() => downloadAesKeys().then(setError)}
                        >
                            <Icon name="download" size={13} />
                            Download keys
                        </button>
                    </div>
                    {error && <div className="acct-hint" style={{ color: "var(--danger)" }}>{error}</div>}
                </div>
            </div>
        </div>
    )
}

/** `openRequest`: each new value opens it and scrolls it into view (the stage's "Help" button). */
export function Help({ route, openRequest = 0 }: { route: string; openRequest?: number }) {
    const [open, setOpen] = useState(false)
    // the route needs sudo: Desktop runs it (the user presses Run and types the password in its terminal)
    const [fixing, setFixing] = useState<string | null>(null)
    const fix = async () => {
        setFixing("waiting for you to press Run in Desktop…")
        try {
            const result = await runCommand(route, {
                title: "Route Go2 discovery over Wi-Fi",
                note: "Send the Go2 discovery probe out the Wi-Fi interface (sudo)",
                message: "A VPN took the route LAN discovery needs, so the probe never reaches the dog. This adds " +
                    "a route for the Go2 discovery group (231.1.1.1) through your Wi-Fi. It asks for your password.",
            })
            setFixing(
                result.status === "succeeded"
                    ? "Done: press Scan again."
                    : result.status === "unavailable"
                    ? "Only inside dimOS Desktop: copy the command and run it in a terminal."
                    : `Not done (${result.reason ?? result.status}).`,
            )
        } catch (error) {
            setFixing(`Couldn't ask Desktop: ${error instanceof Error ? error.message : error}`)
        }
    }
    const box = useRef<HTMLDivElement>(null)
    useEffect(() => {
        if (openRequest) {
            setOpen(true)
            box.current?.scrollIntoView({ block: "nearest", behavior: "smooth" })
        }
    }, [openRequest])
    return (
        <div ref={box} className={`p-help${open ? " open" : ""}`}>
            <button type="button" className="help-toggle dim-btn ghost sm" onClick={() => setOpen(!open)}>
                <span className="hc">
                    <Icon name="chevron-right" size={12} />
                </span>
                A dog shows but never gets an IP?
            </button>
            <div className="help-body">
                <p>
                    <b>Try disabling your VPN (including tailscale) if you have one.</b>{" "}
                    Note: once you know the IP you can connect manually without disabling your VPN, its just needed for
                    scanning.
                </p>
                <p>
                    A Go2 is found over <b>Bluetooth</b>, but its IP only appears once it's discovered on your{" "}
                    <b>network</b>. If the IP never shows:
                </p>
                <ul>
                    <li>
                        Make sure the dog joined the <b>same Wi-Fi</b> as this computer.
                    </li>
                    <li>
                        On macOS a <b>VPN</b> can block LAN discovery. If yours <b>hijacks the route</b>{" "}
                        (Tailscale &amp; other tun VPNs), route the Go2 discovery group out your Wi-Fi:
                    </li>
                </ul>
                <Copyable className="help-cmd" value={route}>{route}</Copyable>
                <div className="help-fix">
                    <button type="button" className="dim-btn sm" onClick={fix} data-testid="route-fix">
                        Run it for me
                    </button>
                    {fixing && <span className="help-fix-note">{fixing}</span>}
                </div>
                <ul>
                    <li>
                        If your VPN has a <b>kill-switch firewall</b>{" "}
                        (Mullvad &amp; co.), the route won't help — enable “local network sharing” (then reconnect), or
                        temporarily disconnect the VPN.
                    </li>
                </ul>
                <p>
                    Then press <b>Scan</b> again.
                </p>
            </div>
        </div>
    )
}

export function ManualDrive(
    { inputRef, onArrowDown }: { inputRef: React.RefObject<HTMLInputElement | null>; onArrowDown: () => void },
) {
    const [ip, setIp] = useState(() => stored<string>(LAST_IP_LS, ""))
    const ok = validIp(ip)
    const drive = () => {
        if (!ok) {
            return
        }
        store(LAST_IP_LS, ip.trim())
        call("POST", "api/drive/connect", { ip: ip.trim() }).catch(() => {}) // the stage shows the error
    }
    return (
        <div className={`p-manual${ok ? " ready" : ""}`}>
            <input
                ref={inputRef}
                className="dim-input"
                placeholder="Know the IP? Drive it directly…"
                autoComplete="off"
                spellCheck={false}
                inputMode="decimal"
                value={ip}
                onChange={(e) => setIp(e.target.value)}
                onKeyDown={(e) => {
                    if (e.key === "Enter") {
                        e.preventDefault()
                        drive()
                    } else if (e.key === "ArrowDown" || e.key === "Escape") {
                        e.preventDefault()
                        e.currentTarget.blur()
                        onArrowDown()
                    }
                }}
            />
            <button type="button" className="dim-btn primary sm" title="Drive this IP live" onClick={drive}>
                Drive
            </button>
        </div>
    )
}

export function scanStatus(scan: Scan, error: string | null): { text: string; cls: string } {
    if (error) {
        return { text: error, cls: "err" }
    }
    if (scan.scanning) {
        return { text: "Scanning…", cls: "scanning" }
    }
    if (scan.lastCount === null) {
        return { text: "Ready", cls: "" }
    }
    return scan.lastCount ? { text: `${scan.lastCount} found`, cls: "ok" } : { text: "None found", cls: "" }
}

// The first-run guide, on the stage until it's done: welcome → find the Go2 (Bluetooth + this network) → put it on
// Wi-Fi → confirm its IP → launch dimos for it (through Desktop) → open Controller. Or, without a robot, a replay. The
// step and the robot picked are the backend's (api/setup), so a reload or another viewer resumes where it was.
import { useEffect, useRef, useState } from "react"
import { call } from "./api.ts"
import { openApp } from "./dim-app/desktop.js"
import { useAppInstalled } from "./dim-app/react.js"
import { Icon } from "./icons.tsx"
import type { Launch, Network, Robot, Scan, SetupState, SetupStep, Wifi } from "./state.ts"
import { store, stored, validIp } from "./util.ts"

const WIFI_LS = "go2dash.wifi" // shared with the panel's Wi-Fi form: the last network that worked
/** a real Go2 joins the network about 30 s after taking the credentials; give it a while */
const JOIN_TIMEOUT_MS = 90_000
const RESCAN_EVERY_MS = 9_000
export const BLUEPRINT = { name: "unitree-go2-basic", title: "Go2 basic" }

type Props = {
    setup: SetupState
    robots: Robot[]
    scan: Scan
    wifi: Wifi
    network: Network
    onOpenAccounts: () => void
    onShowHelp: () => void
}

const put = (body: Partial<{ step: SetupStep; robot: string; ip: string; mode: string }>) =>
    call<SetupState>("PUT", "api/setup", body)

const ROBOT_STEPS: { step: SetupStep; label: string }[] = [
    { step: "find", label: "Find" },
    { step: "wifi", label: "Wi-Fi" },
    { step: "address", label: "Address" },
    { step: "launch", label: "Launch" },
]

function Progress({ setup }: { setup: SetupState }) {
    if (setup.step === "welcome" || setup.mode === "replay") {
        return null
    }
    const at = ROBOT_STEPS.findIndex((s) => s.step === setup.step)
    return (
        <ol className="su-steps" aria-label="Setup progress">
            {ROBOT_STEPS.map((s, i) => (
                <li
                    key={s.step}
                    className={i < at ? "done" : i === at ? "now" : ""}
                    aria-current={i === at ? "step" : undefined}
                >
                    <span className="n">{i < at ? <Icon name="check" size={11} /> : i + 1}</span>
                    {s.label}
                </li>
            ))}
        </ol>
    )
}

export function Setup(props: Props) {
    const { setup } = props
    const [error, setError] = useState<string | null>(null)
    const go = (body: Parameters<typeof put>[0]) => {
        setError(null)
        return put(body).catch((e) => setError(e.message))
    }
    useEffect(() => setError(null), [setup.step])
    const step = setup.step
    return (
        <div className="setup dim-empty" data-testid={`setup-${step}`} data-step={step}>
            <div className="su-top">
                <span className="dim-empty-label">
                    {setup.mode === "replay" ? "Try it on a recording" : "Set up your Go2"}
                </span>
                <Progress setup={setup} />
            </div>
            {step === "welcome" && <Welcome go={go} robot={setup.robot} />}
            {step === "find" && <Find {...props} go={go} />}
            {step === "wifi" && <WifiStep {...props} go={go} />}
            {step === "address" && <Address {...props} go={go} />}
            {step === "launch" && <LaunchStep {...props} go={go} />}
            {error && <div className="dim-alert danger su-err">{error}</div>}
            {step !== "welcome" && (
                <button
                    type="button"
                    className="dim-link su-skip"
                    onClick={() => go({ step: "done" })}
                >
                    Skip setup: show the robot list
                </button>
            )}
        </div>
    )
}

type Go = (body: Parameters<typeof put>[0]) => Promise<unknown>

function Welcome({ go, robot }: { go: Go; robot: SetupState["robot"] }) {
    return (
        <>
            <div className="dim-empty-title">Let's get your Go2 running</div>
            <div className="dim-empty-body">
                Go2 Ctrl finds your robot dog, puts it on your Wi-Fi and starts dimos for it, in about five minutes.
                You'll need the Go2 charged and switched on, this computer near it, and your Wi-Fi's name and password.
            </div>
            <div className="su-choices">
                <button
                    type="button"
                    className="su-choice"
                    data-testid="setup-have-robot"
                    onClick={() => go({ step: "find", mode: "robot" })}
                >
                    <Icon name="robot" size={22} />
                    <span>
                        <b>I have a Go2 here</b>
                        <small>Find it over Bluetooth and this network</small>
                    </span>
                </button>
                <button
                    type="button"
                    className="su-choice"
                    data-testid="setup-no-robot"
                    onClick={() => go({ step: "launch", mode: "replay" })}
                >
                    <Icon name="play" size={22} />
                    <span>
                        <b>No robot: try a replay</b>
                        <small>Play a recorded Go2 walk as if it were live</small>
                    </span>
                </button>
                {robot?.ip && (
                    <button
                        type="button"
                        className="su-choice wide"
                        data-testid="setup-continue"
                        onClick={() => go({ step: "launch", mode: "robot" })}
                    >
                        <Icon name="arrow-right" size={22} />
                        <span>
                            <b>Continue with {robot.name}</b>
                            <small>
                                Saved at <span className="dim-mono">{robot.ip}</span>: launch dimos for it
                            </small>
                        </span>
                    </button>
                )}
            </div>
            <div className="dim-empty-actions">
                <button type="button" className="dim-link" onClick={() => go({ step: "address", mode: "robot" })}>
                    I already know its IP address
                </button>
                <span className="spacer" />
                <button type="button" className="dim-btn ghost sm" onClick={() => go({ step: "done" })}>
                    Skip setup
                </button>
            </div>
        </>
    )
}

function Find(props: Props & { go: Go }) {
    const { robots, scan, go, setup } = props
    const [picked, setPicked] = useState<string | null>(setup.robot?.key ?? null)
    const [scanError, setScanError] = useState<string | null>(null)
    const start = () => {
        setScanError(null)
        call("POST", "api/scan", { timeout: 7, wait: false }).catch((e) => setScanError(e.message))
    }
    const visible = robots.filter((r) => !r.arpOnly || r.ip)
    const chosen = visible.find((r) => r.key === picked) ?? (visible.length === 1 ? visible[0] : undefined)
    const never = scan.lastCount === null && !scan.scanning
    const none = !scan.scanning && scan.lastCount === 0
    const next = () =>
        chosen && go({ robot: chosen.key, step: chosen.ip && chosen.ipSource === "scan" ? "address" : "wifi" })
    return (
        <>
            <div className="dim-empty-title">Find your Go2</div>
            {never && (
                <>
                    <ol className="su-list">
                        <li>Switch the Go2 on and give it about a minute to boot.</li>
                        <li>Keep this computer within a few meters of it, with Bluetooth on.</li>
                        <li>
                            Press Scan. A new Go2 shows up over Bluetooth; one that's already on Wi-Fi shows its IP.
                        </li>
                    </ol>
                    <div className="dim-alert info su-note">
                        The first scan, your computer may ask to let dimOS Desktop use Bluetooth or find devices on your
                        local network: choose Allow. The scan only listens; it changes nothing on the robot or this
                        computer, and needs no password.
                    </div>
                </>
            )}
            {scan.scanning && (
                <div className="su-busy" data-testid="setup-scanning">
                    <div className="dim-progress info indeterminate">
                        <span />
                    </div>
                    <span>Looking over Bluetooth and this network… (about 7 s)</span>
                </div>
            )}
            {scan.notice && <div className="dim-alert warn su-note">{scan.notice}</div>}
            {scanError && <div className="dim-alert danger su-note">{scanError}</div>}
            {visible.length > 0 && (
                <div className="su-robots" role="radiogroup" aria-label="Robots found">
                    {visible.map((robot) => (
                        <label key={robot.key} className={`su-robot${chosen?.key === robot.key ? " on" : ""}`}>
                            <input
                                type="radio"
                                name="su-robot"
                                checked={chosen?.key === robot.key}
                                onChange={() =>
                                    setPicked(robot.key)}
                            />
                            <span className="su-robot-main">
                                <b>{robot.name}</b>
                                <small>
                                    {robot.bleId ? "Bluetooth ✓" : "No Bluetooth"}
                                    {" · "}
                                    {robot.ip && robot.ipSource === "scan"
                                        ? (
                                            <>
                                                on this network at <span className="dim-mono">{robot.ip}</span>
                                            </>
                                        )
                                        : robot.ip
                                        ? (
                                            <>
                                                not seen on this network now (last at{" "}
                                                <span className="dim-mono">{robot.ip}</span>)
                                            </>
                                        )
                                        : "not on Wi-Fi yet"}
                                    {robot.serial ? ` · ${robot.serial}` : ""}
                                </small>
                            </span>
                            {robot.arpOnly && <span className="dim-badge warn">unverified</span>}
                        </label>
                    ))}
                </div>
            )}
            {none && (
                <div className="dim-alert warn su-note" data-testid="setup-none-found">
                    <span>
                        No Go2 found. Check that it's switched on and has finished booting (about a minute), that this
                        computer's Bluetooth is on, and that you're within a few meters. If it's already on your Wi-Fi,
                        this computer must be on the same network, and a VPN can hide it from the scan:{" "}
                        <button type="button" className="dim-link" onClick={props.onShowHelp}>see Help</button>.
                    </span>
                </div>
            )}
            <div className="dim-empty-actions">
                <button type="button" className="dim-btn ghost sm" onClick={() => go({ step: "welcome" })}>Back</button>
                <span className="spacer" />
                {(none || visible.length > 0) && (
                    <button
                        type="button"
                        className="dim-link"
                        onClick={() => go({ step: "address", robot: "" })}
                    >
                        Type its IP instead
                    </button>
                )}
                <button
                    type="button"
                    className={`dim-btn sm${visible.length ? "" : " primary"}`}
                    data-testid="setup-scan"
                    disabled={scan.scanning}
                    onClick={start}
                >
                    {never ? "Scan" : "Scan again"}
                </button>
                {visible.length > 0 && (
                    <button
                        type="button"
                        className="dim-btn primary sm"
                        data-testid="setup-next"
                        disabled={!chosen}
                        onClick={next}
                    >
                        {chosen ? `Use ${chosen.name}` : "Pick one"}
                    </button>
                )}
            </div>
        </>
    )
}

type SavedWifi = { ssid?: string; password?: string; country?: string }

function WifiStep(props: Props & { go: Go }) {
    const { setup, robots, wifi, network, scan, go } = props
    const key = setup.robot?.key ?? ""
    const name = setup.robot?.name ?? "the Go2"
    const saved = stored<SavedWifi>(WIFI_LS, {})
    const initialSsid = (network.ssidStatus === "ok" ? network.ssid : "") || saved.ssid || ""
    const [ssid, setSsid] = useState(initialSsid)
    const [password, setPassword] = useState(saved.ssid && saved.ssid === initialSsid ? saved.password ?? "" : "")
    const [country, setCountry] = useState(saved.country || "US")
    const [formError, setFormError] = useState<string | null>(null)
    const mine = wifi.robot === key
    const sending = mine && wifi.status === "running"
    const accepted = mine && wifi.status === "ok"
    const failed = mine && wifi.status === "error"
    const seen = robots.find((r) => r.key === key && r.ipSource === "scan" && r.ip)
    // after the credentials are accepted: scan every few seconds until the robot shows up with an IP
    const [joinStarted, setJoinStarted] = useState<number | null>(null)
    const [now, setNow] = useState(Date.now())
    useEffect(() => {
        if (accepted && joinStarted === null) {
            setJoinStarted(Date.now())
        }
    }, [accepted])
    const scanning = useRef(scan.scanning)
    scanning.current = scan.scanning
    const waiting = joinStarted !== null && !seen && now - joinStarted < JOIN_TIMEOUT_MS
    useEffect(() => {
        if (joinStarted === null || seen) {
            return
        }
        const rescan = () => {
            if (!scanning.current) {
                call("POST", "api/scan", { timeout: 6, wait: false }).catch(() => {})
            }
        }
        const first = setTimeout(rescan, 1500)
        const every = setInterval(rescan, RESCAN_EVERY_MS)
        const tick = setInterval(() => setNow(Date.now()), 500)
        return () => {
            clearTimeout(first)
            clearInterval(every)
            clearInterval(tick)
        }
    }, [joinStarted, !!seen])
    useEffect(() => {
        if (seen) {
            go({ robot: key, step: "address" })
        }
    }, [!!seen])
    const timedOut = joinStarted !== null && !seen && !waiting
    const mismatch = network.ssidStatus === "ok" && network.ssid && ssid.trim() && ssid.trim() !== network.ssid
    const send = () => {
        if (!ssid.trim()) {
            setFormError("Enter your Wi-Fi's name first.")
            return
        }
        setFormError(null)
        setJoinStarted(null)
        const attempt = { ssid: ssid.trim(), password, country: (country.trim() || "US").toUpperCase() }
        call("POST", `api/robots/${encodeURIComponent(key)}/wifi`, attempt).then(
            () => store(WIFI_LS, attempt),
            (e) => setFormError(e.message === "cancelled" ? null : e.message),
        )
    }
    const elapsed = joinStarted === null ? 0 : Math.min(1, (now - joinStarted) / JOIN_TIMEOUT_MS)
    return (
        <>
            <div className="dim-empty-title">Put {name} on your Wi-Fi</div>
            {!accepted && (
                <>
                    <div className="dim-empty-body">
                        {name}{" "}
                        isn't on a network yet. Go2 Ctrl sends it your Wi-Fi's name and password over Bluetooth. Use the
                        network this computer is on, so the two can reach each other.
                    </div>
                    <div className="su-form">
                        <div className="dim-field">
                            <label htmlFor="su-ssid">Wi-Fi name</label>
                            <input
                                id="su-ssid"
                                className="dim-input"
                                value={ssid}
                                placeholder="your network's name"
                                autoComplete="off"
                                spellCheck={false}
                                disabled={sending}
                                onChange={(e) => setSsid(e.target.value)}
                            />
                        </div>
                        <div className="su-two">
                            <div className="dim-field">
                                <label htmlFor="su-pass">Password</label>
                                <input
                                    id="su-pass"
                                    className="dim-input"
                                    type="password"
                                    value={password}
                                    placeholder="••••••"
                                    disabled={sending}
                                    onChange={(e) => setPassword(e.target.value)}
                                    onKeyDown={(e) => e.key === "Enter" && send()}
                                />
                            </div>
                            <div className="dim-field su-country">
                                <label htmlFor="su-country">Country</label>
                                <input
                                    id="su-country"
                                    className="dim-input"
                                    value={country}
                                    maxLength={2}
                                    disabled={sending}
                                    onChange={(e) => setCountry(e.target.value)}
                                />
                            </div>
                        </div>
                        {mismatch && (
                            <div className="dim-alert warn su-note">
                                This computer is on “{network.ssid}”, not “{ssid.trim()}”. A Go2 on “{ssid.trim()}”
                                can't be reached from here unless this computer joins it too.
                            </div>
                        )}
                        {network.ssidStatus === "redacted" && (
                            <div className="su-hint">
                                macOS hides this computer's Wi-Fi name from apps without Location Services, so type it
                                in.
                            </div>
                        )}
                    </div>
                </>
            )}
            {(sending || (mine && (wifi.log?.length ?? 0) > 0 && !accepted)) && (
                <div className="su-log dim-mono">
                    {(wifi.log ?? []).map((line, i) => <div key={i}>{line}</div>)}
                </div>
            )}
            {failed && (
                <div className="dim-alert danger su-note">
                    Couldn't send it:{" "}
                    {wifi.error}. Stay close to the Go2 and try again; if it keeps failing, switch the Go2 off and on.
                </div>
            )}
            {formError && <div className="dim-alert danger su-note">{formError}</div>}
            {accepted && (waiting || seen) && (
                <div className="su-busy" data-testid="setup-joining">
                    <div className="dim-progress info">
                        <span style={{ width: `${Math.round(elapsed * 100)}%` }} />
                    </div>
                    <span>
                        ✓ {name}{" "}
                        took the credentials. Waiting for it to join “{wifi.ssid}” (usually about 30 s)… scanning every
                        few seconds.
                    </span>
                </div>
            )}
            {accepted && timedOut && (
                <div className="dim-alert warn su-note" data-testid="setup-join-timeout">
                    <span>
                        {name}{" "}
                        hasn't shown up on this network after 90 s. Check the Wi-Fi name and password (both are
                        case-sensitive) and that this computer is on “{wifi.ssid}” too; a VPN can also hide it (
                        <button type="button" className="dim-link" onClick={props.onShowHelp}>Help</button>). The
                        Unitree Go app or your router's device list shows its IP if you'd rather type it.
                    </span>
                </div>
            )}
            <div className="dim-empty-actions">
                <button type="button" className="dim-btn ghost sm" onClick={() => go({ step: "find" })}>Back</button>
                <span className="spacer" />
                <button type="button" className="dim-link" onClick={() => go({ step: "address" })}>
                    {accepted ? "Type its IP" : "It's already on my Wi-Fi"}
                </button>
                {sending
                    ? (
                        <button
                            type="button"
                            className="dim-btn sm"
                            onClick={() => call("POST", "api/wifi/cancel").catch(() => {})}
                        >
                            Cancel
                        </button>
                    )
                    : accepted && !timedOut
                    ? null
                    : (
                        <button
                            type="button"
                            className="dim-btn primary sm"
                            data-testid="setup-send-wifi"
                            onClick={accepted ? () => setJoinStarted(Date.now()) : send}
                        >
                            <Icon name="signal" size={13} />
                            {accepted ? "Keep waiting" : failed ? "Try again" : "Send Wi-Fi to Go2"}
                        </button>
                    )}
            </div>
        </>
    )
}

type Check = { ip: string; reachable: boolean; ms: number | null; error: string | null } | null

function Address(props: Props & { go: Go }) {
    const { setup, go, robots } = props
    const robot = setup.robot
    const scanned = robots.find((r) => r.key === robot?.key && r.ipSource === "scan")
    const [ip, setIp] = useState(robot?.ip ?? "")
    const [check, setCheck] = useState<Check>(null)
    const [checking, setChecking] = useState(false)
    useEffect(() => setIp(robot?.ip ?? ""), [robot?.ip])
    const ok = validIp(ip)
    const runCheck = (target: string) => {
        setChecking(true)
        setCheck(null)
        return call<NonNullable<Check>>("POST", "api/check-ip", { ip: target.trim() }).then(
            (c) => setCheck(c),
            (e) => setCheck({ ip: target, reachable: false, ms: null, error: e.message }),
        ).finally(() => setChecking(false))
    }
    // check the IP the scan found (or the one saved) as soon as the step opens
    useEffect(() => {
        if (robot?.ip && validIp(robot.ip)) {
            runCheck(robot.ip)
        }
    }, [])
    const current = check && check.ip === ip.trim() ? check : null
    const save = () => ok && go({ ip: ip.trim(), step: "launch" })
    return (
        <>
            <div className="dim-empty-title">{robot ? `Confirm ${robot.name}'s address` : "Your Go2's address"}</div>
            <div className="dim-empty-body">
                {scanned?.ip
                    ? "Found on this network. dimos connects to the Go2 at this IP address; change it only if you know better."
                    : "dimos connects to the Go2 at its IP address on your network. The Unitree Go app, or your router's list of devices, shows it."}
            </div>
            <div className="su-ip">
                <div className="dim-field">
                    <label htmlFor="su-ip">IP address</label>
                    <input
                        id="su-ip"
                        className={`dim-input dim-mono${ip && !ok ? " bad" : ""}`}
                        value={ip}
                        placeholder="e.g. 192.168.1.42"
                        inputMode="decimal"
                        autoComplete="off"
                        spellCheck={false}
                        onChange={(e) => setIp(e.target.value)}
                        onKeyDown={(e) => e.key === "Enter" && ok && runCheck(ip)}
                    />
                </div>
                <button type="button" className="dim-btn sm" disabled={!ok || checking} onClick={() => runCheck(ip)}>
                    {checking ? "Checking…" : "Check"}
                </button>
            </div>
            {ip && !ok && (
                <div className="su-hint">
                    That isn't an IP address: four numbers 0–255 with dots, like 192.168.1.42.
                </div>
            )}
            {current?.reachable && (
                <div className="dim-alert ok su-note" data-testid="setup-reachable">
                    A Go2 answers at {current.ip}
                    {current.ms !== null ? ` (${current.ms} ms)` : ""}.
                </div>
            )}
            {current && !current.reachable && (
                <div className="dim-alert warn su-note" data-testid="setup-unreachable">
                    Nothing answered at {current.ip}{" "}
                    ({current.error}). Is the Go2 on, and on the same network as this computer? You can still continue.
                </div>
            )}
            {robot && (
                <div className="su-hint su-aes">
                    {robot.hasAesKey ? "✓ Its AES key is saved (firmware 1.1.15 and newer need it)." : (
                        <>
                            Firmware 1.1.15 and newer also need the robot's AES key, which only Unitree's cloud hands
                            out.{" "}
                            <button type="button" className="dim-link" onClick={props.onOpenAccounts}>
                                Sign in with your Unitree account
                            </button>{" "}
                            to fetch it, or skip this for older firmware.
                        </>
                    )}
                </div>
            )}
            <div className="dim-empty-actions">
                <button
                    type="button"
                    className="dim-btn ghost sm"
                    onClick={() => go({ step: robot?.ip || !robot ? "find" : "wifi" })}
                >
                    Back
                </button>
                <span className="spacer" />
                <button
                    type="button"
                    className="dim-btn primary sm"
                    data-testid="setup-save-ip"
                    disabled={!ok}
                    onClick={save}
                >
                    Save and continue
                </button>
            </div>
        </>
    )
}

function stepIcon(state: string) {
    return state === "done" ? "✓" : state === "failed" ? "✗" : state === "now" ? "" : "○"
}

function LaunchStep(props: Props & { go: Go }) {
    const { setup, go } = props
    const replay = setup.mode === "replay"
    const robot = setup.robot
    const [launch, setLaunch] = useState<Launch | null | undefined>(undefined)
    const [launchError, setLaunchError] = useState<string | null>(null)
    const [busy, setBusy] = useState(false)
    const [asDefault, setAsDefault] = useState(true)
    const controller = useAppInstalled("dim-controller")
    const refresh = () =>
        call<{ launch: Launch | null }>("GET", "api/launch").then(
            (r) => setLaunch(r.launch),
            (e) => {
                setLaunch(null)
                setLaunchError(e.message)
            },
        )
    useEffect(() => {
        refresh()
    }, [])
    useEffect(() => {
        if (launch?.phase !== "starting") {
            return
        }
        const timer = setInterval(refresh, 1000)
        return () => clearInterval(timer)
    }, [launch?.phase])
    const start = (stopFirst = false) => {
        setBusy(true)
        setLaunchError(null)
        const body = replay
            ? { replay: true, blueprint: BLUEPRINT.name }
            : { blueprint: BLUEPRINT.name, default: asDefault }
        ;(stopFirst ? call("POST", "api/launch/stop") : Promise.resolve())
            .then(() => call<Launch>("POST", "api/launch", body))
            .then((l) => setLaunch(l), (e) => setLaunchError(e.message))
            .finally(() => setBusy(false))
    }
    const stop = () => call("POST", "api/launch/stop").then(refresh, (e) => setLaunchError(e.message))
    const ours = launch && launch.blueprint === BLUEPRINT.name &&
        (replay ? launch.overrides?.replay === true : launch.overrides?.robot_ip === robot?.ip)
    const other = launch && !ours && (launch.phase === "starting" || launch.phase === "running") ? launch : null
    const shown = ours ? launch : null
    const busyElsewhere = launchError?.includes("stop it first")
    return (
        <>
            <div className="dim-empty-title">
                {replay ? "No robot? Play a recording" : `Start dimos for ${robot?.name ?? "your Go2"}`}
            </div>
            <div className="dim-empty-body">
                {replay
                    ? "dimos plays back a recorded Go2 walk (go2_short) as if it were live: camera, lidar and pose. Open it in Controller to look around; nothing reaches a real robot."
                    : `${BLUEPRINT.title} (${BLUEPRINT.name}) connects to the Go2 and streams its camera, lidar and pose. Drive it from Controller.`}
            </div>
            {!replay && robot && (
                <div className="su-summary">
                    <div className="dim-kv">
                        <span>Robot</span>
                        <span>{robot.name}</span>
                    </div>
                    <div className="dim-kv">
                        <span>IP address</span>
                        <span>{robot.ip ?? "—"}</span>
                    </div>
                    <div className="dim-kv">
                        <span>AES key</span>
                        <span>{robot.hasAesKey ? "saved" : "none"}</span>
                    </div>
                </div>
            )}
            {!replay && !robot?.ip && (
                <div className="dim-alert warn su-note">
                    There's no IP address for this robot yet: go back a step and add it.
                </div>
            )}
            {!replay && !shown && (
                <label className="dim-check su-default">
                    <input type="checkbox" checked={asDefault} onChange={(e) => setAsDefault(e.target.checked)} />
                    <span className="box" />
                    Also use {robot?.ip ?? "this IP"} for Go2 launches from the Launcher
                </label>
            )}
            {other && (
                <div className="dim-alert warn su-note" data-testid="setup-other-running">
                    Desktop is already running {other.blueprint}{" "}
                    ({other.phase}). One launch at a time: stop it to start this one.
                </div>
            )}
            {shown && (
                <div className="su-launch" data-testid={`setup-launch-${shown.phase}`}>
                    <ol className="su-launch-steps">
                        {(shown.steps ?? []).map((s) => (
                            <li key={s.label} className={s.state}>
                                <span className="st">{stepIcon(s.state)}</span>
                                {s.label}
                                {s.detail ? <small>· {s.detail}</small> : null}
                            </li>
                        ))}
                    </ol>
                    {shown.phase === "running" && (
                        <div className="dim-alert ok su-note">
                            dimos is running {BLUEPRINT.title}
                            {replay
                                ? " on the recording. Open Controller to look around."
                                : ` for ${robot?.name}. Open Controller to see it and drive.`}
                        </div>
                    )}
                    {shown.phase === "failed" && (
                        <div className="dim-alert danger su-note">
                            <span>
                                dimos stopped while starting: {shown.error}
                                {(shown.problems ?? []).map((p, i) => (
                                    <span key={i} className="su-problem">
                                        <br />
                                        {p.text}
                                        {p.fix ? ` Fix: ${p.fix}` : ""}
                                    </span>
                                ))}
                            </span>
                        </div>
                    )}
                    {shown.phase === "stopped" && (
                        <div className="su-hint">It stopped. Launch it again whenever you like.</div>
                    )}
                    {shown.mock && (
                        <div className="su-hint">Mock mode: this launch is simulated, nothing was started.</div>
                    )}
                </div>
            )}
            {launchError && !busyElsewhere && <div className="dim-alert danger su-note">{launchError}</div>}
            {busyElsewhere && <div className="dim-alert warn su-note">{launchError}</div>}
            <div className="dim-empty-actions">
                <button
                    type="button"
                    className="dim-btn ghost sm"
                    onClick={() => go(replay ? { step: "welcome", mode: "" } : { step: "address" })}
                >
                    Back
                </button>
                <span className="spacer" />
                {!replay && !shown && (
                    <button
                        type="button"
                        className="dim-link"
                        onClick={() => openApp("launcher", { robot: "go2", kind: "blueprint" })}
                    >
                        Pick another blueprint
                    </button>
                )}
                {shown && (shown.phase === "starting" || shown.phase === "running") && (
                    <button type="button" className="dim-btn sm" onClick={stop}>
                        <Icon name="stop" size={13} />
                        Stop
                    </button>
                )}
                {shown?.phase === "running"
                    ? (
                        <button
                            type="button"
                            className="dim-btn primary sm"
                            data-testid="setup-open-controller"
                            onClick={() => {
                                openApp(controller === false ? "appstore" : "dim-controller")
                                go({ step: "done" })
                            }}
                        >
                            {controller === false ? "Get Controller from the App Store" : "Open Controller"}
                        </button>
                    )
                    : shown?.phase === "starting"
                    ? (
                        <button type="button" className="dim-btn primary sm" disabled>
                            Starting…
                        </button>
                    )
                    : (
                        <button
                            type="button"
                            className="dim-btn primary sm"
                            data-testid="setup-launch-go"
                            disabled={busy || launch === undefined || (!replay && !robot?.ip)}
                            onClick={() => start(!!other || !!busyElsewhere)}
                        >
                            <Icon name="play" size={13} />
                            {other || busyElsewhere
                                ? "Stop it and launch"
                                : replay
                                ? "Launch the replay"
                                : `Launch ${BLUEPRINT.title}`}
                        </button>
                    )}
            </div>
            {!replay && robot?.ip && !shown && (
                <div className="su-hint su-alt">
                    Or skip dimos and{" "}
                    <button
                        type="button"
                        className="dim-link"
                        onClick={() => {
                            // the scanned robot (for its saved AES key), else just its IP
                            const scanned = props.robots.some((r) => r.key === robot.key)
                            call(
                                "POST",
                                "api/drive/connect",
                                scanned ? { robot: robot.key, ip: robot.ip } : { ip: robot.ip },
                            ).catch(() => {})
                            go({ step: "done" })
                        }}
                    >
                        drive it right here
                    </button>{" "}
                    (camera and controls over WebRTC; a Go2 takes one connection at a time).
                </div>
            )}
        </>
    )
}

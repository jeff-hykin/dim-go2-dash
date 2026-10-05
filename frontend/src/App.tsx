// Go2 Ctrl: discover nearby Unitree Go2s (BLE + LAN), put them on Wi-Fi over Bluetooth, and drive one live. A floating
// setup panel on the left; the stage fills with the camera and controls while driving (the panel slides away, J
// toggles it). All state is the backend's (state.ts); every button is an endpoint call.
import { useCallback, useEffect, useRef, useState } from "react"
import { call } from "./api.ts"
import { Control } from "./Control.tsx"
import { EmptyState } from "./dim-app/react.js"
import { Icon } from "./icons.tsx"
import { Accounts, Help, ManualDrive, RobotCard, scanStatus } from "./Panel.tsx"
import { Setup } from "./Setup.tsx"
import { type CommandRecord, type Robot, useBackend } from "./state.ts"
import { ThemeToggle } from "./ThemeToggle.tsx"

type Open = { key: string; what: "menu" | "form" | "details" | "edit" } | null

const DEFAULT_ROUTE_FIX = "sudo route -n add -host 231.1.1.1 -interface en0"
// keyboard navigation: per-dog focus targets, left to right on the card
const NAV_SELECTORS = [".ip", ".drive", ".kebab"]

export function App() {
    const [flash, setFlash] = useState<{ name: string; ok: boolean; n: number } | null>(null)
    const flashTimer = useRef<ReturnType<typeof setTimeout>>(undefined)
    const showFlash = useCallback((name: string, ok: boolean) => {
        clearTimeout(flashTimer.current)
        setFlash((f) => ({ name, ok, n: (f?.n ?? 0) + 1 }))
        flashTimer.current = setTimeout(() => setFlash(null), 900)
    }, [])
    const onCommand = useCallback((command: CommandRecord) => {
        showFlash(command.name, true)
        if (command.dryRun) {
            showToastRef.current(`Dry run: ${command.label} — not sent to a robot`)
        }
    }, [])
    const [state, loadError] = useBackend(onCommand)
    const [slid, setSlid] = useState(false)
    const [open, setOpen] = useState<Open>(null)
    const [accountsOpen, setAccountsOpen] = useState(false)
    const [accountsFlash, setAccountsFlash] = useState(0)
    const [scanError, setScanError] = useState<string | null>(null)
    const [awaitingIp, setAwaitingIp] = useState<Set<string>>(new Set())
    const [routeFix, setRouteFix] = useState(DEFAULT_ROUTE_FIX)
    const [toast, setToast] = useState<string | null>(null)
    const toastTimer = useRef<ReturnType<typeof setTimeout>>(undefined)
    const lastToastAt = useRef(0)
    const showToast = (text: string) => {
        const now = Date.now()
        if (now - lastToastAt.current < 1200 && toast) {
            return // throttled, so a held key doesn't retrigger it every frame
        }
        lastToastAt.current = now
        setToast(text)
        clearTimeout(toastTimer.current)
        toastTimer.current = setTimeout(() => setToast(null), 2600)
    }
    const showToastRef = useRef(showToast)
    showToastRef.current = showToast
    const manualInput = useRef<HTMLInputElement>(null)
    const scanButton = useRef<HTMLButtonElement>(null)
    const list = useRef<HTMLDivElement>(null)
    const nav = useRef<{ key: string | null; btn: number }>({ key: null, btn: 0 })

    const drive = state?.drive ?? { active: false as const }
    const driving = drive.active
    // the first-run guide owns the stage until it's done (or skipped)
    const setup = state?.setup
    const guiding = !!setup && setup.step !== "done" && !driving

    // the panel slides away while driving or being guided, and comes back after
    useEffect(() => setSlid(driving || guiding), [driving, guiding])
    useEffect(() => {
        call<{ multicast?: { fix: string } }>("GET", "api/network").then(
            (n) => n.multicast && setRouteFix(n.multicast.fix),
            () => {},
        )
    }, [])
    // after a successful provisioning, wait for that dog to show up on the network
    useEffect(() => {
        const wifi = state?.wifi
        if (wifi?.status === "ok" && wifi.robot) {
            setAwaitingIp((s) => new Set(s).add(wifi.robot!))
            const timer = setTimeout(() => setOpen((o) => (o?.what === "form" ? null : o)), 1200)
            return () => clearTimeout(timer)
        }
    }, [state?.wifi.status, state?.wifi.robot])
    useEffect(() => {
        const seen = (state?.robots ?? []).filter((r) => r.ipSource === "scan").map((r) => r.key)
        if (seen.some((key) => awaitingIp.has(key))) {
            setAwaitingIp((s) => new Set([...s].filter((key) => !seen.includes(key))))
        }
    }, [state?.robots])

    const [helpRequest, setHelpRequest] = useState(0)
    const openAccounts = () => {
        setSlid(false)
        setAccountsOpen(true)
        setAccountsFlash((n) => n + 1)
    }

    const scan = () => {
        setOpen(null)
        setScanError(null)
        if (driving) {
            call("POST", "api/drive/disconnect").catch(() => {}) // re-scan wipes the list; drop the drive session
        }
        call("POST", "api/scan", { timeout: 7, wait: false }).catch((e) => setScanError(e.message))
    }

    // ── keyboard navigation of the panel: manual IP ↔ Scan ↔ dogs ↔ each dog's buttons ──
    const dogEls = () => [...(list.current?.querySelectorAll<HTMLElement>(".dog") ?? [])]
    const dogEl = (key: string | null) => (key === null ? null : dogEls().find((d) => d.dataset.key === key) ?? null)
    const dogNavEls = (dog: HTMLElement | null): HTMLElement[] => {
        if (!dog) {
            return []
        }
        const base = NAV_SELECTORS.map((s) => dog.querySelector<HTMLElement>(s)).filter((e): e is HTMLElement => !!e)
        if (dog.classList.contains("menu-open")) {
            base.push(...dog.querySelectorAll<HTMLElement>(".menu button:not([disabled])"))
        }
        return base
    }
    const applyNav = (scroll: boolean) => {
        document.querySelectorAll(".nav-focus").forEach((e) => e.classList.remove("nav-focus"))
        list.current?.querySelectorAll(".nav-dog").forEach((e) => e.classList.remove("nav-dog"))
        const dog = dogEl(nav.current.key)
        if (nav.current.key !== null && !dog) {
            nav.current.key = null // the focused dog vanished — fall back to Scan
        }
        if (!dog) {
            scanButton.current?.classList.add("nav-focus")
            return
        }
        dog.classList.add("nav-dog")
        const buttons = dogNavEls(dog)
        if (!buttons.length) {
            return
        }
        nav.current.btn = Math.max(0, Math.min(nav.current.btn, buttons.length - 1))
        buttons[nav.current.btn].classList.add("nav-focus")
        if (scroll) {
            buttons[nav.current.btn].scrollIntoView({ block: "nearest" })
        }
    }
    useEffect(() => applyNav(false))

    useEffect(() => {
        const onKey = (e: KeyboardEvent) => {
            const t = e.target as HTMLElement | null
            const typing = t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)
            // "j" toggles the setup panel (and hands the keyboard between it and the driver)
            if (e.key === "j" && !e.altKey && !e.ctrlKey && !e.metaKey && !e.repeat && !typing) {
                e.preventDefault()
                setSlid((s) => !s)
                return
            }
            if (slid || typing || e.altKey || e.ctrlKey || e.metaKey) {
                return
            }
            const dogs = dogEls()
            const dog = dogEl(nav.current.key)
            const menuItems = (d: HTMLElement) =>
                dogNavEls(d).map((b, i) => (b.closest(".menu") ? i : -1)).filter((i) => i >= 0)
            if (dog && dog.classList.contains("menu-open")) {
                const items = menuItems(dog)
                const pos = items.indexOf(nav.current.btn)
                const closeToKebab = () => {
                    setOpen(null)
                    nav.current.btn = NAV_SELECTORS.length - 1
                }
                if (e.key === "ArrowDown") {
                    e.preventDefault()
                    nav.current.btn = items[(pos + 1) % items.length]
                } else if (e.key === "ArrowUp") {
                    e.preventDefault()
                    if (pos <= 0) {
                        closeToKebab()
                    } else {
                        nav.current.btn = items[pos - 1]
                    }
                } else if (e.key === "ArrowLeft" || e.key === "Escape") {
                    e.preventDefault()
                    closeToKebab()
                } else if (e.key === "Enter") {
                    e.preventDefault()
                    dogNavEls(dog)[nav.current.btn]?.click()
                }
                applyNav(true)
                return
            }
            if (e.key === "ArrowDown") {
                e.preventDefault()
                if (nav.current.key === null) {
                    if (dogs.length) {
                        nav.current = {
                            key: dogs[0].dataset.key!,
                            btn: Math.max(0, dogNavEls(dogs[0]).findIndex((b) => b.classList.contains("drive"))),
                        }
                    }
                } else {
                    const i = dogs.findIndex((d) => d.dataset.key === nav.current.key)
                    if (i >= 0 && i < dogs.length - 1) {
                        nav.current.key = dogs[i + 1].dataset.key!
                    }
                }
            } else if (e.key === "ArrowUp") {
                e.preventDefault()
                if (nav.current.key === null) {
                    manualInput.current?.focus()
                    manualInput.current?.select()
                    return
                }
                const i = dogs.findIndex((d) => d.dataset.key === nav.current.key)
                nav.current.key = i <= 0 ? null : dogs[i - 1].dataset.key!
            } else if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
                const buttons = dogNavEls(dog)
                if (!buttons.length) {
                    return
                }
                e.preventDefault()
                nav.current.btn = (nav.current.btn + (e.key === "ArrowRight" ? 1 : -1) + buttons.length) %
                    buttons.length
            } else if (e.key === "Enter") {
                e.preventDefault()
                if (!dog) {
                    scanButton.current?.click()
                    return
                }
                const target = dogNavEls(dog)[nav.current.btn]
                const openingMenu = target?.classList.contains("kebab") && !dog.classList.contains("menu-open")
                target?.click()
                if (openingMenu) {
                    // the menu renders next frame — drop focus onto its first item
                    requestAnimationFrame(() => {
                        const items = menuItems(dogEl(nav.current.key)!)
                        if (items.length) {
                            nav.current.btn = items[0]
                        }
                        applyNav(true)
                    })
                }
            } else {
                return
            }
            applyNav(true)
        }
        globalThis.addEventListener("keydown", onKey)
        return () => globalThis.removeEventListener("keydown", onKey)
    }, [slid])

    // click outside any dog closes an open ⋯ menu
    useEffect(() => {
        const onClick = (e: MouseEvent) => {
            if (open?.what === "menu" && !(e.target as HTMLElement).closest(".dog")) {
                setOpen(null)
            }
        }
        document.addEventListener("click", onClick)
        return () => document.removeEventListener("click", onClick)
    }, [open])

    const status = scanStatus(
        state?.scan ?? { scanning: false, lastCount: null, notice: null },
        scanError ?? (loadError ? "No backend — can't scan" : null),
    )
    const robots: Robot[] = state?.robots ?? []
    const wifi = state?.wifi
    const sent = wifi?.status === "ok" ? robots.find((r) => r.key === wifi.robot) : undefined
    return (
        <>
            <div
                className={`stage${sent ? " connected" : ""}${guiding ? " guiding" : ""}`}
                style={driving ? { display: "none" } : undefined}
            >
                {guiding && state
                    ? (
                        <Setup
                            setup={state.setup}
                            robots={robots}
                            scan={state.scan}
                            wifi={state.wifi}
                            network={state.network}
                            onOpenAccounts={openAccounts}
                            onShowHelp={() => {
                                setSlid(false)
                                setHelpRequest((n) => n + 1)
                            }}
                        />
                    )
                    : sent || (state && robots.length > 0)
                    ? (
                        <div className="empty">
                            <div className="glyph">
                                <Icon name={sent ? "signal" : "robot"} size={44} />
                            </div>
                            <h1>
                                {sent
                                    ? `Wi-Fi sent to ${sent.name} — waiting for it on the network`
                                    : "No dog connected yet"}
                            </h1>
                            <p>
                                {sent
                                    ? `Credentials delivered over Bluetooth. Once ${sent.name} joins the network and we see its IP, press Drive for its camera & controls.`
                                    : "Pick a Go2 in the list: Drive it if it has an IP, or send it Wi-Fi credentials over Bluetooth to bring it onto your network."}
                            </p>
                        </div>
                    )
                    : loadError && !state
                    ? (
                        <EmptyState
                            testId="onboard-no-backend"
                            tone="warn"
                            label="Server not answering"
                            title="Can't reach the Go2 Ctrl server"
                            body={`It didn't answer (${loadError}). Restarting the app usually fixes it: try again, or stop and start Go2 Ctrl from Desktop's App Store.`}
                            actions={[
                                { label: "Try again", onClick: () => location.reload() },
                                { label: "Open the App Store", app: "appstore", primary: false },
                            ]}
                        />
                    )
                    : state?.scan.scanning
                    ? (
                        <EmptyState
                            testId="onboard-scanning"
                            busy
                            label="Scanning"
                            title="Looking for Go2s nearby"
                            body="Searching this network and Bluetooth. This takes a few seconds."
                        />
                    )
                    : state && state.scan.lastCount === 0
                    ? (
                        <EmptyState
                            testId="onboard-no-robot"
                            tone="warn"
                            label="No robot found"
                            title="No Go2 found on this network"
                            body="Power the Go2 on and wait about a minute for it to boot. Then put this computer on the same Wi-Fi as the dog (or on the dog's own hotspot), or stay close to it: a new Go2 shows up over Bluetooth, and you can send it your Wi-Fi from here."
                            actions={[
                                { label: "Scan again", onClick: scan },
                                {
                                    label: "Help: it shows but never gets an IP",
                                    onClick: () => {
                                        setSlid(false)
                                        setHelpRequest((n) => n + 1)
                                    },
                                },
                            ]}
                        />
                    )
                    : state
                    ? (
                        <EmptyState
                            testId="onboard-find-robot"
                            label="Go2 Ctrl"
                            title="Find your Go2"
                            body="Scan this network and Bluetooth for Unitree Go2s nearby. Then put one on your Wi-Fi and drive it, with its camera, from here."
                            actions={[{ label: "Scan for Go2s", onClick: scan }]}
                        />
                    )
                    : null}
            </div>

            <div className={`panel dim-panel${slid ? " slid" : ""}`}>
                <div className="p-head">
                    <span className="t dim-label">Robots</span>
                    {state?.network.mock && <span className="dim-badge warn">Mock</span>}
                    <span className="spacer" />
                    <ThemeToggle />
                    <span className="kbd-hint" title="J shows and hides this panel">
                        <kbd>J</kbd>
                    </span>
                    <button
                        type="button"
                        className="icon-btn dim-btn ghost icon"
                        title="Slide panel away"
                        onClick={() => setSlid(true)}
                    >
                        <Icon name="chevron-left" size={16} />
                    </button>
                </div>
                <ManualDrive
                    inputRef={manualInput}
                    onArrowDown={() => {
                        nav.current = { key: null, btn: 0 }
                        applyNav(true)
                    }}
                />
                <div className="p-scan">
                    <button
                        type="button"
                        ref={scanButton}
                        className="dim-btn primary sm"
                        disabled={!!state?.scan.scanning}
                        onClick={scan}
                    >
                        Scan
                    </button>
                    <span className="status">
                        <span className={`dot ${status.cls}`} />
                        <span>{status.text}</span>
                    </span>
                    <span className="spacer" />
                    <button
                        type="button"
                        className="dim-btn ghost sm guide-btn"
                        title="Walk through finding a Go2, putting it on Wi-Fi and launching dimos for it"
                        disabled={!state}
                        onClick={() =>
                            call("PUT", "api/setup", { step: state?.setup.robot?.ip ? "launch" : "welcome" }).catch(
                                () => {},
                            )}
                    >
                        Setup guide
                    </button>
                </div>
                <div className="list" ref={list}>
                    {state?.scan.notice && <div className="none warn">{state.scan.notice}</div>}
                    {loadError && !state && (
                        <div className="none err">
                            Can't reach the Go2 Ctrl server: {loadError}
                            <br />Restart the app (App Store), then reload.
                        </div>
                    )}
                    {state && robots.length === 0 && (
                        <div className="none">
                            {state.scan.scanning ? "Scanning for nearby Go2s…" : (
                                <>
                                    {state.scan.lastCount === null
                                        ? "Not scanned yet."
                                        : "No Go2 found on this network."}
                                    <br />Power on a Go2 nearby and press <b>Scan</b>.
                                </>
                            )}
                        </div>
                    )}
                    {state && robots.map((robot) => (
                        <RobotCard
                            key={robot.key}
                            robot={robot}
                            drive={state.drive}
                            wifi={state.wifi}
                            network={state.network}
                            awaitingIp={awaitingIp.has(robot.key)}
                            open={{
                                menu: open?.key === robot.key && open.what === "menu",
                                form: open?.key === robot.key && open.what === "form",
                                details: open?.key === robot.key && open.what === "details",
                                edit: open?.key === robot.key && open.what === "edit",
                            }}
                            setOpen={(what) => setOpen(what ? { key: robot.key, what } : null)}
                            onOpenAccounts={openAccounts}
                        />
                    ))}
                </div>
                <Accounts
                    accounts={state?.accounts ?? []}
                    open={accountsOpen}
                    setOpen={setAccountsOpen}
                    flash={accountsFlash}
                />
                <Help route={routeFix} openRequest={helpRequest} />
            </div>

            <div className="edge-tabs dim-tabs vertical">
                <button
                    type="button"
                    className={`reopen dim-tab active${slid ? " show" : ""}`}
                    onClick={() => setSlid(false)}
                >
                    Robots
                </button>
            </div>

            {drive.active && state && (
                <Control
                    drive={drive}
                    commands={state.commands}
                    keyboardActive={slid}
                    flash={flash}
                    onFlash={showFlash}
                    onToast={showToast}
                    onSignIn={openAccounts}
                />
            )}

            <div className="dim-toasts">
                <div className={`toast dim-toast warn${toast ? " show" : ""}`} role="status">{toast}</div>
            </div>
        </>
    )
}

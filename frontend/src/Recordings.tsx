// This app's recordings (Desktop's recordings folder, go2/), newest first. The row's button uploads it through
// Desktop's upload queue (Upload → progress → Uploaded); ⋯ has Open in Recordings, Rename, Delete, Cancel upload.
// Auto-upload (persisted, on by default) uploads each finished recording, retries failures, waits while offline.
import { useCallback, useEffect, useState } from "react"
import { call } from "./api.ts"
import { megabytes } from "./Control.tsx"
import { openApp } from "./dim-app/source/desktop.js"
import { Icon } from "./icons.tsx"
import type { Recording, Settings, Upload } from "./state.ts"

const RECORDINGS_APP = "dim-recordings"

function when(ms: number): string {
    const date = new Date(ms)
    const today = new Date()
    const time = date.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
    if (date.toDateString() === today.toDateString()) {
        return `Today ${time}`
    }
    return `${
        date.toLocaleDateString([], {
            month: "short",
            day: "numeric",
            year: date.getFullYear() === today.getFullYear() ? undefined : "numeric",
        })
    } ${time}`
}

function duration(seconds: number | null): string | null {
    if (seconds == null) {
        return null
    }
    const s = Math.round(seconds)
    return s >= 60 ? `${Math.floor(s / 60)} min ${s % 60} s` : `${s} s`
}

function percent(upload: Upload): number | null {
    return upload.bytesTotal ? Math.min(100, Math.round((100 * (upload.bytesDone ?? 0)) / upload.bytesTotal)) : null
}

/** The row's main button: what it does now, what state it's in. */
function UploadButton({ rec, onError, ctx }: { rec: Recording; onError: (text: string) => void; ctx: Ctx }) {
    const upload = rec.upload
    // signed out: queue it anyway (Desktop holds it and sends it after sign-in) and ask to sign in
    const start = () => {
        call("POST", `api/recordings/${encodeURIComponent(rec.file)}/upload`).catch((e) => onError(e.message))
        if (ctx.account && ctx.account.available && !ctx.account.loggedIn) {
            ctx.signIn()
        }
    }
    if (rec.recording) {
        return <span className="rec-state dim-badge danger">Recording…</span>
    }
    switch (upload?.state) {
        case "done":
            return (
                <span className="rec-state dim-badge ok" title={upload.link ? `Uploaded: ${upload.link}` : "Uploaded"}>
                    <Icon name="check" size={12} />
                    Uploaded
                </span>
            )
        case "queued":
            return <span className="rec-state dim-badge info">Waiting to upload…</span>
        case "uploading": {
            const p = percent(upload)
            return (
                <span
                    className="rec-state uploading dim-badge info"
                    style={{ "--p": `${p ?? 0}%` } as React.CSSProperties}
                >
                    Uploading{p != null ? ` ${p}%` : "…"}
                </span>
            )
        }
        case "offline":
            return <span className="rec-state dim-badge warn" title={upload.error ?? ""}>Offline — will upload</span>
        case "signin":
            return (
                <button
                    type="button"
                    className="dim-btn sm"
                    title="It waits for a Dimensional cloud sign-in, then uploads by itself"
                    onClick={ctx.signIn}
                >
                    Sign in to upload…
                </button>
            )
        case "failed":
            return (
                <button
                    type="button"
                    className="dim-btn sm warn"
                    title={upload.error ?? "the upload failed"}
                    onClick={start}
                >
                    <Icon name="refresh" size={12} />
                    {upload.auto && upload.retryAt ? "Failed — retrying" : "Retry upload"}
                </button>
            )
        default:
            return (
                <button type="button" className="dim-btn sm primary" onClick={start}>
                    <Icon name="upload" size={12} />
                    Upload
                </button>
            )
    }
}

function Row({ rec, onError, ctx }: { rec: Recording; onError: (text: string) => void; ctx: Ctx }) {
    const [menu, setMenu] = useState(false)
    const [renaming, setRenaming] = useState(false)
    const [confirmDelete, setConfirmDelete] = useState(false)
    const uploading = ["queued", "uploading", "offline", "signin"].includes(rec.upload?.state ?? "")
    const file = encodeURIComponent(rec.file)
    const rename = (name: string) => {
        setRenaming(false)
        if (name.trim() && name.trim() !== rec.name) {
            call("PUT", `api/recordings/${file}/name`, { name: name.trim() }).catch((e) => onError(e.message))
        }
    }
    const details = [when(rec.startedAt), duration(rec.seconds), megabytes(rec.bytes)].filter(Boolean).join(" · ")
    return (
        <div className={`rec-row${rec.recording ? " live" : ""}`}>
            <div className="rec-meta">
                {renaming
                    ? (
                        <input
                            className="dim-input rec-rename"
                            defaultValue={rec.name}
                            autoFocus
                            onFocus={(e) => e.currentTarget.select()}
                            onBlur={(e) => rename(e.currentTarget.value)}
                            onKeyDown={(e) => {
                                if (e.key === "Enter") {
                                    e.currentTarget.blur()
                                } else if (e.key === "Escape") {
                                    setRenaming(false)
                                }
                            }}
                        />
                    )
                    : <div className="rec-name" title={rec.path}>{rec.name}</div>}
                <div className="rec-sub">
                    {details}
                    {rec.mock && <span className="dim-badge warn">mock</span>}
                    {rec.recovered && (
                        <span
                            className="dim-badge"
                            title="The app stopped mid-recording; everything up to then was kept"
                        >
                            recovered
                        </span>
                    )}
                </div>
                {rec.upload?.state === "failed" && rec.upload.error && <div className="rec-err">{rec.upload.error}
                </div>}
            </div>
            <div className="rec-acts">
                <UploadButton rec={rec} onError={onError} ctx={ctx} />
                <div className="rec-more">
                    <button type="button" className="dim-btn ghost icon sm" title="More" onClick={() => setMenu(!menu)}>
                        <Icon name="more-horizontal" size={15} />
                    </button>
                </div>
            </div>
            {menu && (
                <div className="rec-menu">
                    <button
                        type="button"
                        disabled={!rec.id || rec.recording}
                        onClick={() => {
                            setMenu(false)
                            openApp(RECORDINGS_APP, { path: `#/replay/${encodeURIComponent(rec.id ?? "")}` }).then((
                                ok,
                            ) => ok ||
                                onError("Recordings isn't installed (App Store), or this page isn't inside Desktop")
                            )
                        }}
                    >
                        <Icon name="play" size={13} />
                        Open in Recordings
                    </button>
                    {uploading && (
                        <button
                            type="button"
                            onClick={() => {
                                setMenu(false)
                                call("POST", `api/recordings/${file}/upload/cancel`).catch((e) => onError(e.message))
                            }}
                        >
                            <Icon name="close" size={13} />
                            Cancel upload
                        </button>
                    )}
                    {rec.upload?.link && (
                        <a
                            href={rec.upload.link}
                            target="_blank"
                            rel="noreferrer"
                            onClick={() => setMenu(false)}
                        >
                            <Icon name="link" size={13} />
                            Open uploaded
                        </a>
                    )}
                    <button
                        type="button"
                        disabled={rec.recording || uploading}
                        onClick={() => {
                            setMenu(false)
                            setRenaming(true)
                        }}
                    >
                        <Icon name="edit" size={13} />
                        Rename
                    </button>
                    <button
                        type="button"
                        className="danger"
                        disabled={rec.recording}
                        onClick={() => {
                            setMenu(false)
                            setConfirmDelete(true)
                        }}
                    >
                        <Icon name="trash" size={13} />
                        Delete…
                    </button>
                </div>
            )}
            {confirmDelete && (
                <div className="rec-confirm">
                    Delete{" "}
                    {rec.name}? This can't be undone{rec.upload?.state === "done" ? " (the uploaded copy stays)" : ""}.
                    <button type="button" className="dim-btn sm" onClick={() => setConfirmDelete(false)}>Keep</button>
                    <button
                        type="button"
                        className="dim-btn sm danger"
                        onClick={() => {
                            setConfirmDelete(false)
                            call("DELETE", `api/recordings/${file}`).catch((e) => onError(e.message))
                        }}
                    >
                        Delete
                    </button>
                </div>
            )}
        </div>
    )
}

/** Desktop's Dimensional cloud account (GET api/cloud) */
type Account = { loggedIn: boolean; email: string | null; available: boolean; error?: string | null }
type Ctx = { account: Account | null; signIn: () => void }

/** Desktop's own sign-in page in a frame (GET /dimos/cloud/login/page, the same one Recordings uses): it shows a code,
 * opens the console in a new tab to approve it, and tells this page when it's done. */
function LoginPanel({ onDone, onClose }: { onDone: () => void; onClose: () => void }) {
    useEffect(() => {
        const onMessage = (event: MessageEvent) => {
            if (event.data?.type === "dimos-cloud-login" && ["approved", "loggedIn"].includes(event.data.state)) {
                onDone()
            }
        }
        addEventListener("message", onMessage)
        return () => removeEventListener("message", onMessage)
    }, [onDone])
    const src = new URL("../../dimos/cloud/login/page?theme=dark", location.href).href
    return (
        <div className="rec-login">
            <div className="rec-login-head">
                Sign in to the Dimensional cloud (console.dimensional.org) to upload. Waiting uploads start by
                themselves after.
                <button type="button" className="dim-btn ghost sm" onClick={onClose}>Close</button>
            </div>
            <iframe title="Dimensional sign-in" src={src} />
        </div>
    )
}

function AccountLine({ account, onSignIn, onSignOut }: {
    account: Account | null
    onSignIn: () => void
    onSignOut: () => void
}) {
    if (!account) {
        return <span className="rec-account muted">Checking the cloud account…</span>
    }
    if (!account.available) {
        return <span className="rec-account muted" title={account.error ?? ""}>Uploads need dimOS Desktop</span>
    }
    if (account.loggedIn) {
        return (
            <span className="rec-account">
                Signed in as <b>{account.email ?? "your account"}</b> ·{" "}
                <button type="button" className="rec-link" onClick={onSignOut}>Sign out</button>
            </span>
        )
    }
    return (
        <button type="button" className="dim-btn sm primary" onClick={onSignIn}>
            Sign in to upload
        </button>
    )
}

export function Recordings(
    { recordings, settings, onClose }: { recordings: Recording[]; settings: Settings; onClose: () => void },
) {
    const [error, setError] = useState<string | null>(null)
    const [account, setAccount] = useState<Account | null>(null)
    const [loggingIn, setLoggingIn] = useState(false)
    const loadAccount = useCallback((fresh = false) => {
        call<Account>("GET", `api/cloud${fresh ? "?fresh=true" : ""}`).then(
            setAccount,
            (e) => setAccount({ loggedIn: false, email: null, available: true, error: e.message }),
        )
    }, [])
    useEffect(() => loadAccount(true), [])
    // an upload Desktop holds for a sign-in: ask for one
    const waiting = recordings.some((r) => r.upload?.state === "signin")
    useEffect(() => {
        if (waiting && account && account.available && !account.loggedIn) {
            setLoggingIn(true)
        }
    }, [waiting, account?.loggedIn])
    const onLoggedIn = useCallback(() => {
        setLoggingIn(false)
        loadAccount(true)
    }, [])
    const ctx: Ctx = { account, signIn: () => setLoggingIn(true) }
    useEffect(() => {
        const key = (e: KeyboardEvent) => e.key === "Escape" && onClose()
        addEventListener("keydown", key)
        return () => removeEventListener("keydown", key)
    }, [])
    // always newest first, whatever order arrives
    const sorted = [...recordings].sort((a, b) => b.startedAt - a.startedAt)
    const pending = sorted.filter((r) => r.upload?.state === "offline").length
    return (
        <div className="rec-layer" onClick={onClose}>
            <div
                className="rec-dialog dim-panel"
                onClick={(e) => e.stopPropagation()}
                role="dialog"
                aria-label="Recordings"
            >
                <div className="rec-head">
                    <span className="rec-title">Recordings</span>
                    <span className="rec-count">{sorted.length}</span>
                    <span className="spacer" />
                    <label
                        className="rec-auto"
                        title="Upload each recording when it ends (through Desktop's upload queue); failures are retried, and while offline it waits"
                    >
                        <input
                            type="checkbox"
                            checked={settings.autoUpload}
                            onChange={(e) =>
                                call("PUT", "api/settings", { autoUpload: e.target.checked }).catch((err) =>
                                    setError(err.message)
                                )}
                        />
                        Auto-upload
                        {settings.autoUpload && (
                            <span className={`dim-badge ${pending ? "warn" : "ok"}`}>
                                {pending ? `${pending} waiting for network` : "on"}
                            </span>
                        )}
                    </label>
                    <button type="button" className="dim-btn ghost icon sm" title="Close (Esc)" onClick={onClose}>
                        <Icon name="close" size={15} />
                    </button>
                </div>
                <div className="rec-accountbar">
                    <AccountLine
                        account={account}
                        onSignIn={() => setLoggingIn(true)}
                        onSignOut={() =>
                            call<Account>("POST", "api/cloud/logout").then(setAccount, (e) => setError(e.message))}
                    />
                </div>
                {loggingIn && <LoginPanel onDone={onLoggedIn} onClose={() => setLoggingIn(false)} />}
                {error && (
                    <div className="rec-error dim-alert danger" onClick={() => setError(null)}>
                        {error}
                    </div>
                )}
                <div className="rec-list">
                    {sorted.length === 0 && (
                        <div className="rec-empty">
                            No recordings yet. Recording starts automatically when you connect to a dog.
                        </div>
                    )}
                    {sorted.map((rec) => <Row key={rec.file} rec={rec} onError={setError} ctx={ctx} />)}
                </div>
            </div>
        </div>
    )
}

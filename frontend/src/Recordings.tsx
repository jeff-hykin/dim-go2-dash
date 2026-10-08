// This app's recordings (Desktop's recordings folder, go2/), newest first. The row's button uploads it through
// Desktop's upload queue (Upload → progress → Uploaded); ⋯ has Open in Recordings, Rename, Delete, Cancel upload.
// Auto-upload (persisted, off by default) uploads each finished recording, retries failures, waits while offline.
import { useEffect, useState } from "react"
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
function UploadButton({ rec, onError }: { rec: Recording; onError: (text: string) => void }) {
    const upload = rec.upload
    const start = () =>
        call("POST", `api/recordings/${encodeURIComponent(rec.file)}/upload`).catch((e) => onError(e.message))
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
                    title="Desktop needs a Dimensional cloud sign-in before it uploads (the upload starts by itself after)"
                    onClick={() => openApp(RECORDINGS_APP, { path: "#/transfer" })}
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

function Row({ rec, onError }: { rec: Recording; onError: (text: string) => void }) {
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
            <UploadButton rec={rec} onError={onError} />
            <div className="rec-more">
                <button type="button" className="dim-btn ghost icon sm" title="More" onClick={() => setMenu(!menu)}>
                    <Icon name="more-horizontal" size={15} />
                </button>
                {menu && (
                    <div className="rec-menu dim-panel" onMouseLeave={() => setMenu(false)}>
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
                                    call("POST", `api/recordings/${file}/upload/cancel`).catch((e) =>
                                        onError(e.message)
                                    )
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
            </div>
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

export function Recordings(
    { recordings, settings, onClose }: { recordings: Recording[]; settings: Settings; onClose: () => void },
) {
    const [error, setError] = useState<string | null>(null)
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
                {error && (
                    <div className="rec-error dim-alert danger" onClick={() => setError(null)}>
                        {error}
                    </div>
                )}
                <div className="rec-list">
                    {sorted.length === 0 && (
                        <div className="rec-empty">
                            No recordings yet. Connect to a dog, then press <b>Record</b>.
                        </div>
                    )}
                    {sorted.map((rec) => <Row key={rec.file} rec={rec} onError={setError} />)}
                </div>
            </div>
        </div>
    )
}

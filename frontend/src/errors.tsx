import { useEffect, useState } from "react"
import { notify } from "./dim-app/source/notify.js"

const recent = new Map<string, number>()
export function reportError(message: string, title = "Go2 Ctrl") {
    const now = Date.now()
    if (now - (recent.get(message) ?? 0) < 10000) return Promise.resolve("already-reported")
    recent.set(message, now)
    for (const [key, time] of recent) if (now - time > 10000) recent.delete(key)
    return notify({ title, body: message, kind: "warn", app: "dim-go2-dash" })
}

/** Desktop supplies dismissal/history; standalone or unavailable Desktop gets a closeable fallback. */
export function ErrorNotice({ message }: { message?: string | null }) {
    const [fallback, setFallback] = useState<string | null>(null)
    useEffect(() => {
        let live = true
        setFallback(null)
        if (message) {
            reportError(message).then((id) => {
                if (live && id === null) setFallback(message)
            })
        }
        return () => {
            live = false
        }
    }, [message])
    return fallback
        ? (
            <div className="dim-alert danger" role="alert">
                <span>{fallback}</span>
                <button
                    type="button"
                    className="dim-btn sm"
                    aria-label="Dismiss error"
                    onClick={() => setFallback(null)}
                >
                    ×
                </button>
            </div>
        )
        : null
}

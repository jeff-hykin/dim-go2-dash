// Small page-only helpers: clipboard, colours, input checks, and per-viewer conveniences kept in localStorage.

/** Copy, with a fallback for sandboxed iframes where navigator.clipboard is blocked. */
export async function copyText(text: string): Promise<boolean> {
    try {
        await navigator.clipboard.writeText(text)
        return true
    } catch {
        // fall through
    }
    try {
        const area = document.createElement("textarea")
        area.value = text
        area.style.position = "fixed"
        area.style.opacity = "0"
        document.body.appendChild(area)
        area.focus()
        area.select()
        const ok = document.execCommand("copy")
        area.remove()
        return ok
    } catch {
        return false
    }
}

/** A deterministic pleasant colour from a robot's Bluetooth id. */
export function colorFor(id: string): string {
    let h = 0
    for (const c of id) {
        h = (h * 31 + c.charCodeAt(0)) >>> 0
    }
    return `hsl(${h % 360} 62% 55%)`
}

export function validIp(text: string): boolean {
    const t = text.trim()
    return /^(\d{1,3})(\.\d{1,3}){3}$/.test(t) && t.split(".").every((n) => Number(n) <= 255)
}

export function stored<T>(key: string, fallback: T): T {
    try {
        const raw = localStorage.getItem(key)
        return raw === null ? fallback : (JSON.parse(raw) as T)
    } catch {
        return fallback
    }
}

export function store(key: string, value: unknown) {
    try {
        localStorage.setItem(key, JSON.stringify(value))
    } catch {
        // private mode
    }
}

export function agoText(ms: number): string {
    const s = Math.max(0, Math.round((Date.now() - ms) / 1000))
    if (s < 60) {
        return "just now"
    }
    if (s < 3600) {
        return `${Math.round(s / 60)} min ago`
    }
    if (s < 86400) {
        return `${Math.round(s / 3600)} h ago`
    }
    return `${Math.round(s / 86400)} d ago`
}

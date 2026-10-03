// The app's backend API (backend/src/routes.rs), by relative URL: the page lives at Desktop's /apps/<name>/.
export class ApiError extends Error {}

export async function call<T = unknown>(method: string, path: string, body?: unknown): Promise<T> {
    const response = await fetch(path, {
        method,
        headers: body === undefined ? undefined : { "content-type": "application/json" },
        body: body === undefined ? undefined : JSON.stringify(body),
    })
    const data = await response.json().catch(() => null)
    if (!response.ok) {
        throw new ApiError(data?.error ?? `${response.status} ${response.statusText}`)
    }
    return data as T
}

export type BackendEvent = { type?: string; [key: string]: unknown }

/** The backend's events (api/events/ws), reconnecting with backoff; `onReconnect` runs after a dropped link comes back
 * (events may have been missed). Returns an unsubscribe. */
export function events(onEvent: (event: BackendEvent) => void, onReconnect?: () => void): () => void {
    let socket: WebSocket | null = null
    let delay = 500
    let stopped = false
    let dropped = false
    const open = () => {
        const url = new URL("api/events/ws", location.href)
        url.protocol = url.protocol.replace("http", "ws")
        socket = new WebSocket(url)
        socket.onopen = () => {
            delay = 500
            if (dropped) {
                dropped = false
                onReconnect?.()
            }
        }
        socket.onmessage = (message) => {
            try {
                onEvent(JSON.parse(message.data))
            } catch {
                // not JSON
            }
        }
        socket.onclose = () => {
            if (!stopped) {
                dropped = true
                setTimeout(open, delay)
                delay = Math.min(delay * 2, 10_000)
            }
        }
    }
    open()
    return () => {
        stopped = true
        socket?.close()
    }
}

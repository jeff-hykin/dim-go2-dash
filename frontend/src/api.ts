// The app's backend API (backend/src/routes.rs), by relative URL: the page lives at Desktop's /apps/<name>/.
import { reportError } from "./errors.tsx"
import { appEvents } from "./dim-app/source/events.js"

export class ApiError extends Error {}

/** `quiet`: a failure only throws, without a Desktop notification (calls that retry on their own) */
export async function call<T = unknown>(
    method: string,
    path: string,
    body?: unknown,
    { quiet = false }: { quiet?: boolean } = {},
): Promise<T> {
    const response = await fetch(path, {
        method,
        headers: body === undefined ? undefined : { "content-type": "application/json" },
        body: body === undefined ? undefined : JSON.stringify(body),
    })
    const data = await response.json().catch(() => null)
    if (!response.ok) {
        const message = data?.error ?? `${response.status} ${response.statusText}`
        if (!quiet) void reportError(message)
        throw new ApiError(message)
    }
    return data as T
}

export type BackendEvent = { type?: string; [key: string]: unknown }

/** The backend's events (its frontend zenoh topic `events`, on the page's one zenoh-gateway connection); `onConnect` runs
 * each time that connection comes up (events sent before, or while it was down, are missed: re-GET). Returns an
 * unsubscribe. */
export function events(
    onEvent: (event: BackendEvent) => void,
    onConnect?: () => void,
    onDisconnect?: () => void,
): () => void {
    return appEvents(onEvent, { onOpen: () => onConnect?.(), onClose: () => onDisconnect?.() })
}

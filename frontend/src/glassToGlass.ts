// Glass-to-glass latency (ported from dim-controller): this page shows a QR code carrying the time it went up; held to
// the robot's camera, it comes back in the camera picture, and the frame that shows it is timed against the code's time.
// Both are this page's clock, so the code is shown and read on the same device. The picture is the WebRTC <video> or,
// in WebSocket mode, the canvas the page decodes into (it fires "framepainted" after each frame it paints).
import qrcode from "qrcode-generator"

const PREFIX = "g2g:"
/** the code's time wraps here (ms), so it stays a short code (a small, coarse QR reads from further away) */
const WRAP = 1e7
/** frames are scanned at most this wide (jsQR's time grows with the pixels) */
const SCAN_WIDTH = 640
/** results kept per camera (the stats are over these) */
const KEEP = 200

/** The QR modules for `text`: a square of booleans (true: dark). */
export function qrModules(text: string): boolean[][] {
    const code = qrcode(0, "M")
    code.addData(text)
    code.make()
    const size = code.getModuleCount()
    return Array.from({ length: size }, (_, row) => Array.from({ length: size }, (_, col) => code.isDark(row, col)))
}

export const encodeTime = (ms: number) => `${PREFIX}${Math.floor(ms % WRAP)}`

/** the code's time back (null: not one of ours) */
export function decodeTime(text: string): number | null {
    if (!text.startsWith(PREFIX)) {
        return null
    }
    const value = Number(text.slice(PREFIX.length))
    return Number.isInteger(value) && value >= 0 ? value : null
}

/** how long ago (ms) a code drawn at `codeMs` (wrapped) was, seen at `shownMs` */
export const elapsed = (codeMs: number, shownMs: number) => ((shownMs - codeMs) % WRAP + WRAP) % WRAP

/** each code stays up this many camera frames, so a whole camera frame (its exposure included) sees it unblended */
const HOLD_FRAMES = 2
/** the slowest watched camera's frame interval (ms), measured from the frames its <video> shows; null before any */
let cameraFrameMs: number | null = null
/** how long each code stays up before one is measured: a 2 Hz camera's two frames, slower than any camera here */
const UNMEASURED_HOLD_MS = 1000

/** How long each code stays up now (ms): HOLD_FRAMES of the slowest camera's frames. */
export const holdMs = () => cameraFrameMs === null ? UNMEASURED_HOLD_MS : HOLD_FRAMES * cameraFrameMs

/**
 * Draws a new code into `canvas` every holdMs() (a camera slower than the screen saw two codes blended into one
 * frame when it changed every screen frame), each carrying the time it first went up, until the returned function is
 * called.
 */
export function showCode(canvas: HTMLCanvasElement, gatewayNow: () => number): () => void {
    const context = canvas.getContext("2d")!
    let frame = 0
    let shownAt = -Infinity
    const draw = (now: number) => {
        if (now - shownAt < holdMs()) {
            frame = requestAnimationFrame(draw)
            return
        }
        shownAt = now
        const modules = qrModules(encodeTime(gatewayNow()))
        const quiet = 4
        const cells = modules.length + quiet * 2
        const cell = Math.floor(canvas.width / cells)
        const offset = Math.floor((canvas.width - cell * cells) / 2) + quiet * cell
        context.fillStyle = "#fff"
        context.fillRect(0, 0, canvas.width, canvas.height)
        context.fillStyle = "#000"
        modules.forEach((row, y) => row.forEach((dark, x) => dark && context.fillRect(offset + x * cell, offset + y * cell, cell, cell)))
        frame = requestAnimationFrame(draw)
    }
    frame = requestAnimationFrame(draw)
    return () => cancelAnimationFrame(frame)
}

export interface CameraResult {
    /** the picture's id */
    panel: string
    samples: number[]
    /** its frames come back blank: this browser keeps the WebRTC video's pixels from the page (Firefox on Android) */
    unreadable?: boolean
}

/** this many blank frames in a row (and no code yet) mark a camera unreadable */
const BLANK_FRAMES = 6

/**
 * Watches every camera panel's <video> for the code: each frame it shows is scanned in a worker (one at a time per
 * camera, so a slow scan skips frames instead of queueing them) and a code seen for the first time is timed from when it was drawn
 * to when that frame reached the screen. `onChange` gets every camera's results so far. Returns what stops it.
 */
/** a picture to scan: the WebRTC <video> or the WebSocket mode's canvas */
export interface Picture {
    id: string
    element: HTMLVideoElement | HTMLCanvasElement
}

export function watchCameras(onChange: (results: CameraResult[]) => void, toGateway: (performanceMs: number) => number, pictures: () => Picture[]): () => void {
    const worker = new Worker(new URL("./glassToGlassWorker.ts", import.meta.url), { type: "module" })
    const results = new Map<string, CameraResult>()
    const busy = new Set<string>()
    const lastCode = new Map<string, number>()
    const frameMs = new Map<string, number>()
    /** frames out at the worker: their camera and when they reached the screen */
    const pending = new Map<number, { panel: string; shownAt: number }>()
    let nextId = 0
    let stopped = false
    const blankRun = new Map<string, number>()
    worker.onmessage = (event: MessageEvent<{ id: number; text: string | null; blank: boolean }>) => {
        const frame = pending.get(event.data.id)
        pending.delete(event.data.id)
        if (!frame || stopped) {
            return
        }
        busy.delete(frame.panel)
        // frames that keep coming back blank: this browser keeps the video's pixels from the page
        const run = event.data.blank ? (blankRun.get(frame.panel) ?? 0) + 1 : 0
        blankRun.set(frame.panel, run)
        const unreadable = run >= BLANK_FRAMES
        const known = results.get(frame.panel)
        if (unreadable !== !!known?.unreadable) {
            results.set(frame.panel, { ...(known ?? { panel: frame.panel, samples: [] }), unreadable })
            onChange([...results.values()])
        }
        const codeMs = event.data.text === null ? null : decodeTime(event.data.text)
        // the same code in two frames: the later one isn't when it arrived
        if (codeMs === null || lastCode.get(frame.panel) === codeMs) {
            return
        }
        lastCode.set(frame.panel, codeMs)
        const result = results.get(frame.panel) ?? { panel: frame.panel, samples: [] }
        result.samples = [...result.samples, elapsed(codeMs, frame.shownAt)].slice(-KEEP)
        results.set(frame.panel, result)
        onChange([...results.values()])
    }
    const scan = async (video: HTMLVideoElement | HTMLCanvasElement, panel: string, shownAt: number) => {
        const sourceWidth = video instanceof HTMLVideoElement ? video.videoWidth : video.width
        const sourceHeight = video instanceof HTMLVideoElement ? video.videoHeight : video.height
        const scale = Math.min(1, SCAN_WIDTH / sourceWidth)
        const width = Math.round(sourceWidth * scale), height = Math.round(sourceHeight * scale)
        if (!width || !height || busy.has(panel)) {
            return
        }
        busy.add(panel)
        try {
            const frame = await createImageBitmap(video, { resizeWidth: width, resizeHeight: height })
            const id = nextId++
            pending.set(id, { panel, shownAt })
            worker.postMessage({ id, frame }, [frame])
        } catch {
            busy.delete(panel)
        }
    }
    const frameTimes = new Map<string, number>()
    /** a picture's frame reached the screen at `shownAt` (performance ms): time it, then scan it */
    const onFrame = (picture: Picture, shownAt: number) => {
        const last = frameTimes.get(picture.id)
        if (last !== undefined) {
            const interval = shownAt - last
            if (interval > 0 && interval < 2000) {
                frameMs.set(picture.id, (frameMs.get(picture.id) ?? interval) * 0.8 + interval * 0.2)
                cameraFrameMs = Math.max(...frameMs.values())
            }
        }
        frameTimes.set(picture.id, shownAt)
        scan(picture.element, picture.id, toGateway(shownAt))
    }
    const watching = new Map<HTMLElement, () => void>()
    const watch = (picture: Picture) => {
        const element = picture.element
        if (element instanceof HTMLVideoElement) {
            let handle = 0
            const next = (_now: number, metadata: VideoFrameCallbackMetadata) => {
                if (stopped) {
                    return
                }
                // when this frame is on the screen (the second glass): never before it went to the compositor
                onFrame(picture, Math.max(metadata.expectedDisplayTime, metadata.presentationTime))
                handle = element.requestVideoFrameCallback(next)
            }
            handle = element.requestVideoFrameCallback(next)
            watching.set(element, () => element.cancelVideoFrameCallback(handle))
        } else {
            const painted = () => !stopped && onFrame(picture, performance.now())
            element.addEventListener("framepainted", painted)
            watching.set(element, () => element.removeEventListener("framepainted", painted))
        }
    }
    // the picture can change (WebRTC <video> or the WebSocket canvas): look again each second
    const find = () => {
        for (const picture of pictures()) {
            if (!watching.has(picture.element)) {
                watch(picture)
            }
        }
    }
    find()
    const finder = setInterval(find, 1000)
    return () => {
        stopped = true
        clearInterval(finder)
        for (const stop of watching.values()) {
            stop()
        }
        worker.terminate()
    }
}

/** the q-th quantile (0..1) of `values` */
export function quantile(values: number[], q: number): number {
    const sorted = [...values].sort((a, b) => a - b)
    return sorted[Math.min(sorted.length - 1, Math.max(0, Math.round(q * (sorted.length - 1))))]
}

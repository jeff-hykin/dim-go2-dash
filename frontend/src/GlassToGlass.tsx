// Glass-to-glass latency test (glassToGlass.ts): a moving QR code to hold up to the robot's camera, and how long from
// the code going up here to the camera's frame showing it being on this screen.
import { useEffect, useRef, useState } from "react"
import {
    type CameraResult,
    holdMs,
    type Picture,
    pixelsBlocked,
    quantile,
    showCode,
    watchCameras,
} from "./glassToGlass.ts"

const CODE_PX = 260

/** a reading as 4 digit-wide places (figure spaces before it), so the numbers keep one width as they change */
const ms4 = (value: number) => String(Math.min(9999, Math.round(value))).padStart(4, "\u2007")

export function GlassToGlass({ pictures, onClose }: { pictures: () => Picture[]; onClose: () => void }) {
    const canvas = useRef<HTMLCanvasElement>(null)
    const [results, setResults] = useState<CameraResult[]>([])
    useEffect(() => {
        const element = canvas.current!
        element.width = element.height = Math.round(CODE_PX * devicePixelRatio)
        // this page's clock both ways: shown and read on the same device
        const now = () => performance.timeOrigin + performance.now()
        const stopCode = showCode(element, now)
        const stopWatch = watchCameras(setResults, (performanceMs) => performance.timeOrigin + performanceMs, pictures)
        return () => {
            stopCode()
            stopWatch()
        }
    }, [])
    const result = results[0]
    return (
        <div className="g2g-overlay" onClick={onClose}>
            <section className="g2g dim-panel glass" onClick={(event) => event.stopPropagation()} data-ui="control.g2g">
                <p className="g2g-intro">
                    Show this to the robot's camera to get a{" "}
                    <a href="https://bloggeek.me/webrtcglossary/glass-to-glass/" target="_blank" rel="noreferrer">
                        glass-to-glass
                    </a>{" "}
                    latency test.
                </p>
                {pixelsBlocked() && (
                    <p className="g2g-unreadable" data-ui="control.g2g-firefox-warning">
                        Firefox on Android won't let the page read its WebRTC video (
                        <a href="https://bugzilla.mozilla.org/show_bug.cgi?id=1479947" target="_blank" rel="noreferrer">
                            bug 1479947
                        </a>), so it can't measure here: open this page in Chrome.
                    </p>
                )}
                <canvas
                    ref={canvas}
                    className="g2g-code"
                    style={{ width: CODE_PX, height: CODE_PX }}
                    aria-label="Glass-to-glass test code"
                />
                {!result
                    ? <p className="g2g-status">Looking for the code in the camera picture…</p>
                    : result.unreadable && !result.samples.length
                    ? (
                        <p className="g2g-unreadable">
                            Its frames come back{" "}
                            {result.unreadable}: this browser won't let the page read its video (Firefox on Android:
                            {" "}
                            <a
                                href="https://bugzilla.mozilla.org/show_bug.cgi?id=1479947"
                                target="_blank"
                                rel="noreferrer"
                            >
                                bug 1479947
                            </a>): use Chrome, or the WebSocket video format.
                        </p>
                    )
                    : (
                        <div className="g2g-stats dim-mono">
                            <span>
                                typical <b>{ms4(quantile(result.samples, 0.5))} ms</b>
                            </span>
                            <span>
                                spike <b>{ms4(quantile(result.samples, 0.95))} ms</b>
                            </span>
                            <span>
                                best <b>{ms4(Math.min(...result.samples))} ms</b>
                            </span>
                            <span>{result.samples.length} frames</span>
                        </div>
                    )}
                <p className="g2g-note">
                    From this page drawing the code to the camera's frame showing it reaching this screen (typical is
                    the median, spike the p95). Typical is what you see: it includes waiting for the camera's next frame
                    (a change in front of the robot waits the same), so a slow camera reads high; best is the luckiest
                    frame. Each code stays up two camera frames ({Math.round(holdMs())} ms).
                </p>
                <button type="button" className="dim-btn sm" onClick={onClose}>Stop</button>
            </section>
        </div>
    )
}

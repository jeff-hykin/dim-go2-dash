// Glass-to-glass latency test (glassToGlass.ts): a moving QR code to hold up to the robot's camera, and how long from
// the code going up here to the camera's frame showing it being on this screen.
import { useEffect, useRef, useState } from "react"
import { type CameraResult, holdMs, type Picture, quantile, showCode, watchCameras } from "./glassToGlass.ts"

const CODE_PX = 260

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
                    <a href="https://bloggeek.me/webrtcglossary/glass-to-glass/" target="_blank" rel="noreferrer">glass-to-glass</a>{" "}
                    latency test.
                </p>
                <canvas ref={canvas} className="g2g-code" style={{ width: CODE_PX, height: CODE_PX }} aria-label="Glass-to-glass test code" />
                {!result
                    ? <p className="g2g-status">Looking for the code in the camera picture…</p>
                    : result.unreadable && !result.samples.length
                    ? (
                        <p className="g2g-unreadable">
                            This browser won't let the page read its video (Firefox on Android:{" "}
                            <a href="https://bugzilla.mozilla.org/show_bug.cgi?id=1479947" target="_blank" rel="noreferrer">bug 1479947</a>): use
                            Chrome, or the WebSocket video format.
                        </p>
                    )
                    : (
                        <div className="g2g-stats dim-mono">
                            <span>typical <b>{Math.round(quantile(result.samples, 0.5))} ms</b></span>
                            <span>spike <b>{Math.round(quantile(result.samples, 0.95))} ms</b></span>
                            <span>best <b>{Math.round(Math.min(...result.samples))} ms</b></span>
                            <span>{result.samples.length} frames</span>
                        </div>
                    )}
                <p className="g2g-note">
                    From this page drawing the code to the camera's frame showing it reaching this screen (typical is the median, spike
                    the p95). Each code stays up for two of the camera's frames ({Math.round(holdMs())} ms), so a reading runs up to one
                    camera frame high; best is the closest.
                </p>
                <button type="button" className="dim-btn sm" onClick={onClose}>Stop</button>
            </section>
        </div>
    )
}

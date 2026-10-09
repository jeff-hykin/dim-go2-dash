import { useEffect, useState } from "react"
import { call } from "./api.ts"
import { openApp } from "./dim-app/source/desktop.js"
import type { RecordState } from "./state.ts"

const TOPICS = [
    "/color_image",
    "/camera_info",
    "/lidar",
    "/odom",
    "/tf",
    "/imu",
    "/battery",
    "/joint_states",
    "/joystick",
    "/cmd_vel",
    "/robot_action",
    "/logs",
]
type Options = {
    directory: string
    compression: string
    imageFormat: string
    recordNew: boolean
    logs: boolean
    topics: Record<string, boolean>
    rates: Record<string, number>
}
type Settings = { autoUpload: boolean; recordOptions: Options }
const defaults: Options = {
    directory: "",
    compression: "zstd",
    imageFormat: "jpeg",
    recordNew: true,
    logs: false,
    topics: {},
    rates: {},
}

export function RecordOptions(
    { record, onClose, onError }: { record: RecordState; onClose: () => void; onError: (text: string) => void },
) {
    const [settings, setSettings] = useState<Settings>({ autoUpload: true, recordOptions: defaults })
    const [loaded, setLoaded] = useState(false)
    const [busy, setBusy] = useState(false)
    const [advanced, setAdvanced] = useState(false)
    const [folder, setFolder] = useState("")
    useEffect(() => {
        call("GET", "api/settings").then((result) => {
            const value = result as Settings
            setSettings(value)
            setFolder(value.recordOptions.directory)
            setLoaded(true)
        }).catch((error) => onError(error.message))
    }, [])
    const options = settings.recordOptions
    const save = (patch: Partial<Options>) => {
        setBusy(true)
        call("PUT", "api/settings", { recordOptions: { ...options, ...patch } })
            .then((value) => setSettings(value as Settings)).catch((error) => onError(error.message)).finally(() =>
                setBusy(false)
            )
    }
    const streams = record.active ? (record.streams ?? []) : []
    const top = [...streams].sort((a, b) => b.bytesPerSecond - a.bytesPerSecond).slice(0, 5)
    const enabled = (topic: string) => options.topics[topic] !== false && (topic !== "/logs" || options.logs)
    const toggle = (topic: string, value: boolean) => save({ topics: { ...options.topics, [topic]: value } })
    const rate = (bytes: number) => `${(bytes / 1e6).toFixed(2)} MB/s`
    return (
        <div
            className="go2-record-options dim-panel"
            role="dialog"
            aria-label="Recording options"
            onKeyDown={(event) => event.stopPropagation()}
        >
            <div className="rec-options-head">
                <b>Recording options</b>
                <button type="button" className="dim-btn sm" onClick={onClose}>Close</button>
            </div>
            <fieldset disabled={!loaded || busy}>
                <label>
                    Folder<input
                        className="dim-input"
                        value={folder}
                        placeholder="Desktop’s recordings folder / go2"
                        disabled={record.active}
                        onChange={(event) => setFolder(event.target.value)}
                        onBlur={() => folder !== options.directory && save({ directory: folder })}
                    />
                </label>
                {record.active && (
                    <div className="rec-path">
                        {record.path}
                        <br />
                        {record.messages.toLocaleString()} messages · {record.dropped} dropped · {record.skipped ?? 0}
                        {" "}
                        rate-limited / excluded
                    </div>
                )}
                <label className="rec-check">
                    <input
                        type="checkbox"
                        checked={settings.autoUpload}
                        onChange={(event) => {
                            setBusy(true)
                            call("PUT", "api/settings", { autoUpload: event.target.checked }).then((value) =>
                                setSettings(value as Settings)
                            ).catch((error) => onError(error.message)).finally(() => setBusy(false))
                        }}
                    />Auto-upload saved recordings
                </label>
                <h3>Biggest streams</h3>
                {top.map((stream) => (
                    <label className="rec-stream" key={stream.topic}>
                        <input
                            type="checkbox"
                            checked={enabled(stream.topic)}
                            onChange={(event) => toggle(stream.topic, event.target.checked)}
                        />
                        <span>{stream.topic}</span>
                        <small>{rate(stream.bytesPerSecond)} avg</small>
                    </label>
                ))}
                {!top.length && <p>Streams appear here as messages arrive.</p>}
                <button type="button" className="dim-btn sm" onClick={() => openApp("dim-recordings")}>
                    See recordings
                </button>
                <button
                    type="button"
                    className="dim-btn sm"
                    aria-expanded={advanced}
                    onClick={() => setAdvanced(!advanced)}
                >
                    Advanced {advanced ? "▴" : "▾"}
                </button>
                {advanced && (
                    <>
                        <label>
                            Images<select
                                className="dim-input"
                                value={options.imageFormat}
                                disabled={record.active}
                                onChange={(event) => save({ imageFormat: event.target.value })}
                            >
                                <option value="jpeg">JPEG (small)</option>
                                <option value="raw">Raw RGB (exact decoded pixels)</option>
                            </select>
                        </label>
                        <label>
                            Chunks<select
                                className="dim-input"
                                value={options.compression}
                                disabled={record.active}
                                onChange={(event) => save({ compression: event.target.value })}
                            >
                                <option value="zstd">Zstd</option>
                                <option value="none">Uncompressed</option>
                            </select>
                        </label>
                        <label className="rec-check">
                            <input
                                type="checkbox"
                                checked={options.recordNew}
                                onChange={(event) => save({ recordNew: event.target.checked })}
                            />Record new streams as they appear
                        </label>
                        <label className="rec-check">
                            <input
                                type="checkbox"
                                checked={options.logs}
                                onChange={(event) => save({ logs: event.target.checked })}
                            />Include Go2 session event logs
                        </label>
                        <div className="rec-stream legend">
                            <span />
                            <span>Stream</span>
                            <span>Max Hz</span>
                        </div>
                        {TOPICS.map((topic) => (
                            <div className="rec-stream" key={topic}>
                                <input
                                    type="checkbox"
                                    aria-label={`Record ${topic}`}
                                    checked={enabled(topic)}
                                    onChange={(event) => toggle(topic, event.target.checked)}
                                />
                                <span>{topic}</span>
                                <input
                                    className="dim-input"
                                    type="number"
                                    min="0.01"
                                    max="1000"
                                    step="any"
                                    placeholder="All"
                                    aria-label={`Max rate ${topic}`}
                                    defaultValue={options.rates[topic] ?? ""}
                                    key={`${topic}-${options.rates[topic]}`}
                                    onBlur={(event) => {
                                        const rates = { ...options.rates }
                                        if (!event.target.value) delete rates[topic]
                                        else rates[topic] = Number(event.target.value)
                                        if (JSON.stringify(rates) !== JSON.stringify(options.rates)) save({ rates })
                                    }}
                                />
                            </div>
                        ))}
                    </>
                )}
            </fieldset>
        </div>
    )
}

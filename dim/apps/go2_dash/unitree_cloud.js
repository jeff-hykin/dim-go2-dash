// Unitree cloud client — the same calls the official Go2 app makes, used to fetch
// each bound robot's per-device AES-128 key (the data2=3 LAN handshake on Go2
// firmware ≥ 1.1.15 / G1 ≥ 1.5.1). Mirrors unitree_webrtc_connect/unitree_cloud.py.
import { createHash } from "node:crypto"

const BASE_URLS = {
    global: "https://global-robot-api.unitree.com/",
    cn: "https://robot-api.unitree.com/",
}
const APP_SIGN_SECRET = "XyvkwK45hp5PHfA8"
// Header set copied from the apk verbatim — the cloud is picky (a wrong
// AppVersion flips the response from code 100 to 1003).
const BASE_HEADERS = {
    "Content-Type": "application/x-www-form-urlencoded",
    "DeviceId": "Samsung/Samsung/SM-S931B/s24/14/34",
    "DevicePlatform": "Android",
    "DeviceModel": "SM-S931B",
    "SystemVersion": "34",
    "AppVersion": "1.11.4",
    "AppLocale": "en_US",
    "Channel": "UMENG_CHANNEL",
    "User-Agent": "Mozilla/5.0 (Linux; Android 14; SM-S931B Build/AP3A.240905.015.A2; wv) "
        + "AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/127.0.6533.103 Mobile Safari/537.36",
}

const md5 = (text) => createHash("md5").update(text).digest("hex")

function signedHeaders(token) {
    const ts = String(Date.now())
    const nonce = crypto.randomUUID().replace(/-/g, "")
    const tz = new Date().toLocaleTimeString("en-US", { timeZoneName: "short" }).split(" ").pop() || "UTC"
    return {
        ...BASE_HEADERS,
        AppTimezone: tz,
        AppTimestamp: ts,
        AppNonce: nonce,
        AppSign: md5(APP_SIGN_SECRET + ts + nonce),
        AppName: "Go2",
        Token: token,
    }
}

async function call(region, method, path, params, token) {
    const base = BASE_URLS[region]
    if (!base) throw new Error(`unknown Unitree cloud region "${region}" (global or cn)`)
    const body = new URLSearchParams(params || {})
    const url = method === "GET" ? `${base}${path}?${body}` : base + path
    const res = await fetch(url, { method, headers: signedHeaders(token), body: method === "GET" ? undefined : body })
    if (!res.ok) throw new Error(`Unitree cloud ${path}: HTTP ${res.status}`)
    const result = await res.json()
    if (result.code !== 100) throw new Error(`Unitree cloud ${path} failed (code ${result.code}${result.errorMsg ? ": " + result.errorMsg : ""})`)
    return result.data
}

/** Sign in and list every robot bound to the account: [{ sn, alias, key }].
 *  `key` is empty for firmware below the data2=3 cutover (no key needed there). */
export async function fetchBoundRobots({ email, password, region = "global" }) {
    const login = await call(region, "POST", "login/email", { email, password: md5(password) }, "")
    const devices = await call(region, "GET", "device/bind/list", {}, (login && login.accessToken) || "")
    return (devices || []).map((d) => ({ sn: d.sn || "", alias: d.alias || "", key: d.key || d.gcm_key || "" }))
}

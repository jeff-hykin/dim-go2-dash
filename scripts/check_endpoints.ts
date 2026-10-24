// dimos.yaml's `provides:` (an OpenAPI 3.1 document) must list exactly the backend's routes (method + path), so Desktop sees every endpoint even
// before the app runs; and its `uses:` must declare every Desktop endpoint the code calls (a path in the source, backend
// or pages, under /gateways/dimos/ or /gateways/desktop/); an old Desktop path (/api/..., /dimos-gateway/...) fails. The backend prints them with
// `--openapi-json` (the same document it serves as openapi.json).
// `deno task check-endpoints` (CI runs it after `nix build`); `--write` adds the missing operations to its provides:
// and drops the gone ones, keeping the rest as written.
import { parse, stringify } from "@std/yaml"

type OpenApi = {
    openapi: string
    info: Record<string, unknown>
    paths: Record<string, Record<string, Record<string, unknown>>>
}

async function openapiJson(): Promise<OpenApi> {
    const built = new URL("../result/bin/dimos-app-server", import.meta.url).pathname
    const hasBuilt = await Deno.stat(built).then(() => true, () => false)
    const [cmd, args] = hasBuilt && !Deno.args.includes("--cargo") ? [built, ["--openapi-json"]] : ["cargo", [
        "run",
        "--quiet",
        "--manifest-path",
        new URL("../backend/Cargo.toml", import.meta.url).pathname,
        "--",
        "--openapi-json",
    ]]
    const out = await new Deno.Command(cmd, { args, stderr: "inherit" }).output()
    if (!out.success) {
        throw new Error(`${cmd} --openapi-json failed`)
    }
    return JSON.parse(new TextDecoder().decode(out.stdout))
}

const file = new URL("../dimos.yaml", import.meta.url)
const text = await Deno.readTextFile(file)
const yaml = parse(text) as Record<string, unknown>
const provides = (yaml.provides ?? {}) as Partial<OpenApi> & Record<string, unknown>
const served = await openapiJson()

// keys in a readable order: the ones listed first, the rest after as they come
const ordered = <T extends Record<string, unknown>>(value: T, first: string[]) =>
    Object.fromEntries([
        ...first.filter((k) => k in value).map((k) => [k, value[k]]),
        ...Object.entries(value).filter(([k]) => !first.includes(k)),
    ]) as T
if (Deno.args.includes("--write")) {
    // the served operations, each one dimos.yaml already has kept as written (its x-when and fuller text); x-private stays
    const paths: OpenApi["paths"] = {}
    for (const [path, item] of Object.entries(ordered(served.paths, Object.keys(provides.paths ?? {})))) {
        paths[path] = Object.fromEntries(
            Object.entries(item).map(([method, operation]) => {
                const op = ordered(operation, [
                    "summary",
                    "description",
                    "x-when",
                    "x-dimos-role",
                    "parameters",
                    "requestBody",
                ])
                if (Array.isArray(op.parameters)) {
                    op.parameters = op.parameters.map((p) =>
                        ordered(p, ["name", "in", "required", "schema", "description"])
                    )
                }
                return [method, provides.paths?.[path]?.[method] ?? op]
            }),
        )
    }
    const want = {
        openapi: served.openapi,
        info: ordered({ ...served.info, ...provides.info }, ["title", "version", "description", "x-when"]),
        ...(provides["x-private"] ? { "x-private": provides["x-private"] } : {}),
        paths,
    }
    // replace only the `provides:` block, so the rest of the file (and its comments) stays as written
    const lines = text.split("\n")
    const block = stringify({ provides: want }, { lineWidth: 120, indent: 4 }).trimEnd().split("\n")
    const start = lines.findIndex((line) => /^provides:/.test(line))
    if (start === -1) {
        lines.splice(lines.length - (lines.at(-1) === "" ? 1 : 0), 0, ...block)
    } else {
        let end = start + 1
        while (end < lines.length && (lines[end] === "" || /^[ #]/.test(lines[end]))) {
            end++
        }
        // trailing blank and comment lines belong to what follows
        while (end - 1 > start && (lines[end - 1].trim() === "" || lines[end - 1].startsWith("#"))) {
            end--
        }
        lines.splice(start, end - start, ...block)
    }
    await Deno.writeTextFile(file, lines.join("\n"))
    console.log(`wrote ${Object.values(paths).flatMap(Object.keys).length} operations into dimos.yaml`)
    Deno.exit(0)
}
const operations = (paths: OpenApi["paths"] = {}) =>
    new Set(Object.entries(paths).flatMap(([path, item]) => Object.keys(item).map((m) => `${m.toUpperCase()} ${path}`)))
const have = operations(provides.paths)
const need = operations(served.paths)
const missing = [...need].filter((k) => !have.has(k))
const extra = [...have].filter((k) => !need.has(k))
if (missing.length || extra.length) {
    console.error(
        `dimos.yaml's provides paths differ from backend/src/routes.rs:\n  missing: ${
            missing.join(", ") || "-"
        }\n  extra: ${extra.join(", ") || "-"}\nrun: deno task check-endpoints --write`,
    )
    Deno.exit(1)
}
console.log(`dimos.yaml lists all ${need.size} endpoints`)

// ── uses: every Desktop endpoint in the source is declared ──
/** `{id}`, `${encodeURIComponent(id)}` → `{}`; no query */
const shape = (path: string) => path.split("?")[0].replace(/\$\{[^}]*\}|\{[^}]*\}/g, "{}").replace(/\/+$/, "")
const uses = yaml.uses as Record<string, string[] | string>
const declared = (gateway: string) =>
    new Set((Array.isArray(uses[gateway]) ? uses[gateway] as string[] : []).map((route) => shape(route.split(" ")[1])))
const gateways: Record<string, Set<string>> = {
    "@dimos-gateway": declared("@dimos-gateway"),
    "@desktop-gateway": declared("@desktop-gateway"),
}
const found: [string, string, string][] = [] // gateway, path, where
const oldPaths: string[] = [] // Desktop paths from before /gateways/
async function scan(dir: URL, extensions: RegExp) {
    for await (const entry of Deno.readDir(dir)) {
        const url = new URL(entry.name + (entry.isDirectory ? "/" : ""), dir)
        if (entry.isDirectory) {
            await scan(url, extensions)
            continue
        }
        if (!extensions.test(entry.name)) {
            continue
        }
        const lines = (await Deno.readTextFile(url)).split("\n")
        lines.forEach((line, n) => {
            if (/^\s*(\/\/|\*|\/\*)/.test(line)) {
                return // a comment
            }
            for (
                const [, quoted] of line.matchAll(
                    /["'`]((?:\.\.\/)*\/?(?:gateways\/(?:dimos|desktop)|dimos-gateway|api)\/[^"'`]*)["'`]/g,
                )
            ) {
                const path = quoted.replace(/^(\.\.\/)+/, "/").replace(/^(?!\/)/, "/")
                if (/^\/gateways\/(dimos|desktop)\//.test(path) === false) {
                    // the app's own api/ (relative, no leading slash or ../; and the backend's own routes)
                    const ownApi = url.pathname.endsWith(".rs") || !/^(\.\.\/|\/)/.test(quoted)
                    if (!quoted.includes("dimos-gateway/") && ownApi) {
                        continue
                    }
                    oldPaths.push(`${quoted}  (${url.pathname.split("/frontend/")[1] ?? url.pathname})`)
                    continue
                }
                const gateway = path.startsWith("/gateways/dimos/") ? "@dimos-gateway" : "@desktop-gateway"
                const route = shape(path.replace(/^\/gateways\/(dimos|desktop)\//, "/"))
                if (route === "" || route === "/{}") {
                    continue // a prefix (the gateway fallback), not an endpoint
                }
                found.push([gateway, route, `${url.pathname.split("/dim-go2-dash/")[1]}:${n + 1}`])
            }
        })
    }
}
await scan(new URL("../backend/src/", import.meta.url), /\.rs$/)
await scan(new URL("../frontend/src/", import.meta.url), /\.(ts|tsx|js)$/)
if (oldPaths.length) {
    console.error(`old Desktop paths (use /gateways/desktop/... or /gateways/dimos/...):\n  ${oldPaths.join("\n  ")}`)
    Deno.exit(1)
}
const undeclared = found.filter(([gateway, route]) => !gateways[gateway].has(route))
if (undeclared.length) {
    console.error(
        `dimos.yaml's uses: doesn't declare Desktop endpoints the code calls:\n${
            undeclared.map(([gateway, route, where]) => `  ${gateway} ${route}  (${where})`).join("\n")
        }`,
    )
    Deno.exit(1)
}
const used = new Set(found.map(([gateway, route]) => `${gateway} ${route}`))
for (const [gateway, routes] of Object.entries(gateways)) {
    for (const route of routes) {
        if (!used.has(`${gateway} ${route}`)) {
            console.warn(
                `note: dimos.yaml uses ${gateway} ${route}, which no source path names (built at run time, or unused?)`,
            )
        }
    }
}
console.log(`dimos.yaml declares all ${new Set(found.map(([g, r]) => g + r)).size} Desktop endpoints the code calls`)

// backend/Cargo.toml's version is the app's own version: `CARGO_PKG_VERSION` is what the server logs at startup and
// what `app_version` in every recording's metadata is. The repo bumps dimos.yaml on release, so the crate has to move
// with it — it sat at 0.5.37 from v0.5.37 through v0.6.2, stamping a version that hadn't shipped in months onto every
// recording. Checked here rather than in the build, because the nix build copies only backend/ and so cannot read
// dimos.yaml. `dimos-desktop app check .` reports the same drift from the outside (Desktop 0.2.227).
const cargoToml = await Deno.readTextFile(new URL("../backend/Cargo.toml", import.meta.url))
const crateVersion = cargoToml.match(/^\[package\][^[]*?^version\s*=\s*"([^"]+)"/ms)?.[1]
if (crateVersion !== yaml.version) {
    console.error(
        `backend/Cargo.toml says version ${crateVersion}, dimos.yaml says ${yaml.version}: that version is what the ` +
            `server reports of itself and writes into a recording's app_version, so a release bumps both`,
    )
    Deno.exit(1)
}
console.log(`backend/Cargo.toml and dimos.yaml agree on ${crateVersion}`)

// dimos.yaml's `provides:` must list exactly the backend's routes (method + path), so Desktop sees every endpoint even
// before the app runs; and its `uses:` must declare every Desktop endpoint the code calls (a path in the source, backend
// or pages, under /dimos-gateway/ (or the older /dimos/) or Desktop's /api/). The backend prints them with `--agent-json` (the same table it serves as agent.json).
// `deno task check-endpoints` (CI runs it after `nix build`); `--write` rewrites its description and endpoints.
import { parse, stringify } from "@std/yaml"

type Endpoint = { method: string; path: string }

async function agentJson(): Promise<{ description: string; endpoints: Endpoint[] }> {
    const built = new URL("../result/bin/dimos-app-server", import.meta.url).pathname
    const hasBuilt = await Deno.stat(built).then(() => true, () => false)
    const [cmd, args] = hasBuilt && !Deno.args.includes("--cargo") ? [built, ["--agent-json"]] : ["cargo", [
        "run",
        "--quiet",
        "--manifest-path",
        new URL("../backend/Cargo.toml", import.meta.url).pathname,
        "--",
        "--agent-json",
    ]]
    const out = await new Deno.Command(cmd, { args, stderr: "inherit" }).output()
    if (!out.success) {
        throw new Error(`${cmd} --agent-json failed`)
    }
    return JSON.parse(new TextDecoder().decode(out.stdout))
}

const file = new URL("../dimos.yaml", import.meta.url)
const yaml = parse(await Deno.readTextFile(file)) as Record<string, unknown>
const want = await agentJson()
if (Deno.args.includes("--write")) {
    yaml.provides = { ...(yaml.provides as Record<string, unknown>), ...want }
    await Deno.writeTextFile(file, stringify(yaml, { lineWidth: 120 }))
    console.log(`wrote ${want.endpoints.length} endpoints into dimos.yaml`)
    Deno.exit(0)
}
const key = (e: Endpoint) => `${e.method} ${e.path}`
const have = new Set(((yaml.provides as { endpoints?: Endpoint[] })?.endpoints ?? []).map(key))
const need = new Set(want.endpoints.map(key))
const missing = [...need].filter((k) => !have.has(k))
const extra = [...have].filter((k) => !need.has(k))
if (missing.length || extra.length) {
    console.error(
        `dimos.yaml's provides endpoints differ from backend/src/routes.rs:\n  missing: ${
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
            for (const [, quoted] of line.matchAll(/["'`]((?:\.\.\/)*\/?(?:dimos-gateway|dimos|api)\/[^"'`]*)["'`]/g)) {
                const path = quoted.replace(/^(\.\.\/)+/, "/").replace(/^(?!\/)/, "/")
                const gateway = /^\/(dimos-gateway|dimos)\//.test(path) ? "@dimos-gateway" : "@desktop-gateway"
                // the app's own api/ (relative, no leading slash or ../; and the backend's own routes), except Desktop's api/desktop/
                const own = url.pathname.endsWith(".rs") ||
                    (!/^(\.\.\/|\/)/.test(quoted) && !quoted.startsWith("api/desktop/"))
                if (gateway === "@desktop-gateway" && own) {
                    continue
                }
                const route = shape(path.replace(/^\/(dimos-gateway|dimos)\//, "/"))
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

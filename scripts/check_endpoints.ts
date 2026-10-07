// dimos.yaml's `provides:` must list exactly the backend's routes (method + path), so Desktop sees every endpoint even
// before the app runs. The backend prints them with `--agent-json` (the same table it serves as agent.json).
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

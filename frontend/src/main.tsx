import { createRoot } from "react-dom/client"
import { App } from "./App.tsx"
import "./theme.css"
import "./app.css"

// Follow dimOS Desktop's light/dark choice: the shell writes localStorage "dimos.themeChoice" ("light" | "dark",
// missing = dark); apps are same-origin iframes, so read it now and on every `storage` event.
const THEME_KEY = "dimos.themeChoice"
function applyTheme() {
    let light = false
    try {
        light = localStorage.getItem(THEME_KEY) === "light"
    } catch {
        // storage blocked
    }
    document.documentElement.style.colorScheme = light ? "light" : "dark"
    document.body.classList.add("science")
    document.body.classList.toggle("dark", !light)
}
applyTheme()
globalThis.addEventListener("storage", (event) => {
    if (event.key === THEME_KEY || event.key === null) {
        applyTheme()
    }
})

createRoot(document.getElementById("root")!).render(<App />)

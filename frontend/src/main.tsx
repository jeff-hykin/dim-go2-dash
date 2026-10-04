import { createRoot } from "react-dom/client"
import { App } from "./App.tsx"
import { initTheme } from "./dim-app/theme.js"
import "./dim-app/theme.css"
import "./app.css"

// Portal (dark) / Research (light): follows prefers-color-scheme, or the choice saved by the panel's toggle
initTheme()

createRoot(document.getElementById("root")!).render(<App />)

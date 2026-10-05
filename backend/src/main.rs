// dimos-app-server: this app's API and its built frontend on the unix socket Desktop gives (DIMOS_APP's `socket`; older
// Desktops: --socket), else a port. What Desktop passes: the DIMOS_APP env var, one JSON object (docs/apps.md).
// `--agent-json` prints the endpoints (what dimos.yaml's `agent:` must list) and exits. GO2_DASH_MOCK=1 simulates
// every robot, Bluetooth and cloud interaction (for trying the UI without hardware).

use std::path::PathBuf;

use go2_dash::app::App;

mod dimos_app;

fn flag(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|arg| arg == &format!("--{name}")).and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main]
async fn main() {
    let mock = std::env::var("GO2_DASH_MOCK").is_ok_and(|v| v == "1" || v == "true");
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    let desktop_dir = dimos_app::get().and_then(|given| given.data_dir.clone()).map(PathBuf::from);
    let data_dir = go2_dash::app::data_dir(std::env::var("GO2_DASH_DATA_DIR").ok().map(PathBuf::from), desktop_dir.clone(), home.clone(), mock);
    if desktop_dir.is_some() && !mock && std::env::var_os("GO2_DASH_DATA_DIR").is_none() {
        let copied = go2_dash::app::migrate_legacy(&home.join(".local/share/dim"), &data_dir);
        if copied > 0 {
            eprintln!("copied {copied} saved files from ~/.local/share/dim into {}", data_dir.display());
        }
    }
    let app = App::new(data_dir, mock);
    if let Some(url) = dimos_app::get().and_then(|given| given.desktop_url.clone()) {
        let _ = app.desktop_url.set(url);
    }
    if std::env::args().any(|arg| arg == "--agent-json") {
        println!("{}", serde_json::to_string_pretty(&go2_dash::api::describe(go2_dash::routes::DESCRIPTION, &app.routes)).unwrap());
        return;
    }
    tokio::spawn({
        let app = app.clone();
        async move { app.refresh_ssid().await }
    });
    // backend → page: every event through Desktop's relay onto the page's zenoh-web connection
    match dimos_app::get().and_then(|given| Some((given.desktop_url.clone()?, given.name.clone()?))) {
        Some((desktop_url, name)) => go2_dash::relay::spawn(app.clone(), desktop_url, name),
        None => eprintln!("no Desktop URL or app name in DIMOS_APP: events reach no page"),
    }
    let frontend = flag("frontend").map(PathBuf::from);
    let router = go2_dash::server::router(app, frontend);
    if let Some(socket) = dimos_app::get().and_then(|app| app.socket.clone()).or_else(|| flag("socket")) {
        let _ = std::fs::remove_file(&socket);
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the socket");
        eprintln!("listening on {socket}");
        axum::serve(listener, router).await.unwrap();
    } else {
        let port: u16 = flag("port").and_then(|p| p.parse().ok()).unwrap_or(8787);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind --port");
        eprintln!("listening on http://127.0.0.1:{port}");
        axum::serve(listener, router).await.unwrap();
    }
}

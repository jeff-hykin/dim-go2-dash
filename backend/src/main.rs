// dimos-app-server: this app's API and its built frontend on the unix socket Desktop gives (--socket), else a port.
// Desktop's flags: --socket --desktop-url --zenoh-web-url --zenoh-connect --dimos-dir --dimos-python (docs/apps.md).
// `--agent-json` prints the endpoints (what dimos.yaml's `agent:` must list) and exits. GO2_DASH_MOCK=1 simulates
// every robot, Bluetooth and cloud interaction (for trying the UI without hardware).

use std::path::PathBuf;

use go2_dash::app::App;

fn flag(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|arg| arg == &format!("--{name}")).and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main]
async fn main() {
    let data_dir = std::env::var("GO2_DASH_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/share/dim"));
    let mock = std::env::var("GO2_DASH_MOCK").is_ok_and(|v| v == "1" || v == "true");
    let app = App::new(data_dir, mock);
    if std::env::args().any(|arg| arg == "--agent-json") {
        println!("{}", serde_json::to_string_pretty(&go2_dash::api::describe(go2_dash::routes::DESCRIPTION, &app.routes)).unwrap());
        return;
    }
    tokio::spawn({
        let app = app.clone();
        async move { app.refresh_ssid().await }
    });
    let frontend = flag("frontend").map(PathBuf::from);
    let router = go2_dash::server::router(app, frontend);
    if let Some(socket) = flag("socket") {
        let _ = std::fs::remove_file(&socket);
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind --socket");
        eprintln!("listening on {socket}");
        axum::serve(listener, router).await.unwrap();
    } else {
        let port: u16 = flag("port").and_then(|p| p.parse().ok()).unwrap_or(8787);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind --port");
        eprintln!("listening on http://127.0.0.1:{port}");
        axum::serve(listener, router).await.unwrap();
    }
}

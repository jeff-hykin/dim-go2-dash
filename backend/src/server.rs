// The HTTP server: the routes (api.rs) and the built frontend. Events go to pages over zenoh (relay.rs).

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;

use crate::app::App;

#[derive(Clone)]
struct Server {
    app: Arc<App>,
    frontend: Option<PathBuf>,
}

pub fn router(app: Arc<App>, frontend: Option<PathBuf>) -> Router {
    Router::new()
        .route("/api/drive/video.ws", axum::routing::get(video_ws))
        .fallback(serve)
        .with_state(Server { app, frontend })
}

/// The camera as H.264 over a WebSocket (stream.rs), for pages whose WebRTC video won't connect.
async fn video_ws(State(server): State<Server>, upgrade: axum::extract::ws::WebSocketUpgrade) -> Response {
    let Some(drive) = server.app.drive.lock().await.clone() else {
        return (StatusCode::CONFLICT, "no drive session").into_response();
    };
    upgrade.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        use tokio::sync::broadcast::error::RecvError;
        let mut frames = drive.stream.subscribe();
        crate::dlog!("video.ws: a page connected");
        for _ in 0..3 {
            drive.keyframe().await;
        }
        loop {
            match frames.recv().await {
                Ok(frame) => {
                    if socket.send(Message::Binary(frame.as_ref().clone().into())).await.is_err() {
                        break;
                    }
                }
                // behind: the page skips to the next keyframe; ask for one
                Err(RecvError::Lagged(_)) => drive.keyframe().await,
                Err(RecvError::Closed) => break,
            }
        }
        crate::dlog!("video.ws: the page disconnected");
    })
}

async fn serve(State(server): State<Server>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body: Bytes = match axum::body::to_bytes(body, 4 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
    };
    if let Some(response) = crate::api::handle(server.app.clone(), &parts.method, &parts.uri, body).await {
        return response;
    }
    file(server.frontend.as_deref(), parts.uri.path()).await
}

async fn file(frontend: Option<&std::path::Path>, path: &str) -> Response {
    let Some(root) = frontend else {
        return (StatusCode::NOT_FOUND, "no frontend (pass --frontend)").into_response();
    };
    let clean: Vec<&str> = path.split('/').filter(|part| !part.is_empty() && *part != "..").collect();
    let clean = if clean.is_empty() { "index.html".to_string() } else { clean.join("/") };
    // unknown paths get the app (hash routing)
    for candidate in [clean.as_str(), "index.html"] {
        if let Ok(bytes) = tokio::fs::read(root.join(candidate)).await {
            let kind = match candidate.rsplit('.').next().unwrap_or("") {
                "html" => "text/html; charset=utf-8",
                "js" => "text/javascript",
                "css" => "text/css",
                "svg" => "image/svg+xml",
                "png" => "image/png",
                "json" => "application/json",
                "wasm" => "application/wasm",
                _ => "application/octet-stream",
            };
            return ([(header::CONTENT_TYPE, kind)], bytes).into_response();
        }
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

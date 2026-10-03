// The HTTP layer: a table of routes (method, relative path, description, params, role) that is both what the server
// answers and what agent.json / dimos.yaml say about it, so the UI, Desktop's agent and the docs can't drift apart.
// Same shape as the Deno template's backend/http.ts (dimos-desktop docs/apps.md, docs/agent.md).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::app::App;

pub type Args = Map<String, Value>;
pub type Reply = Pin<Box<dyn Future<Output = Result<Value, HttpError>> + Send>>;
pub type Handler = Arc<dyn Fn(Arc<App>, Args) -> Reply + Send + Sync>;

pub struct Route {
    pub method: &'static str,
    /// relative to the app, e.g. `api/scan`; `{name}` segments become params
    pub path: String,
    pub description: String,
    /// `{ name: { type, description, required } }`, the agent.json shorthand
    pub params: Option<Value>,
    /// "view": what Desktop's screenshot() calls; "context": what desktop_context adds while the app is focused
    pub role: Option<&'static str>,
    pub handler: Handler,
}

pub fn handler<F, Fut>(f: F) -> Handler
where
    F: Fn(Arc<App>, Args) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, HttpError>> + Send + 'static,
{
    Arc::new(move |app, args| Box::pin(f(app, args)))
}

/// A readable error with an HTTP status: the UI and the agent both show `message`.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub message: String,
}

impl HttpError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        HttpError { status, message: message.into() }
    }
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, message)
    }
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(502, message)
    }
}

pub fn describe(description: &str, routes: &[Route]) -> Value {
    let endpoints: Vec<Value> = routes
        .iter()
        .map(|route| {
            let mut endpoint = json!({ "method": route.method, "path": route.path, "description": route.description });
            if let Some(params) = &route.params {
                endpoint["params"] = params.clone();
            }
            if let Some(role) = route.role {
                endpoint["role"] = json!(role);
            }
            endpoint
        })
        .collect();
    json!({ "description": description, "endpoints": endpoints })
}

fn matches(route: &Route, method: &str, path: &str) -> Option<Args> {
    if route.method != method {
        return None;
    }
    let want: Vec<&str> = route.path.split('/').collect();
    let got: Vec<&str> = path.split('/').collect();
    if want.len() != got.len() {
        return None;
    }
    let mut params = Args::new();
    for (w, g) in want.iter().zip(got.iter()) {
        if let Some(name) = w.strip_prefix('{').and_then(|rest| rest.strip_suffix('}')) {
            params.insert(name.to_string(), json!(percent_decode(g, false)));
        } else if w != g {
            return None;
        }
    }
    Some(params)
}

fn percent_decode(text: &str, query: bool) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(if query && bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn error(status: u16, message: impl Into<String>) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// Answers `/api/...` and `/agent.json`; None for anything else (the static frontend).
pub async fn handle(app: Arc<App>, method: &Method, uri: &Uri, body: Bytes) -> Option<Response> {
    let path = uri.path().trim_start_matches('/');
    if path == "agent.json" {
        return Some(Json(describe(crate::routes::DESCRIPTION, &app.routes)).into_response());
    }
    for route in app.routes.iter() {
        let Some(path_params) = matches(route, method.as_str(), path) else {
            continue;
        };
        let mut args = Args::new();
        for pair in uri.query().unwrap_or("").split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            args.insert(percent_decode(key, true), json!(percent_decode(value, true)));
        }
        args.extend(path_params);
        if method != Method::GET && method != Method::DELETE && !body.is_empty() {
            match serde_json::from_slice::<Value>(&body) {
                Ok(Value::Object(fields)) => args.extend(fields),
                Ok(_) => return Some(error(400, "the body must be a JSON object")),
                Err(_) => return Some(error(400, "the body isn't JSON")),
            }
        }
        if let Some(Value::Object(spec)) = &route.params {
            for (name, schema) in spec {
                let required = schema.get("required").and_then(Value::as_bool).unwrap_or(false);
                if required && args.get(name).is_none_or(Value::is_null) {
                    return Some(error(400, format!("{name} is required")));
                }
            }
        }
        return Some(match (route.handler)(app.clone(), args).await {
            Ok(value) => Json(if value.is_null() { json!({ "ok": true }) } else { value }).into_response(),
            Err(err) => error(err.status, err.message),
        });
    }
    if path.starts_with("api/") {
        return Some(error(404, format!("no such endpoint: {method} /{path}")));
    }
    None
}

// ── argument helpers: query strings arrive as text, JSON bodies typed ──

pub fn text(args: &Args, name: &str) -> Option<String> {
    match args.get(name) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

pub fn flag(args: &Args, name: &str) -> Result<bool, HttpError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::String(text)) if text == "true" || text == "1" || text.is_empty() => Ok(true),
        Some(Value::String(text)) if text == "false" || text == "0" => Ok(false),
        _ => Err(HttpError::bad(format!("{name} must be true or false"))),
    }
}

pub fn number(args: &Args, name: &str) -> Result<Option<f64>, HttpError> {
    let value = match args.get(name) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    match value {
        Some(value) if value.is_finite() => Ok(Some(value)),
        _ => Err(HttpError::bad(format!("{name} must be a number"))),
    }
}

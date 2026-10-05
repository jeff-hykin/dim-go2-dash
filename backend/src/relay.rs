// Backend → page (Desktop's docs/events.md): every event the app publishes goes to Desktop's relay,
// `POST <desktopUrl>/desktop/frontend/<name>/events`, which puts it on `<ns>/apps/<name>/frontend/events` for the page's
// zenoh-web connection (dim-app's appEvents). One task sends them one at a time, so they arrive in order.

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;

use crate::app::App;

/// `<desktopUrl>/desktop/frontend/<name>/<topic>`
pub fn relay_url(desktop_url: &str, name: &str, topic: &str) -> String {
    format!("{}/desktop/frontend/{}/{topic}", desktop_url.trim_end_matches('/'), encode(name))
}

fn encode(chunk: &str) -> String {
    chunk
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Forwards the app's events to the relay until the app goes away. Failures are logged once per kind, never fatal.
pub fn spawn(app: Arc<App>, desktop_url: String, name: String) {
    let url = relay_url(&desktop_url, &name, "events");
    let mut rx = app.subscribe();
    drop(app);
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut warned = std::collections::HashSet::new();
        loop {
            let event = match rx.recv().await {
                Ok(event) => event,
                Err(RecvError::Lagged(n)) => {
                    eprintln!("relay: {n} events dropped (Desktop slow)");
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            let sent = client.post(&url).header("content-type", "application/json").body(event).send().await;
            let problem = match sent {
                Ok(response) if response.status().is_success() => None,
                Ok(response) => Some(format!("HTTP {}", response.status())),
                Err(error) => Some(if error.is_connect() { "Desktop unreachable".to_string() } else { error.to_string() }),
            };
            if let Some(problem) = problem {
                if warned.insert(problem.clone()) {
                    eprintln!("relay: POST {url}: {problem}");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url() {
        assert_eq!(relay_url("http://127.0.0.1:7341/", "dim go2", "events"), "http://127.0.0.1:7341/desktop/frontend/dim%20go2/events");
    }
}

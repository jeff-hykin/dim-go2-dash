//! Connects to a Go2 through dimos' Go2 WebRTC crate and prints what arrives: `cargo run --example dimos_go2_webrtc -- <ip> [aes_128_key]`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dimos_go2_webrtc::conn::{Conn, Event};
use dimos_go2_webrtc::timing::Timing;

#[tokio::main]
async fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let ip = args.next().ok_or("usage: dimos_go2_webrtc <ip> [aes_128_key]")?;
    let aes_key = args.next().unwrap_or_default();
    let (messages, packets) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let on_event = {
        let (messages, packets) = (messages.clone(), packets.clone());
        Arc::new(move |event| match event {
            Event::Data { .. } => {
                messages.fetch_add(1, Ordering::Relaxed);
            }
            Event::Rtp { .. } => {
                packets.fetch_add(1, Ordering::Relaxed);
            }
            Event::Lost => eprintln!("link lost"),
        })
    };
    let conn = Conn::connect(&ip, &aes_key, &Timing::default(), on_event).await?;
    conn.subscribe("rt/lf/lowstate").await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    println!(
        "{} data messages, {} camera packets in 5 s",
        messages.load(Ordering::Relaxed),
        packets.load(Ordering::Relaxed)
    );
    conn.close().await;
    Ok(())
}

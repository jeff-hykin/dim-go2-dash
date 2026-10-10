// dim-go2-dash backend: Unitree Go2 discovery (BLE + LAN + ARP), Wi-Fi provisioning over Bluetooth, and live control
// over WebRTC, every action an HTTP endpoint (routes.rs) the UI and Desktop's agent share.

pub mod api;
pub mod app;
pub mod arp;
pub mod ble;
pub mod camera;
pub mod cdr;
pub mod db;
mod cloud;
pub mod discovery;
pub mod log;
pub mod msg;
#[cfg(target_os = "macos")]
pub mod macos_wifi;
pub mod drive;
pub mod health;
pub mod hotspot;
mod lan;
mod protocol;
pub mod record;
pub mod recording;
pub mod stream;
pub mod relay;
pub mod robot_rtc;
pub mod routes;
pub mod sensors;
pub mod server;
pub mod setup;
pub mod sweep;
pub mod uploads;
mod video;

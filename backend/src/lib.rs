// dim-go2-dash backend: Unitree Go2 discovery (BLE + LAN + ARP), Wi-Fi provisioning over Bluetooth, and live control
// over WebRTC, every action an HTTP endpoint (routes.rs) the UI and Desktop's agent share.

pub mod api;
pub mod app;
pub mod arp;
pub mod ble;
pub mod camera;
pub mod cdr;
mod cloud;
pub mod discovery;
pub mod log;
#[cfg(target_os = "macos")]
pub mod macos_wifi;
pub mod drive;
pub mod hotspot;
mod lan;
mod protocol;
pub mod record;
pub mod preview;
pub mod recording;
pub mod relay;
pub mod robot_rtc;
pub mod routes;
pub mod sensors;
pub mod server;
pub mod setup;
pub mod sweep;
pub mod uploads;
mod video;

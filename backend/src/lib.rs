// dim-go2-dash backend: Unitree Go2 discovery (BLE + LAN + ARP), Wi-Fi provisioning over Bluetooth, and live control
// over WebRTC, every action an HTTP endpoint (routes.rs) the UI and Desktop's agent share.

pub mod api;
pub mod app;
mod arp;
mod ble;
mod cloud;
mod discovery;
pub mod drive;
mod lan;
mod protocol;
pub mod relay;
pub mod robot_rtc;
pub mod routes;
pub mod server;
pub mod setup;
mod video;

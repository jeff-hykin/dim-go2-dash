// BLE discovery + wifi provisioning via btleplug (CoreBluetooth on macOS, BlueZ
// on Linux). Faithful port of dimos' discover_ble / provision_wifi.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use btleplug::api::{
    Central, CentralEvent, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;

use crate::protocol::{
    build_packet, decrypt, serial_from_manufacturer, validate_response, CHUNK_SIZE,
    HANDSHAKE_CONTENT, INST_COUNTRY, INST_HANDSHAKE, INST_INIT_STA, INST_PASSWORD, INST_SERIAL,
    INST_SSID, NOTIFY_CHAR_UUID, UNITREE_NAME_PREFIXES, WRITE_CHAR_UUID,
};

/// A robot seen over BLE. `address` is the btleplug peripheral id string — the
/// same value the frontend later hands back in a `connect` command.
#[derive(Clone)]
pub struct BleDevice {
    pub serial: Option<String>,
    pub name: String,
    pub address: String,
}

/// Shared registry of discovered peripherals so a later `connect` can reconnect
/// by the id string we advertised.
pub type Registry = Arc<Mutex<HashMap<String, Peripheral>>>;

pub async fn first_adapter() -> Result<Adapter, String> {
    let manager = Manager::new().await.map_err(|e| e.to_string())?;
    let adapters = manager.adapters().await.map_err(|e| e.to_string())?;
    adapters
        .into_iter()
        .next()
        .ok_or_else(|| "no bluetooth adapter found".to_string())
}

fn name_matches(name: &str) -> bool {
    UNITREE_NAME_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Stream Unitree robots seen over BLE to `out`, mirroring dimos' discover_ble:
/// emit on first sighting and again when a later advertisement upgrades a
/// previously-unknown serial. Runs until the caller drops the task.
pub async fn scan_ble(
    adapter: Adapter,
    registry: Registry,
    out: mpsc::Sender<BleDevice>,
) -> Result<(), String> {
    let mut events = adapter.events().await.map_err(|e| e.to_string())?;
    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|e| e.to_string())?;

    // address -> last serial we reported (None until recovered)
    let mut seen: HashMap<String, Option<String>> = HashMap::new();

    while let Some(event) = events.next().await {
        let id = match event {
            CentralEvent::DeviceDiscovered(id)
            | CentralEvent::DeviceUpdated(id)
            | CentralEvent::ManufacturerDataAdvertisement { id, .. } => id,
            _ => continue,
        };
        let peripheral = match adapter.peripheral(&id).await {
            Ok(p) => p,
            Err(_) => continue,
        };
        let props = match peripheral.properties().await {
            Ok(Some(props)) => props,
            _ => continue,
        };
        let name = match props.local_name {
            Some(name) if name_matches(&name) => name,
            _ => continue,
        };
        let serial = props
            .manufacturer_data
            .iter()
            .next()
            .and_then(|(&cid, payload)| serial_from_manufacturer(cid, payload));

        let address = peripheral.id().to_string();
        registry.lock().await.insert(address.clone(), peripheral);

        let emit = match seen.get(&address) {
            None => true,
            Some(prev) => prev.is_none() && serial.is_some(),
        };
        if emit {
            seen.insert(address.clone(), serial.clone());
            if out
                .send(BleDevice {
                    serial,
                    name,
                    address,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    }
    Ok(())
}

// ── provisioning ─────────────────────────────────────────────────────────────

enum Signal {
    Response(Vec<u8>),
    SerialReady,
}

fn find_char(peripheral: &Peripheral, uuid: &str) -> Option<Characteristic> {
    peripheral
        .characteristics()
        .into_iter()
        .find(|c| c.uuid.to_string() == uuid)
}

async fn connect_with_retry(
    peripheral: &Peripheral,
    attempts: u32,
    mut on_progress: impl FnMut(String),
) -> Result<(), String> {
    let mut last = String::from("no attempts");
    for i in 0..attempts {
        match peripheral.connect().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = e.to_string();
                on_progress(format!("connect attempt {}/{} failed: {}", i + 1, attempts, last));
                if i + 1 < attempts {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
    Err(last)
}

/// Provision a Unitree robot's wifi over BLE. Returns the serial on success.
///
/// Retries only the connection step; once connected, protocol failures are not
/// retried (blind retry against partial on-robot state is counterproductive).
pub async fn provision_wifi(
    peripheral: Peripheral,
    ssid: &str,
    password: &str,
    country_code: &str,
    connect_retries: u32,
    mut progress: impl FnMut(String),
) -> Result<Option<String>, String> {
    connect_with_retry(&peripheral, connect_retries, &mut progress).await?;

    let result = provision_inner(&peripheral, ssid, password, country_code, &mut progress).await;

    let _ = peripheral.disconnect().await;
    result
}

async fn provision_inner(
    peripheral: &Peripheral,
    ssid: &str,
    password: &str,
    country_code: &str,
    progress: &mut impl FnMut(String),
) -> Result<Option<String>, String> {
    peripheral
        .discover_services()
        .await
        .map_err(|e| e.to_string())?;

    let write_char = find_char(peripheral, WRITE_CHAR_UUID)
        .ok_or_else(|| "write characteristic not found".to_string())?;
    let notify_char = find_char(peripheral, NOTIFY_CHAR_UUID)
        .ok_or_else(|| "notify characteristic not found".to_string())?;

    let mut notifications = peripheral.notifications().await.map_err(|e| e.to_string())?;
    peripheral
        .subscribe(&notify_char)
        .await
        .map_err(|e| e.to_string())?;

    let (sig_tx, mut sig_rx) = mpsc::unbounded_channel::<Signal>();
    let serial_slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let reader_serial = serial_slot.clone();

    // Background reader: decrypt notifications, accumulate the chunked serial,
    // route everything else as a validated-response signal. Mirrors _Session.on_notify.
    let reader = tokio::spawn(async move {
        let mut chunks: HashMap<u8, Vec<u8>> = HashMap::new();
        while let Some(notification) = notifications.next().await {
            let packet = decrypt(&notification.value);
            if packet.len() < 5 || packet[0] != 0x51 {
                continue;
            }
            if packet[2] == INST_SERIAL {
                let index = packet[3];
                let total = packet[4] as usize;
                chunks.insert(index, packet[5..packet.len() - 1].to_vec());
                if chunks.len() >= total {
                    let mut keys: Vec<u8> = chunks.keys().copied().collect();
                    keys.sort_unstable();
                    let mut joined = Vec::new();
                    for key in keys {
                        joined.extend_from_slice(&chunks[&key]);
                    }
                    let text = String::from_utf8_lossy(&joined)
                        .trim_end_matches('\0')
                        .to_string();
                    *reader_serial.lock().await = Some(text);
                    let _ = sig_tx.send(Signal::SerialReady);
                }
            } else {
                let _ = sig_tx.send(Signal::Response(packet));
            }
        }
    });

    let write = |packet: Vec<u8>| {
        let peripheral = peripheral.clone();
        let write_char = write_char.clone();
        async move {
            peripheral
                .write(&write_char, &packet, WriteType::WithResponse)
                .await
                .map_err(|e| e.to_string())
        }
    };

    // Wait for the next validated response and check it, mirroring write_validated.
    async fn expect_valid(
        sig_rx: &mut mpsc::UnboundedReceiver<Signal>,
        inst: u8,
        wait: Duration,
    ) -> Result<(), String> {
        match timeout(wait, sig_rx.recv()).await {
            Ok(Some(Signal::Response(packet))) if validate_response(&packet, inst) => Ok(()),
            Ok(Some(Signal::Response(_))) => {
                Err(format!("BLE response invalid for instruction {inst}"))
            }
            Ok(Some(Signal::SerialReady)) => {
                Err(format!("unexpected serial while awaiting instruction {inst}"))
            }
            Ok(None) => Err("BLE reader closed".to_string()),
            Err(_) => Err(format!("timeout awaiting instruction {inst}")),
        }
    }

    let default_wait = Duration::from_secs(10);

    progress("handshake".to_string());
    let mut hs = vec![0u8, 0u8];
    hs.extend_from_slice(HANDSHAKE_CONTENT);
    write(build_packet(INST_HANDSHAKE, &hs)).await?;
    expect_valid(&mut sig_rx, INST_HANDSHAKE, default_wait).await?;

    progress("read serial".to_string());
    write(build_packet(INST_SERIAL, &[0])).await?;
    // Best-effort — some firmware answers, some doesn't. Ignore timeout.
    let _ = timeout(Duration::from_secs(2), sig_rx.recv()).await;

    progress("init STA mode".to_string());
    write(build_packet(INST_INIT_STA, &[2])).await?;
    expect_valid(&mut sig_rx, INST_INIT_STA, default_wait).await?;

    progress(format!("set SSID: {ssid}"));
    send_chunked(&write, &mut sig_rx, INST_SSID, ssid.as_bytes(), default_wait).await?;

    progress("set password".to_string());
    send_chunked(
        &write,
        &mut sig_rx,
        INST_PASSWORD,
        password.as_bytes(),
        Duration::from_secs(5),
    )
    .await?;

    progress(format!("set country: {country_code}"));
    let mut country_payload = vec![1u8];
    country_payload.extend_from_slice(country_code.as_bytes());
    country_payload.push(0);
    write(build_packet(INST_COUNTRY, &country_payload)).await?;
    expect_valid(&mut sig_rx, INST_COUNTRY, default_wait).await?;

    reader.abort();
    let serial = serial_slot.lock().await.clone();
    Ok(serial)
}

async fn send_chunked<F, Fut>(
    write: &F,
    sig_rx: &mut mpsc::UnboundedReceiver<Signal>,
    instruction: u8,
    data: &[u8],
    response_wait: Duration,
) -> Result<(), String>
where
    F: Fn(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let total = std::cmp::max(1, data.len().div_ceil(CHUNK_SIZE));
    for i in 0..total {
        let chunk = &data[i * CHUNK_SIZE..std::cmp::min((i + 1) * CHUNK_SIZE, data.len())];
        let mut payload = vec![(i + 1) as u8, total as u8];
        payload.extend_from_slice(chunk);
        write(build_packet(instruction, &payload)).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    match timeout(response_wait, sig_rx.recv()).await {
        Ok(Some(Signal::Response(packet))) if validate_response(&packet, instruction) => Ok(()),
        Ok(Some(_)) => Err(format!(
            "BLE response invalid for chunked instruction {instruction}"
        )),
        Ok(None) => Err("BLE reader closed".to_string()),
        Err(_) => Err(format!("timeout awaiting chunked instruction {instruction}")),
    }
}

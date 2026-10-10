// macOS shows Wi-Fi names only to an app the user allowed Location. This server is that app: nix installs it inside
// "Go2 Ctrl.app" (macos/Info.plist), and hotspot.rs runs it through `open` (so the prompt names Go2 Ctrl, not Desktop
// or a terminal) as `--wifi-scan <out.json>`.
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread};
use objc2_core_location::{CLAuthorizationStatus, CLLocationManager, CLLocationManagerDelegate};
use objc2_core_wlan::{CWSecurity, CWWiFiClient};
use objc2_foundation::{NSObject, NSObjectProtocol, NSRunLoop};
use serde_json::{json, Value};

/// the file after `--wifi-scan`
fn output() -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == "--wifi-scan").and_then(|i| args.get(i + 1).cloned()).unwrap_or_else(|| "/tmp/go2-ctrl-wifi.json".into())
}

fn finish(value: Value) -> ! {
    let _ = std::fs::write(output(), value.to_string());
    std::process::exit(0);
}

fn scan() -> ! {
    let client = unsafe { CWWiFiClient::sharedWiFiClient() };
    let Some(interface) = (unsafe { client.interface() }) else { finish(json!({ "error": "no Wi-Fi interface" })) };
    // right after Location is allowed (and without it) CoreWLAN hides every name: retry, then say so, not "none nearby"
    for attempt in 0..5 {
        let networks = match unsafe { interface.scanForNetworksWithName_error(None) } {
            Ok(networks) => networks,
            Err(error) => finish(json!({ "error": format!("scan failed: {}", error.localizedDescription()) })),
        };
        let mut found = Vec::new();
        for network in networks.iter() {
            let Some(ssid) = (unsafe { network.ssid() }) else { continue };
            let secured = !unsafe { network.supportsSecurity(CWSecurity::None) };
            found.push(json!({ "ssid": ssid.to_string(), "rssi": unsafe { network.rssiValue() }, "secured": secured }));
        }
        if !found.is_empty() || networks.count() == 0 {
            finish(Value::Array(found))
        }
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    finish(json!({ "error": "macOS hid every Wi-Fi name: allow Location for Go2 Ctrl in System Settings → Privacy & Security → Location Services" }))
}

fn on_status(manager: &CLLocationManager) {
    let status = unsafe { manager.authorizationStatus() };
    if status == CLAuthorizationStatus::AuthorizedAlways || status == CLAuthorizationStatus::AuthorizedWhenInUse {
        scan();
    } else if status == CLAuthorizationStatus::Denied || status == CLAuthorizationStatus::Restricted {
        finish(json!({ "error": "Location is off for Go2 Ctrl: allow it in System Settings → Privacy & Security → Location Services", "denied": true }));
    } else {
        unsafe { manager.requestWhenInUseAuthorization() };
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "Go2CtrlLocationDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl CLLocationManagerDelegate for Delegate {
        #[unsafe(method(locationManagerDidChangeAuthorization:))]
        fn did_change(&self, manager: &CLLocationManager) {
            on_status(manager);
        }
    }
);

impl Delegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// `dimos-app-server --wifi-scan <out.json>`, run as Go2 Ctrl.app through `open`: asks for Location the first time,
/// then writes the nearby networks [{ssid, rssi, secured}] (or {error}) and exits.
pub fn run() -> ! {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(120));
        finish(json!({ "error": "timed out waiting for the Location prompt" }));
    });
    let manager = unsafe { CLLocationManager::new() };
    let delegate = Delegate::new();
    unsafe { manager.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
    let _ = &delegate;
    NSRunLoop::mainRunLoop().run();
    unreachable!("the run loop returned")
}


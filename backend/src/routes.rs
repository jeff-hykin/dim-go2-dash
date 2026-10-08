// Every action this app has, as an endpoint (api.rs). The UI calls only these; so can Desktop's agent. Served as
// agent.json and repeated in dimos.yaml's `agent:` (`deno task check-endpoints` keeps them equal).

use std::time::Duration;

use serde_json::{json, Value};

use crate::api::{flag, handler, number, text, HttpError, Route};
use crate::drive::{self, commands_json, Command, COMMANDS};

pub const DESCRIPTION: &str = "Unitree Go2 robot dogs: find them (Bluetooth + LAN scan), put one on Wi-Fi over Bluetooth, \
and drive one live (stand, sit, jump, dance, walk) over WebRTC. Robot-moving and Wi-Fi-changing endpoints take dryRun: true.";

const DRY_RUN: &str = "true: check everything and say what would be sent, without touching the robot";

fn route(method: &'static str, path: &str, description: &str, params: Option<Value>, handler: crate::api::Handler) -> Route {
    Route { method, path: path.to_string(), description: description.to_string(), params, role: None, handler }
}

fn key_param() -> Value {
    json!({ "type": "string", "required": true, "description": "the robot's key from GET api/robots (its serial, else Bluetooth id, else IP)" })
}

async fn state(app: &std::sync::Arc<crate::app::App>) -> Value {
    let drive = match app.drive.lock().await.as_ref() {
        Some(drive) => drive.snapshot(),
        None => drive::no_session(),
    };
    json!({
        "robots": app.robots(),
        "scan": app.scan_state(),
        "wifi": app.wifi_state(),
        "drive": drive,
        "network": app.network_basic(),
        "accounts": app.accounts(),
        "commands": commands_json(),
        "setup": app.setup_state(),
    })
}

fn command_route(command: &'static Command) -> Route {
    route(
        "POST",
        &format!("api/drive/{}", command.name),
        &format!("{} (on the robot of the open drive session)", command.description),
        Some(json!({ "dryRun": { "type": "boolean", "description": DRY_RUN } })),
        handler(move |app, args| async move {
            let dry_run = flag(&args, "dryRun")?;
            let drive = app.drive.lock().await.clone();
            match drive {
                Some(drive) => drive.command(command, dry_run).await,
                None if dry_run => Ok(json!({
                    "dryRun": true,
                    "sent": false,
                    "command": command.name,
                    "note": "no drive session is open: nothing reaches a robot until POST api/drive/connect",
                })),
                None => Err(HttpError::conflict("no robot connected — POST api/drive/connect first")),
            }
        }),
    )
}

pub fn routes() -> Vec<Route> {
    let mut routes = vec![
        Route {
            role: Some("context"),
            ..route(
                "GET",
                "api/state",
                "Everything at once: scanned robots, scan status, Wi-Fi provisioning, the drive session, this computer's Wi-Fi, Unitree accounts, the robot commands",
                None,
                handler(|app, _| async move { Ok(state(&app).await) }),
            )
        },
        route(
            "GET",
            "api/robots",
            "The robots found by the last scan (name, serial, Bluetooth id, IP, whether its AES key is saved)",
            None,
            handler(|app, _| async move { Ok(json!(app.robots())) }),
        ),
        route(
            "GET",
            "api/robots/{key}",
            "One robot",
            Some(json!({ "key": key_param() })),
            handler(|app, args| async move { app.robot(&text(&args, "key").unwrap_or_default()) }),
        ),
        route(
            "POST",
            "api/scan",
            "Scan for Go2s over Bluetooth and the local network (passive: nothing is paired or changed): LAN discovery, plus an ARP sweep (one ping per address) joined to Bluetooth by MAC, for networks that drop discovery's multicast. Forgets the previous results; with wait (default) answers when the scan ends",
            Some(json!({
                "timeout": { "type": "number", "description": "seconds to scan (default 7; a sweep can run longer)" },
                "wait": { "type": "boolean", "description": "answer when the scan ends with the robots found (default true); false answers right away" },
                "sweep": { "type": "string", "description": "the ARP sweep: quick (default: IPs dogs were seen at, then the subnet if ≤ 1024 addresses, else the /24 around this computer), full (the whole subnet up to a /16; a /17 takes ~35 s on macOS, minutes on Linux), known (remembered IPs only) or off" },
            })),
            handler(|app, args| async move {
                let wait = if args.contains_key("wait") { flag(&args, "wait")? } else { true };
                let sweep = text(&args, "sweep").unwrap_or_default();
                app.scan(number(&args, "timeout")?.unwrap_or(7.0), wait, &sweep).await
            }),
        ),
        route("POST", "api/scan/stop", "Stop the running scan (and its sweep) now; what it found so far stays", None, handler(|app, _| async move { Ok(app.stop_scan()) })),
        route(
            "PUT",
            "api/robots/{key}/name",
            "Rename a robot (saved on this computer); an empty name goes back to its Bluetooth name",
            Some(json!({ "key": key_param(), "name": { "type": "string", "required": true } })),
            handler(|app, args| async move { app.rename(&text(&args, "key").unwrap_or_default(), &text(&args, "name").unwrap_or_default()) }),
        ),
        route(
            "PUT",
            "api/robots/{key}/aes-key",
            "Save a robot's per-device AES-128 key (32 hex chars; firmware ≥ 1.1.15 needs it to connect); empty clears it. POST api/accounts fetches keys instead",
            Some(json!({ "key": key_param(), "aesKey": { "type": "string", "required": true } })),
            handler(|app, args| async move { app.set_aes_key(&text(&args, "key").unwrap_or_default(), &text(&args, "aesKey").unwrap_or_default()) }),
        ),
        route(
            "PUT",
            "api/robots/{key}/ip",
            "Set the IP to drive a robot at when the scan can't see it on the network (a VPN, another subnet); empty clears it",
            Some(json!({ "key": key_param(), "ip": { "type": "string", "required": true } })),
            handler(|app, args| async move { app.set_ip(&text(&args, "key").unwrap_or_default(), &text(&args, "ip").unwrap_or_default()) }),
        ),
        route(
            "GET",
            "api/network",
            "This computer's Wi-Fi (SSID) and, on macOS, whether LAN discovery's multicast can reach the robots (a VPN can hijack it; `fix` is the command that routes it)",
            None,
            handler(|app, _| async move { Ok(app.network().await) }),
        ),
        route(
            "POST",
            "api/robots/{key}/wifi",
            "Put a robot on a Wi-Fi network: sends the SSID, password and country over Bluetooth, answers once the robot accepted them. Use dryRun: true to check without sending",
            Some(json!({
                "key": key_param(),
                "ssid": { "type": "string", "required": true },
                "password": { "type": "string" },
                "country": { "type": "string", "description": "two-letter Wi-Fi country code (default US)" },
                "dryRun": { "type": "boolean", "description": DRY_RUN },
            })),
            handler(|app, args| async move {
                app.provision_wifi(
                    &text(&args, "key").unwrap_or_default(),
                    &text(&args, "ssid").unwrap_or_default(),
                    &text(&args, "password").unwrap_or_default(),
                    &text(&args, "country").unwrap_or_else(|| "US".into()),
                    flag(&args, "dryRun")?,
                )
                .await
            }),
        ),
        route(
            "GET",
            "api/wifi",
            "The current or last Wi-Fi provisioning: status (idle, running, ok, error, cancelled, dry-run), robot, SSID and its log",
            None,
            handler(|app, _| async move { Ok(app.wifi_state()) }),
        ),
        route("POST", "api/wifi/cancel", "Abort the running Wi-Fi provisioning", None, handler(|app, _| async move { app.cancel_wifi().await })),
        route("DELETE", "api/wifi", "Forget the last Wi-Fi provisioning result (closes it in the UI)", None, handler(|app, _| async move { app.clear_wifi() })),
        route(
            "GET",
            "api/drive",
            "The drive session: robot, status (connecting, ready, reconnecting, error), mode (resting, rising, stand, pose), velocity, last command",
            None,
            handler(|app, _| async move {
                Ok(match app.drive.lock().await.as_ref() {
                    Some(drive) => drive.snapshot(),
                    None => drive::no_session(),
                })
            }),
        ),
        route(
            "POST",
            "api/drive/connect",
            "Connect to a robot for live control and camera (closes any other session); answers when ready. dryRun: true opens a simulated session that sends nothing",
            Some(json!({
                "robot": { "type": "string", "description": "a key from GET api/robots" },
                "ip": { "type": "string", "description": "drive this IP directly (no scan needed), or override the robot's" },
                "aesKey": { "type": "string", "description": "the robot's AES key for this connection (default: the saved one)" },
                "dryRun": { "type": "boolean", "description": "true: a simulated session — commands and moves are accepted and shown, never sent" },
            })),
            handler(|app, args| async move {
                let (robot, ip, name) = app.resolve_target(text(&args, "robot").as_deref(), text(&args, "ip").as_deref())?;
                drive::Drive::open(&app, robot, ip, name, text(&args, "aesKey"), flag(&args, "dryRun")?).await
            }),
        ),
        route(
            "POST",
            "api/drive/disconnect",
            "Close the drive session (stops any move first)",
            None,
            handler(|app, _| async move {
                match drive::close(&app).await {
                    Some(last) => Ok(json!({ "closed": last })),
                    None => Err(HttpError::conflict("no drive session is open")),
                }
            }),
        ),
        route(
            "POST",
            "api/drive/move",
            "Walk (or, in pose mode, tilt) at a velocity for a while, then stop; a new move replaces the last. Axes are -1..1 of 0.6 m/s forward, 0.4 m/s sideways, 1.1 rad/s turning (×2.2 with run). Needs stand first",
            Some(json!({
                "forward": { "type": "number", "description": "-1..1, positive = forward" },
                "strafe": { "type": "number", "description": "-1..1, positive = left" },
                "turn": { "type": "number", "description": "-1..1, positive = turn left" },
                "run": { "type": "boolean", "description": "2.2× faster" },
                "durationMs": { "type": "number", "description": "how long, 100..5000 (default 500)" },
                "dryRun": { "type": "boolean", "description": DRY_RUN },
            })),
            handler(|app, args| async move {
                let mut axes = [0.0; 3];
                for (i, name) in ["forward", "strafe", "turn"].iter().enumerate() {
                    let value = number(&args, name)?.unwrap_or(0.0);
                    if !(-1.0..=1.0).contains(&value) {
                        return Err(HttpError::bad(format!("{name} must be between -1 and 1")));
                    }
                    axes[i] = value;
                }
                let duration = number(&args, "durationMs")?.unwrap_or(500.0);
                if !(100.0..=5000.0).contains(&duration) {
                    return Err(HttpError::bad("durationMs must be between 100 and 5000"));
                }
                let dry_run = flag(&args, "dryRun")?;
                let drive = app.drive.lock().await.clone();
                match drive {
                    Some(drive) => drive.drive(axes[0], axes[1], axes[2], flag(&args, "run")?, Duration::from_millis(duration as u64), dry_run),
                    None if dry_run => Ok(json!({ "dryRun": true, "sent": false, "note": "no drive session is open: nothing reaches a robot until POST api/drive/connect" })),
                    None => Err(HttpError::conflict("no robot connected — POST api/drive/connect first")),
                }
            }),
        ),
        route(
            "POST",
            "api/drive/stop",
            "Stop moving now",
            Some(json!({ "dryRun": { "type": "boolean", "description": DRY_RUN } })),
            handler(|app, args| async move {
                let dry_run = flag(&args, "dryRun")?;
                let drive = app.drive.lock().await.clone();
                match drive {
                    Some(drive) => drive.stop(dry_run).await,
                    None if dry_run => Ok(json!({ "dryRun": true, "sent": false, "note": "no drive session is open" })),
                    None => Err(HttpError::conflict("no robot connected — POST api/drive/connect first")),
                }
            }),
        ),
        route(
            "POST",
            "api/drive/video",
            "For pages: a WebRTC offer (receive-only video) in, the answer that streams the robot's camera out",
            Some(json!({ "sdp": { "type": "string", "required": true, "description": "the page's SDP offer" } })),
            handler(|app, args| async move {
                let drive = app.drive.lock().await.clone().ok_or_else(|| HttpError::conflict("no robot connected — POST api/drive/connect first"))?;
                drive.video(text(&args, "sdp").unwrap_or_default()).await
            }),
        ),
        route(
            "GET",
            "api/accounts",
            "Saved Unitree app accounts (the AES keys come from them) and the robots bound to each",
            None,
            handler(|app, _| async move { Ok(json!(app.accounts())) }),
        ),
        route(
            "POST",
            "api/accounts",
            "Save a Unitree app account (on this computer only) and pull the AES key of every robot bound to it",
            Some(json!({ "email": { "type": "string", "required": true }, "password": { "type": "string", "required": true } })),
            handler(|app, args| async move { app.add_account(&text(&args, "email").unwrap_or_default(), &text(&args, "password").unwrap_or_default()).await }),
        ),
        route(
            "POST",
            "api/accounts/{email}/pull",
            "Sign in again and refresh the AES keys of the robots bound to a saved account",
            Some(json!({ "email": { "type": "string", "required": true } })),
            handler(|app, args| async move { app.pull_account(&text(&args, "email").unwrap_or_default()).await }),
        ),
        route(
            "DELETE",
            "api/accounts/{email}",
            "Forget a saved account (keys already pulled stay)",
            Some(json!({ "email": { "type": "string", "required": true } })),
            handler(|app, args| async move { app.remove_account(&text(&args, "email").unwrap_or_default()) }),
        ),
        route(
            "GET",
            "api/setup",
            "The first-run guide: its step (welcome, find, wifi, address, launch, done), mode (robot or replay) and the robot picked (key, name, ip, whether its AES key is saved)",
            None,
            handler(|app, _| async move { Ok(app.setup_state()) }),
        ),
        route(
            "PUT",
            "api/setup",
            "Move the first-run guide: any of step, robot (a key from GET api/robots; empty forgets it), ip (that robot's IP, saved for it; with no robot, the robot is that IP) and mode",
            Some(json!({
                "step": { "type": "string", "description": "welcome, find, wifi, address, launch or done" },
                "robot": { "type": "string", "description": "a key from GET api/robots; empty forgets the picked robot" },
                "ip": { "type": "string", "description": "the picked robot's IPv4 address" },
                "mode": { "type": "string", "description": "robot, or replay (no robot: try a recording)" },
            })),
            handler(|app, args| async move {
                app.update_setup(text(&args, "step").as_deref(), text(&args, "robot").as_deref(), text(&args, "ip").as_deref(), text(&args, "mode").as_deref())
            }),
        ),
        route("DELETE", "api/setup", "Start the first-run guide over (robots, names and keys stay)", None, handler(|app, _| async move { Ok(app.reset_setup()) })),
        route(
            "POST",
            "api/check-ip",
            "Whether a Go2 answers at an IP: opens and closes a connection to its WebRTC signaling port (9991), sends nothing",
            Some(json!({ "ip": { "type": "string", "required": true } })),
            handler(|app, args| async move { app.check_ip(&text(&args, "ip").unwrap_or_default()).await }),
        ),
        route(
            "GET",
            "api/launch",
            "{launch}: Desktop's dimos launch (blueprint, phase starting/running/stopped/failed, startup steps, problems), or null",
            None,
            handler(|app, _| async move { Ok(json!({ "launch": app.launch_state().await? })) }),
        ),
        route(
            "POST",
            "api/launch",
            "Launch dimos through Desktop for the guide's robot (robot_ip, plus its saved AES key) or a replay; one launch at a time (a running one answers 409: POST api/launch/stop first). dryRun: true says what it would send",
            Some(json!({
                "blueprint": { "type": "string", "description": "default unitree-go2-basic" },
                "replay": { "type": "boolean", "description": "play a recording instead of connecting to a robot" },
                "ip": { "type": "string", "description": "the robot's IP (default: the guide's robot)" },
                "default": { "type": "boolean", "description": "also save robot_ip in Desktop's global config, for the Launcher's launches" },
                "dryRun": { "type": "boolean", "description": "true: say what would be launched, launch nothing" },
            })),
            handler(|app, args| async move {
                app.launch(flag(&args, "replay")?, text(&args, "blueprint").as_deref(), text(&args, "ip").as_deref(), flag(&args, "dryRun")?, flag(&args, "default")?).await
            }),
        ),
        route("POST", "api/launch/stop", "Stop Desktop's dimos launch", None, handler(|app, _| async move { app.stop_launch().await })),
    ];
    routes.extend(COMMANDS.iter().map(command_route));
    routes
}

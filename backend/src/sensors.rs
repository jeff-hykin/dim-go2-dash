// The Go2's data-channel topics → recorded messages, named like dimos' go2 streams (`dimos --record run unitree-go2`):
//   rt/utlidar/voxel_map_compressed → /lidar         sensor_msgs/PointCloud2 (frame world; the robot's local voxel window)
//   rt/utlidar/robot_pose           → /odom          geometry_msgs/PoseStamped (frame world)  + /tf world→base_link
//   rt/lf/sportmodestate            → /imu           sensor_msgs/Imu (base_link: quaternion, gyroscope, accelerometer)
//   rt/lf/lowstate                  → /battery       sensor_msgs/BatteryState (≤ 1 Hz, like Stash's cockpit branch)
//                                     /joint_state   sensor_msgs/JointState (the 12 leg joints' positions)
// Times: the robot's own time for the message (robot_time_ns) placed on this computer's clock (clock::RobotClock); the
// arrival time when the message carries none or it doesn't advance.

use serde_json::Value;

use crate::msg::{Battery, Header, Msg, Transform};

pub const LIDAR: &str = "rt/utlidar/voxel_map_compressed";
pub const POSE: &str = "rt/utlidar/robot_pose";
pub const LOWSTATE: &str = "rt/lf/lowstate";
pub const SPORT_STATE: &str = "rt/lf/sportmodestate";
/// the lidar's own state (Unitree's ULIDAR_STATE), kept as text: why point clouds stop
pub const LIDAR_STATE: &str = "rt/utlidar/lidar_state";
/// which of the dog's services are running (Unitree's SERVICE_STATE), kept as text
pub const SERVICE_STATE: &str = "rt/servicestate";
/// what a recording subscribes to
pub const TOPICS: [&str; 6] = [LIDAR, POSE, LOWSTATE, SPORT_STATE, LIDAR_STATE, SERVICE_STATE];

/// Go2 leg joints in motor_state order (unitree_go: FR, FL, RR, RL × hip, thigh, calf)
pub const JOINTS: [&str; 12] = [
    "FR_hip_joint",
    "FR_thigh_joint",
    "FR_calf_joint",
    "FL_hip_joint",
    "FL_thigh_joint",
    "FL_calf_joint",
    "RR_hip_joint",
    "RR_thigh_joint",
    "RR_calf_joint",
    "RL_hip_joint",
    "RL_thigh_joint",
    "RL_calf_joint",
];

/// One message for the file: its topic and the message.
pub struct Out {
    pub topic: &'static str,
    pub msg: Msg,
}

fn f(value: &Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

fn xyz(value: &Value) -> [f64; 3] {
    [f(&value["x"]), f(&value["y"]), f(&value["z"])]
}

fn array3(value: &Value) -> Option<[f64; 3]> {
    let a = value.as_array()?;
    Some([f(a.first()?), f(a.get(1)?), f(a.get(2)?)])
}

/// The voxel bitfield → points: LZ4 block (`src_size` bytes out), then 2048 bytes per z slice of a 128×128 grid, one
/// bit per voxel, most significant bit first; a point = (x, y, z) × resolution + origin. (dimos' native decoder.)
pub fn decode_voxels(meta: &Value, compressed: &[u8]) -> Result<Vec<[f32; 3]>, String> {
    let size = meta["src_size"].as_u64().ok_or("the lidar message has no src_size")? as usize;
    if size > 64 * 1024 * 1024 {
        return Err("the lidar message's src_size is implausible".into());
    }
    let raw = lz4_flex::block::decompress(compressed, size).map_err(|e| format!("lidar LZ4: {e}"))?;
    let resolution = meta["resolution"].as_f64().unwrap_or(0.05);
    let origin = array3(&meta["origin"]).unwrap_or_default();
    let mut points = Vec::new();
    for (index, &byte) in raw.iter().enumerate() {
        if byte == 0 {
            continue;
        }
        let z = index / 0x800;
        let slice = index % 0x800;
        let y = slice / 0x10;
        let x_base = (slice % 0x10) * 8;
        for bit in 0..8 {
            if byte & (0x80 >> bit) != 0 {
                let x = x_base + bit;
                points.push([
                    (x as f64 * resolution + origin[0]) as f32,
                    (y as f64 * resolution + origin[1]) as f32,
                    (z as f64 * resolution + origin[2]) as f32,
                ]);
            }
        }
    }
    Ok(points)
}

/// The camera's fixed mount (dimos' BASE_TO_OPTICAL): base_link → camera_link → camera_optical.
pub fn camera_mount() -> [Transform<'static>; 2] {
    [
        Transform { parent: "base_link", child: "camera_link", translation: [0.3, 0.0, 0.0], rotation: [0.0, 0.0, 0.0, 1.0] },
        Transform { parent: "camera_link", child: "camera_optical", translation: [0.0; 3], rotation: [-0.5, 0.5, -0.5, 0.5] },
    ]
}

/// What a lowstate is worth recording at most this often (the battery changes slowly)
pub const BATTERY_EVERY_NS: u64 = 1_000_000_000;

/// The robot's own time for a data-channel message, ns on its clock: lidar's `stamp` (seconds), robot_pose's header
/// stamp, sportmodestate's `stamp`, lowstate's `tick` (ms since boot). None when it has none.
pub fn robot_time_ns(topic: &str, json: &Value) -> Option<i64> {
    let data = &json["data"];
    let sec_nanosec = |stamp: &Value| Some(stamp["sec"].as_i64()? * 1_000_000_000 + stamp["nanosec"].as_i64().unwrap_or(0));
    let ns = match topic {
        // whole seconds and the fraction apart: seconds × 1e9 as one f64 loses ~100 ns at today's epoch
        LIDAR => data["stamp"].as_f64().map(|s| s.trunc() as i64 * 1_000_000_000 + (s.fract() * 1e9).round() as i64),
        POSE => sec_nanosec(&data["header"]["stamp"]),
        SPORT_STATE => sec_nanosec(&data["stamp"]),
        LOWSTATE => data["tick"].as_i64().map(|ms| ms * 1_000_000),
        _ => None,
    }?;
    (ns > 0).then_some(ns)
}

/// Converts one data-channel message. `battery_due` says whether a battery message may go out now (≤ 1 Hz).
pub fn convert(topic: &str, json: &Value, binary: Option<&[u8]>, now_ns: u64, battery_due: bool) -> Vec<Out> {
    let data = &json["data"];
    let mut out = Vec::new();
    match topic {
        LIDAR => {
            let Some(binary) = binary else {
                crate::rx::count("lidar no-binary", 0);
                return out;
            };
            match decode_voxels(data, binary) {
                Ok(points) => out
                    .push(Out { topic: "/lidar", msg: Msg::Points(Header { stamp_ns: now_ns, frame_id: "world" }, points) }),
                Err(err) => {
                    crate::rx::count("lidar decode-error", binary.len());
                    eprintln!("lidar: {err}");
                }
            }
        }
        LIDAR_STATE | SERVICE_STATE => {
            let stream = if topic == LIDAR_STATE { "/lidar_state" } else { "/service_state" };
            out.push(Out { topic: stream, msg: Msg::Text(data.to_string()) });
        }
        POSE => {
            let pose = &data["pose"];
            let position = xyz(&pose["position"]);
            let o = &pose["orientation"];
            let orientation = [f(&o["x"]), f(&o["y"]), f(&o["z"]), o["w"].as_f64().unwrap_or(1.0)];
            out.push(Out {
                topic: "/odom",
                msg: Msg::Pose(Header { stamp_ns: now_ns, frame_id: "world" }, position, orientation),
            });
            let [mount, optical] = camera_mount();
            let body = Transform { parent: "world", child: "base_link", translation: position, rotation: orientation };
            out.push(Out { topic: "/tf", msg: Msg::Tf(now_ns, vec![body, mount, optical]) });
        }
        SPORT_STATE => {
            let imu = &data["imu_state"];
            // Unitree's quaternion is w, x, y, z
            if let Some(q) = imu["quaternion"].as_array().filter(|q| q.len() == 4) {
                out.push(Out {
                    topic: "/imu",
                    msg: Msg::Imu {
                        header: Header { stamp_ns: now_ns, frame_id: "base_link" },
                        orientation: [f(&q[1]), f(&q[2]), f(&q[3]), f(&q[0])],
                        angular_velocity: array3(&imu["gyroscope"]),
                        linear_acceleration: array3(&imu["accelerometer"]),
                    },
                });
            }
        }
        LOWSTATE => {
            if let Some(motors) = data["motor_state"].as_array().filter(|m| m.len() >= 12) {
                let positions: Vec<f64> = motors[..12].iter().map(|m| f(&m["q"])).collect();
                out.push(Out {
                    topic: "/joint_state",
                    msg: Msg::Joints(Header { stamp_ns: now_ns, frame_id: "base_link" }, &JOINTS, positions),
                });
            }
            let bms = &data["bms_state"];
            if battery_due && bms.is_object() {
                let temperatures: Vec<f32> = bms["bq_ntc"].as_array().map(|t| t.iter().map(|v| f(v) as f32).collect()).unwrap_or_default();
                let battery = Battery {
                    voltage: f(&data["power_v"]) as f32,
                    current: (f(&bms["current"]) / 1000.0) as f32,
                    temperature: temperatures.first().copied().unwrap_or(f32::NAN),
                    percentage: (f(&bms["soc"]) / 100.0) as f32,
                    cell_temperatures: temperatures,
                };
                out.push(Out {
                    topic: "/battery",
                    msg: Msg::Battery(Header { stamp_ns: now_ns, frame_id: "base_link" }, battery),
                });
            }
        }
        _ => {}
    }
    out
}

/// A lidar message as the robot frames it (the mock robot sends these): the lidar framing, its JSON, then LZ4 voxels
pub fn lidar_message(voxels: &[u8], origin: [f64; 3]) -> Vec<u8> {
    let compressed = lz4_flex::block::compress(voxels);
    let header = serde_json::json!({ "type": "msg", "topic": LIDAR, "data": {
        "origin": origin, "resolution": 0.05, "src_size": voxels.len(), "width": [128, 128, 38], "frame_id": "odom", "stamp": 1.0
    }})
    .to_string();
    let mut buf = vec![2, 0, 0, 0];
    buf.extend_from_slice(&(header.len() as u32).to_le_bytes());
    buf.extend_from_slice(&[0, 0, 0, 0]);
    buf.extend_from_slice(header.as_bytes());
    buf.extend_from_slice(&compressed);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_topic_gives_the_robot_time_it_carries() {
        let lidar = serde_json::json!({ "data": { "stamp": 1746565669.25 } });
        assert_eq!(robot_time_ns(LIDAR, &lidar), Some(1_746_565_669_250_000_000));
        let pose = serde_json::json!({ "data": { "header": { "stamp": { "sec": 1746565669, "nanosec": 448350564 } } } });
        assert_eq!(robot_time_ns(POSE, &pose), Some(1_746_565_669_448_350_564));
        let sport = serde_json::json!({ "data": { "stamp": { "sec": 12, "nanosec": 5 } } });
        assert_eq!(robot_time_ns(SPORT_STATE, &sport), Some(12_000_000_005));
        let low = serde_json::json!({ "data": { "tick": 1234 } });
        assert_eq!(robot_time_ns(LOWSTATE, &low), Some(1_234_000_000));
        // none, or a zero stamp: the arrival time is used
        assert_eq!(robot_time_ns(LOWSTATE, &serde_json::json!({ "data": {} })), None);
        assert_eq!(robot_time_ns(LIDAR, &serde_json::json!({ "data": { "stamp": 0.0 } })), None);
    }
    use serde_json::json;

    #[test]
    fn lidar_frame_decodes_to_points() {
        let mut voxels = vec![0u8; 0x800 * 2];
        voxels[0] = 0b1000_0001; // x 0 and 7, y 0, z 0
        voxels[0x800 + 0x10 + 1] = 0b0100_0000; // z 1, y 1, x 8 + 1
        let message = crate::robot_rtc::parse_binary(&super::lidar_message(&voxels, [1.0, 2.0, 3.0])).unwrap();
        assert_eq!(message.topic, LIDAR);
        let points = decode_voxels(&message.json["data"], message.binary.as_deref().unwrap()).unwrap();
        assert_eq!(points.len(), 3);
        assert_eq!(points[0], [1.0, 2.0, 3.0]);
        assert!((points[1][0] - 1.35).abs() < 1e-6);
        assert!((points[2][0] - 1.45).abs() < 1e-6 && (points[2][1] - 2.05).abs() < 1e-6 && (points[2][2] - 3.05).abs() < 1e-6);
        let out = convert(LIDAR, &message.json, message.binary.as_deref(), 1, false);
        assert_eq!(out[0].topic, "/lidar");
    }

    #[test]
    fn lowstate_battery_is_rate_limited() {
        let low = json!({ "data": { "motor_state": (0..20).map(|i| json!({ "q": i as f64 })).collect::<Vec<_>>(),
            "bms_state": { "soc": 55, "current": -2481, "bq_ntc": [30, 29] }, "power_v": 28.3 } });
        let topics = |due| convert(LOWSTATE, &low, None, 1, due).iter().map(|o| o.topic).collect::<Vec<_>>();
        assert_eq!(topics(true), ["/joint_state", "/battery"]);
        assert_eq!(topics(false), ["/joint_state"]);
    }

    #[test]
    fn lidar_state_is_kept_as_text_and_a_cloud_without_voxels_is_counted() {
        let state = serde_json::json!({ "type": "msg", "topic": LIDAR_STATE, "data": { "cloud_frequency": 7.5, "error_state": 0 } });
        let out = convert(LIDAR_STATE, &state, None, 1, false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].topic, "/lidar_state");
        assert!(matches!(&out[0].msg, Msg::Text(text) if text.contains("cloud_frequency")));
        let before = crate::rx::totals().get("lidar no-binary").map(|e| e.0).unwrap_or(0);
        let cloud = serde_json::json!({ "type": "msg", "topic": LIDAR, "data": {} });
        assert!(convert(LIDAR, &cloud, None, 1, false).is_empty());
        assert_eq!(crate::rx::totals()["lidar no-binary"].0, before + 1);
    }
}

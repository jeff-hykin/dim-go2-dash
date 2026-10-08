// The Go2's data-channel topics → recorded messages, named like dimos' go2 streams (`dimos --record run unitree-go2`):
//   rt/utlidar/voxel_map_compressed → /lidar        sensor_msgs/PointCloud2 (frame world; the robot's local voxel window)
//   rt/utlidar/robot_pose           → /odom          geometry_msgs/PoseStamped (frame world)  + /tf world→base_link
//   rt/lf/sportmodestate            → /imu           sensor_msgs/Imu (base_link: quaternion, gyroscope, accelerometer)
//   rt/lf/lowstate                  → /battery       sensor_msgs/BatteryState (≤ 1 Hz, like Stash's cockpit branch)
//                                     /joint_states  sensor_msgs/JointState (the 12 leg joints' positions)
// Times: the header stamp is when the message arrived (dimos does the same: the robot's clock isn't this computer's).

use serde_json::Value;

use crate::cdr::{self, Encoded, Header, Transform};

pub const LIDAR: &str = "rt/utlidar/voxel_map_compressed";
pub const POSE: &str = "rt/utlidar/robot_pose";
pub const LOWSTATE: &str = "rt/lf/lowstate";
pub const SPORT_STATE: &str = "rt/lf/sportmodestate";
/// what a recording subscribes to
pub const TOPICS: [&str; 4] = [LIDAR, POSE, LOWSTATE, SPORT_STATE];

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

/// One message for the file: its topic, CDR, and the robot's own time when it gave one.
pub struct Out {
    pub topic: &'static str,
    pub encoded: Encoded,
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

/// Converts one data-channel message. `battery_due` says whether a battery message may go out now (≤ 1 Hz).
pub fn convert(topic: &str, json: &Value, binary: Option<&[u8]>, now_ns: u64, battery_due: bool) -> Vec<Out> {
    let data = &json["data"];
    let mut out = Vec::new();
    match topic {
        LIDAR => {
            let Some(binary) = binary else { return out };
            match decode_voxels(data, binary) {
                Ok(points) => out
                    .push(Out { topic: "/lidar", encoded: cdr::point_cloud_xyz(&Header { stamp_ns: now_ns, frame_id: "world" }, &points) }),
                Err(err) => eprintln!("lidar: {err}"),
            }
        }
        POSE => {
            let pose = &data["pose"];
            let position = xyz(&pose["position"]);
            let o = &pose["orientation"];
            let orientation = [f(&o["x"]), f(&o["y"]), f(&o["z"]), o["w"].as_f64().unwrap_or(1.0)];
            out.push(Out {
                topic: "/odom",
                encoded: cdr::pose_stamped(&Header { stamp_ns: now_ns, frame_id: "world" }, position, orientation),
            });
            let [mount, optical] = camera_mount();
            let body = Transform { parent: "world", child: "base_link", translation: position, rotation: orientation };
            out.push(Out { topic: "/tf", encoded: cdr::tf(now_ns, &[body, mount, optical]) });
        }
        SPORT_STATE => {
            let imu = &data["imu_state"];
            // Unitree's quaternion is w, x, y, z
            if let Some(q) = imu["quaternion"].as_array().filter(|q| q.len() == 4) {
                out.push(Out {
                    topic: "/imu",
                    encoded: cdr::imu(
                        &Header { stamp_ns: now_ns, frame_id: "base_link" },
                        [f(&q[1]), f(&q[2]), f(&q[3]), f(&q[0])],
                        array3(&imu["gyroscope"]),
                        array3(&imu["accelerometer"]),
                    ),
                });
            }
        }
        LOWSTATE => {
            if let Some(motors) = data["motor_state"].as_array().filter(|m| m.len() >= 12) {
                let positions: Vec<f64> = motors[..12].iter().map(|m| f(&m["q"])).collect();
                out.push(Out {
                    topic: "/joint_states",
                    encoded: cdr::joint_state(&Header { stamp_ns: now_ns, frame_id: "base_link" }, &JOINTS, &positions),
                });
            }
            let bms = &data["bms_state"];
            if battery_due && bms.is_object() {
                let temperatures: Vec<f32> = bms["bq_ntc"].as_array().map(|t| t.iter().map(|v| f(v) as f32).collect()).unwrap_or_default();
                let battery = cdr::Battery {
                    voltage: f(&data["power_v"]) as f32,
                    current: (f(&bms["current"]) / 1000.0) as f32,
                    temperature: temperatures.first().copied().unwrap_or(f32::NAN),
                    percentage: (f(&bms["soc"]) / 100.0) as f32,
                    cell_temperatures: temperatures,
                };
                out.push(Out {
                    topic: "/battery",
                    encoded: cdr::battery_state(&Header { stamp_ns: now_ns, frame_id: "base_link" }, &battery),
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
        assert_eq!(topics(true), ["/joint_states", "/battery"]);
        assert_eq!(topics(false), ["/joint_states"]);
    }
}

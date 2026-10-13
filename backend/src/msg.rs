// A recorded message before it is stored: dimos' LCM types (dimos-lcm's generated Rust structs, the bytes dimos'
// LcmCodec / JpegCodec decode), written by dimos' own recorder stores (record.rs) to a .db or an mcap alike. Each stream
// is the topic without its slash, typed by the dimos class its rows decode to (`stream_type`, what dimos' own Recorder
// would register):
//   color_image   dimos.msgs.sensor_msgs.Image.Image          codec jpeg (raw: lcm, or lz4+lcm when compressed)
//   camera_info   dimos.msgs.sensor_msgs.CameraInfo.CameraInfo
//   lidar         dimos.msgs.sensor_msgs.PointCloud2.PointCloud2 (lz4+lcm when compressed)
//   odom          dimos.msgs.geometry_msgs.PoseStamped.PoseStamped
//   tf            dimos.msgs.tf2_msgs.TFMessage.TFMessage      one row per transform, like dimos' Recorder
//   imu           dimos.msgs.sensor_msgs.Imu.Imu
//   battery       dimos_lcm.sensor_msgs.BatteryState.BatteryState (dimos.msgs has no BatteryState: the raw dimos-lcm class)
//   joint_state   dimos.msgs.sensor_msgs.JointState.JointState
//   joystick      dimos.msgs.sensor_msgs.Joy.Joy
//   cmd_vel       dimos.msgs.geometry_msgs.Twist.Twist
//   robot_action, logs, metadata   dimos.msgs.std_msgs.String.String (JSON text)

use lcm_msgs::{geometry_msgs as geo, sensor_msgs as sensor, std_msgs};

/// A message header: a time (ns since the epoch) and a frame.
#[derive(Clone, Copy)]
pub struct Header<'a> {
    pub stamp_ns: u64,
    pub frame_id: &'a str,
}

#[derive(Clone)]
pub struct CameraInfo<'a> {
    pub width: u32,
    pub height: u32,
    pub distortion_model: &'a str,
    pub d: &'a [f64],
    pub k: [f64; 9],
    pub r: [f64; 9],
    pub p: [f64; 12],
}

#[derive(Clone)]
pub struct Transform<'a> {
    pub parent: &'a str,
    pub child: &'a str,
    pub translation: [f64; 3],
    /// x, y, z, w
    pub rotation: [f64; 4],
}

#[derive(Clone)]
pub struct Battery {
    pub voltage: f32,
    pub current: f32,
    pub temperature: f32,
    /// 0..1
    pub percentage: f32,
    pub cell_temperatures: Vec<f32>,
}

/// power_supply_status from the current: charging, discharging, not charging
pub fn battery_status(current: f32) -> u8 {
    if current > 0.05 {
        1
    } else if current < -0.05 {
        2
    } else {
        3
    }
}

/// The dimos class and codec id of stream `name`, registered before its first row: `raw_images` stores the camera as
/// LCM pixels instead of JPEG, `compress` puts big LCM payloads (points, raw pixels) in LZ4 frames, dimos' "lz4+lcm".
pub fn stream_type(name: &str, raw_images: bool, compress: bool) -> Option<(&'static str, &'static str)> {
    let big = if compress { "lz4+lcm" } else { "lcm" };
    Some(match name {
        "color_image" => ("dimos.msgs.sensor_msgs.Image.Image", if raw_images { big } else { "jpeg" }),
        "camera_info" => ("dimos.msgs.sensor_msgs.CameraInfo.CameraInfo", "lcm"),
        "lidar" => ("dimos.msgs.sensor_msgs.PointCloud2.PointCloud2", big),
        "odom" => ("dimos.msgs.geometry_msgs.PoseStamped.PoseStamped", "lcm"),
        "tf" => ("dimos.msgs.tf2_msgs.TFMessage.TFMessage", "lcm"),
        "imu" => ("dimos.msgs.sensor_msgs.Imu.Imu", "lcm"),
        "battery" => ("dimos_lcm.sensor_msgs.BatteryState.BatteryState", "lcm"),
        "joint_state" => ("dimos.msgs.sensor_msgs.JointState.JointState", "lcm"),
        "joystick" => ("dimos.msgs.sensor_msgs.Joy.Joy", "lcm"),
        "cmd_vel" => ("dimos.msgs.geometry_msgs.Twist.Twist", "lcm"),
        "robot_action" | "logs" | "metadata" => ("dimos.msgs.std_msgs.String.String", "lcm"),
        _ => return None,
    })
}

pub enum Msg {
    Image {
        header: Header<'static>,
        width: u32,
        height: u32,
        rgb: Vec<u8>,
    },
    Jpeg {
        header: Header<'static>,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    CameraInfo(Header<'static>, CameraInfo<'static>),
    Points(Header<'static>, Vec<[f32; 3]>),
    /// position, orientation (x, y, z, w)
    Pose(Header<'static>, [f64; 3], [f64; 4]),
    Tf(u64, Vec<Transform<'static>>),
    Imu {
        header: Header<'static>,
        orientation: [f64; 4],
        angular_velocity: Option<[f64; 3]>,
        linear_acceleration: Option<[f64; 3]>,
    },
    Battery(Header<'static>, Battery),
    Joints(Header<'static>, &'static [&'static str], Vec<f64>),
    Joy(Header<'static>, Vec<f32>, Vec<i32>),
    Twist([f64; 3], [f64; 3]),
    Text(String),
}

/// A message as stored rows: (source time in ns, if it has one; LCM bytes) each.
pub type Rows = Vec<(Option<u64>, Vec<u8>)>;

impl Msg {
    /// Its LCM rows (before any LZ4 framing its stream's codec adds, record.rs).
    pub fn rows(self) -> Rows {
        let (stamp, data) = match self {
            Msg::Image { header, width, height, rgb } => {
                let image = image(&header, width, height, "rgb8", width * 3, rgb);
                (Some(header), image.encode())
            }
            // dimos' JpegCodec: an LCM Image whose data is the JPEG, encoding "jpeg"
            Msg::Jpeg { header, width, height, data } => {
                (Some(header), image(&header, width, height, "jpeg", 0, data).encode())
            }
            Msg::CameraInfo(header, info) => {
                let info = sensor::CameraInfo {
                    header: lcm_header(&header),
                    height: info.height as i32,
                    width: info.width as i32,
                    distortion_model: info.distortion_model.into(),
                    D: info.d.to_vec(),
                    K: info.k,
                    R: info.r,
                    P: info.p,
                    ..Default::default()
                };
                (Some(header), info.encode())
            }
            Msg::Points(header, points) => {
                let fields = ["x", "y", "z"]
                    .iter()
                    .enumerate()
                    .map(|(i, name)| sensor::PointField { name: (*name).into(), offset: i as i32 * 4, datatype: 7, count: 1 })
                    .collect();
                let cloud = sensor::PointCloud2 {
                    header: lcm_header(&header),
                    height: 1,
                    width: points.len() as i32,
                    fields,
                    is_bigendian: false,
                    point_step: 12,
                    row_step: 12 * points.len() as i32,
                    data: points.iter().flatten().flat_map(|v| v.to_le_bytes()).collect(),
                    is_dense: true,
                };
                (Some(header), cloud.encode())
            }
            Msg::Pose(header, position, orientation) => {
                let pose = geo::PoseStamped { header: lcm_header(&header), pose: pose(position, orientation) };
                (Some(header), pose.encode())
            }
            Msg::Tf(stamp_ns, transforms) => {
                let header = Header { stamp_ns, frame_id: "" };
                let rows = transforms
                    .iter()
                    .map(|t| {
                        let stamped = geo::TransformStamped {
                            header: lcm_header(&Header { stamp_ns, frame_id: t.parent }),
                            child_frame_id: t.child.into(),
                            transform: geo::Transform { translation: vector3(t.translation), rotation: quaternion(t.rotation) },
                        };
                        (source_ns(&header), lcm_msgs::tf2_msgs::TFMessage { transforms: vec![stamped] }.encode())
                    })
                    .collect();
                return rows;
            }
            Msg::Imu { header, orientation, angular_velocity, linear_acceleration } => {
                let unknown = {
                    let mut c = [0.0; 9];
                    c[0] = -1.0;
                    c
                };
                let imu = sensor::Imu {
                    header: lcm_header(&header),
                    orientation: quaternion(orientation),
                    orientation_covariance: [0.0; 9],
                    angular_velocity: vector3(angular_velocity.unwrap_or_default()),
                    angular_velocity_covariance: if angular_velocity.is_some() { [0.0; 9] } else { unknown },
                    linear_acceleration: vector3(linear_acceleration.unwrap_or_default()),
                    linear_acceleration_covariance: if linear_acceleration.is_some() { [0.0; 9] } else { unknown },
                };
                (Some(header), imu.encode())
            }
            Msg::Battery(header, battery) => {
                let state = sensor::BatteryState {
                    header: lcm_header(&header),
                    voltage: battery.voltage,
                    temperature: battery.temperature,
                    current: battery.current,
                    charge: f32::NAN,
                    capacity: f32::NAN,
                    design_capacity: f32::NAN,
                    percentage: battery.percentage,
                    power_supply_status: battery_status(battery.current),
                    power_supply_technology: 2,
                    present: true,
                    cell_temperature: battery.cell_temperatures,
                    ..Default::default()
                };
                (Some(header), state.encode())
            }
            Msg::Joints(header, names, positions) => {
                let joints = sensor::JointState {
                    header: lcm_header(&header),
                    name: names.iter().map(|n| (*n).into()).collect(),
                    position: positions,
                    ..Default::default()
                };
                (Some(header), joints.encode())
            }
            Msg::Joy(header, axes, buttons) => {
                let joy = sensor::Joy { header: lcm_header(&header), axes, buttons };
                (Some(header), joy.encode())
            }
            Msg::Twist(linear, angular) => {
                let twist = geo::Twist { linear: vector3(linear), angular: vector3(angular) };
                (None, twist.encode())
            }
            Msg::Text(text) => (None, std_msgs::String { data: text }.encode()),
        };
        vec![(stamp.and_then(|h| source_ns(&h)), data)]
    }
}

fn source_ns(header: &Header) -> Option<u64> {
    (header.stamp_ns > 0).then_some(header.stamp_ns)
}

fn lcm_header(header: &Header) -> std_msgs::Header {
    std_msgs::Header {
        seq: 0,
        stamp: std_msgs::Time { sec: (header.stamp_ns / 1_000_000_000) as i32, nsec: (header.stamp_ns % 1_000_000_000) as i32 },
        frame_id: header.frame_id.into(),
    }
}

fn image(header: &Header, width: u32, height: u32, encoding: &str, step: u32, data: Vec<u8>) -> sensor::Image {
    sensor::Image {
        header: lcm_header(header),
        height: height as i32,
        width: width as i32,
        encoding: encoding.into(),
        is_bigendian: 0,
        step: step as i32,
        data,
    }
}

fn vector3([x, y, z]: [f64; 3]) -> geo::Vector3 {
    geo::Vector3 { x, y, z }
}

fn quaternion([x, y, z, w]: [f64; 4]) -> geo::Quaternion {
    geo::Quaternion { x, y, z, w }
}

fn pose([x, y, z]: [f64; 3], orientation: [f64; 4]) -> geo::Pose {
    geo::Pose { position: geo::Point { x, y, z }, orientation: quaternion(orientation) }
}

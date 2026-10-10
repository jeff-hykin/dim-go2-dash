// A recorded message before it is encoded for the file: ROS2 CDR for an mcap (cdr.rs), dimos' LCM types for a .db
// (dimos-lcm's generated Rust structs, the bytes dimos' LcmCodec / JpegCodec decode). Each .db stream is the topic
// without its slash, typed by the dimos class its rows decode to (what dimos' own Recorder would register):
//   color_image   dimos.msgs.sensor_msgs.Image.Image          codec jpeg (raw: lcm, or lz4+lcm when compressed)
//   camera_info   dimos.msgs.sensor_msgs.CameraInfo.CameraInfo
//   lidar         dimos.msgs.sensor_msgs.PointCloud2.PointCloud2 (lz4+lcm when compressed)
//   odom          dimos.msgs.geometry_msgs.PoseStamped.PoseStamped
//   tf            dimos.msgs.tf2_msgs.TFMessage.TFMessage      one row per transform, like dimos' Recorder
//   imu           dimos.msgs.sensor_msgs.Imu.Imu
//   battery       dimos_lcm.sensor_msgs.BatteryState.BatteryState (dimos.msgs has no BatteryState: the raw dimos-lcm class)
//   joint_states  dimos.msgs.sensor_msgs.JointState.JointState
//   joystick      dimos.msgs.sensor_msgs.Joy.Joy
//   cmd_vel       dimos.msgs.geometry_msgs.Twist.Twist
//   robot_action, logs, metadata   dimos.msgs.std_msgs.String.String (JSON text, as in the mcap)

use lcm_msgs::{geometry_msgs as geo, sensor_msgs as sensor, std_msgs};

use crate::cdr::{self, Battery, CameraInfo, Encoded, Header, Transform};

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

/// A message as .db rows: the dimos class and codec of its stream, and (source time in s, stored bytes) per row.
pub struct Rows {
    pub payload_type: &'static str,
    pub codec: &'static str,
    pub rows: Vec<(Option<f64>, Vec<u8>)>,
}

impl Msg {
    pub fn cdr(&self) -> Encoded {
        match self {
            Msg::Image { header, width, height, rgb } => cdr::image(header, *width, *height, rgb),
            Msg::Jpeg { header, data, .. } => cdr::compressed_image(header, "jpeg", data),
            Msg::CameraInfo(header, info) => cdr::camera_info(header, info),
            Msg::Points(header, points) => cdr::point_cloud_xyz(header, points),
            Msg::Pose(header, position, orientation) => cdr::pose_stamped(header, *position, *orientation),
            Msg::Tf(stamp_ns, transforms) => cdr::tf(*stamp_ns, transforms),
            Msg::Imu { header, orientation, angular_velocity, linear_acceleration } => {
                cdr::imu(header, *orientation, *angular_velocity, *linear_acceleration)
            }
            Msg::Battery(header, battery) => cdr::battery_state(header, battery),
            Msg::Joints(header, names, positions) => cdr::joint_state(header, names, positions),
            Msg::Joy(header, axes, buttons) => cdr::joy(header, axes, buttons),
            Msg::Twist(linear, angular) => cdr::twist(*linear, *angular),
            Msg::Text(text) => cdr::string(text),
        }
    }

    /// The pose a .db row of this message anchors the rows after it to (dimos stores each row's robot pose): odom's.
    pub fn robot_pose(&self) -> Option<[f64; 7]> {
        match self {
            Msg::Pose(_, p, q) => Some([p[0], p[1], p[2], q[0], q[1], q[2], q[3]]),
            _ => None,
        }
    }

    /// `compress`: big LCM payloads (points, raw pixels) go in LZ4 frames, dimos' "lz4+lcm".
    pub fn dimos(self, compress: bool) -> Rows {
        let lz4 = |codec: &'static str| if compress { "lz4+lcm" } else { codec };
        let (payload_type, codec, stamp, data) = match self {
            Msg::Image { header, width, height, rgb } => {
                let image = image(&header, width, height, "rgb8", width * 3, rgb);
                ("dimos.msgs.sensor_msgs.Image.Image", lz4("lcm"), Some(header), image.encode())
            }
            // dimos' JpegCodec: an LCM Image whose data is the JPEG, encoding "jpeg"
            Msg::Jpeg { header, width, height, data } => {
                ("dimos.msgs.sensor_msgs.Image.Image", "jpeg", Some(header), image(&header, width, height, "jpeg", 0, data).encode())
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
                ("dimos.msgs.sensor_msgs.CameraInfo.CameraInfo", "lcm", Some(header), info.encode())
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
                ("dimos.msgs.sensor_msgs.PointCloud2.PointCloud2", lz4("lcm"), Some(header), cloud.encode())
            }
            Msg::Pose(header, position, orientation) => {
                let pose = geo::PoseStamped { header: lcm_header(&header), pose: pose(position, orientation) };
                ("dimos.msgs.geometry_msgs.PoseStamped.PoseStamped", "lcm", Some(header), pose.encode())
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
                        (seconds(&header), lcm_msgs::tf2_msgs::TFMessage { transforms: vec![stamped] }.encode())
                    })
                    .collect();
                return Rows { payload_type: "dimos.msgs.tf2_msgs.TFMessage.TFMessage", codec: "lcm", rows };
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
                ("dimos.msgs.sensor_msgs.Imu.Imu", "lcm", Some(header), imu.encode())
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
                    power_supply_status: cdr::battery_status(battery.current),
                    power_supply_technology: 2,
                    present: true,
                    cell_temperature: battery.cell_temperatures,
                    ..Default::default()
                };
                ("dimos_lcm.sensor_msgs.BatteryState.BatteryState", "lcm", Some(header), state.encode())
            }
            Msg::Joints(header, names, positions) => {
                let joints = sensor::JointState {
                    header: lcm_header(&header),
                    name: names.iter().map(|n| (*n).into()).collect(),
                    position: positions,
                    ..Default::default()
                };
                ("dimos.msgs.sensor_msgs.JointState.JointState", "lcm", Some(header), joints.encode())
            }
            Msg::Joy(header, axes, buttons) => {
                let joy = sensor::Joy { header: lcm_header(&header), axes, buttons };
                ("dimos.msgs.sensor_msgs.Joy.Joy", "lcm", Some(header), joy.encode())
            }
            Msg::Twist(linear, angular) => {
                let twist = geo::Twist { linear: vector3(linear), angular: vector3(angular) };
                ("dimos.msgs.geometry_msgs.Twist.Twist", "lcm", None, twist.encode())
            }
            Msg::Text(text) => ("dimos.msgs.std_msgs.String.String", "lcm", None, std_msgs::String { data: text }.encode()),
        };
        Rows { payload_type, codec, rows: vec![(stamp.and_then(|h| seconds(&h)), data)] }
    }
}

fn seconds(header: &Header) -> Option<f64> {
    (header.stamp_ns > 0).then(|| header.stamp_ns as f64 / 1e9)
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

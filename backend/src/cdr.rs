// ROS2 CDR messages and their ros2msg schemas, what a recording stores (record.rs): the same encoding and schema names
// Controller's recorder writes (sensor_msgs/msg/Image, …), so Foxglove, dtk, Recordings and Map Editor read both alike.

/// A message ready for the file: its schema (name + ros2msg text) and its CDR bytes.
pub struct Encoded {
    pub schema_name: &'static str,
    pub schema_text: String,
    pub data: Vec<u8>,
}

const RULE: &str = "================================================================================\n";

fn dep(name: &str, body: &str) -> String {
    format!("{RULE}MSG: {name}\n{body}")
}

fn time_msg() -> String {
    dep("builtin_interfaces/Time", "int32 sec\nuint32 nanosec\n")
}
fn header_msg() -> String {
    dep("std_msgs/Header", "builtin_interfaces/Time stamp\nstring frame_id\n") + &time_msg()
}
fn vector3_msg() -> String {
    dep("geometry_msgs/Vector3", "float64 x\nfloat64 y\nfloat64 z\n")
}
fn point_msg() -> String {
    dep("geometry_msgs/Point", "float64 x\nfloat64 y\nfloat64 z\n")
}
fn quaternion_msg() -> String {
    dep("geometry_msgs/Quaternion", "float64 x\nfloat64 y\nfloat64 z\nfloat64 w\n")
}

/// A ROS2 header: a time (ns since the epoch) and a frame.
pub struct Header<'a> {
    pub stamp_ns: u64,
    pub frame_id: &'a str,
}

pub struct CdrWriter {
    buffer: Vec<u8>,
}

impl CdrWriter {
    pub fn new() -> Self {
        // encapsulation: little-endian CDR, no options
        CdrWriter { buffer: vec![0x00, 0x01, 0x00, 0x00] }
    }
    /// alignment counts from the body, after the 4 encapsulation bytes
    fn align(&mut self, width: usize) {
        let body = self.buffer.len() - 4;
        let padding = (width - (body % width)) % width;
        self.buffer.resize(self.buffer.len() + padding, 0);
    }
    pub fn u8(&mut self, value: u8) {
        self.buffer.push(value);
    }
    pub fn u32(&mut self, value: u32) {
        self.align(4);
        self.buffer.extend_from_slice(&value.to_le_bytes());
    }
    pub fn i32(&mut self, value: i32) {
        self.align(4);
        self.buffer.extend_from_slice(&value.to_le_bytes());
    }
    pub fn f32(&mut self, value: f32) {
        self.align(4);
        self.buffer.extend_from_slice(&value.to_le_bytes());
    }
    pub fn f64(&mut self, value: f64) {
        self.align(8);
        self.buffer.extend_from_slice(&value.to_le_bytes());
    }
    pub fn f64s(&mut self, values: &[f64]) {
        for value in values {
            self.f64(*value);
        }
    }
    pub fn string(&mut self, value: &str) {
        self.u32(value.len() as u32 + 1);
        self.buffer.extend_from_slice(value.as_bytes());
        self.buffer.push(0);
    }
    pub fn bytes(&mut self, value: &[u8]) {
        self.u32(value.len() as u32);
        self.buffer.extend_from_slice(value);
    }
    pub fn header(&mut self, header: &Header) {
        self.i32((header.stamp_ns / 1_000_000_000) as i32);
        self.u32((header.stamp_ns % 1_000_000_000) as u32);
        self.string(header.frame_id);
    }
    pub fn finish(self) -> Vec<u8> {
        self.buffer
    }
}

impl Default for CdrWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// sensor_msgs/Joy: the controller's raw axes (-1..1) and buttons (0/1), never velocities.
pub fn joy(header: &Header, axes: &[f32], buttons: &[i32]) -> Encoded {
    let mut w = CdrWriter::new();
    w.header(header);
    w.u32(axes.len() as u32);
    for axis in axes {
        w.f32(*axis);
    }
    w.u32(buttons.len() as u32);
    for button in buttons {
        w.i32(*button);
    }
    Encoded {
        schema_name: "sensor_msgs/msg/Joy",
        schema_text: format!("std_msgs/Header header\nfloat32[] axes\nint32[] buttons\n{}", header_msg()),
        data: w.finish(),
    }
}

/// geometry_msgs/Twist: m/s and rad/s.
pub fn twist(linear: [f64; 3], angular: [f64; 3]) -> Encoded {
    let mut w = CdrWriter::new();
    w.f64s(&linear);
    w.f64s(&angular);
    Encoded {
        schema_name: "geometry_msgs/msg/Twist",
        schema_text: format!("geometry_msgs/Vector3 linear\ngeometry_msgs/Vector3 angular\n{}", vector3_msg()),
        data: w.finish(),
    }
}

pub fn image(header: &Header, width: u32, height: u32, rgb: &[u8]) -> Encoded {
    let mut w = CdrWriter::new();
    w.header(header);
    w.u32(height);
    w.u32(width);
    w.string("rgb8");
    w.buffer.push(0);
    w.u32(width * 3);
    w.bytes(rgb);
    Encoded {
        schema_name: "sensor_msgs/msg/Image",
        schema_text: format!(
            "std_msgs/Header header\nuint32 height\nuint32 width\nstring encoding\nuint8 is_bigendian\nuint32 step\nuint8[] data\n{}",
            header_msg()
        ),
        data: w.finish(),
    }
}

/// sensor_msgs/CompressedImage (`format` e.g. "jpeg").
pub fn compressed_image(header: &Header, format: &str, data: &[u8]) -> Encoded {
    let mut w = CdrWriter::new();
    w.header(header);
    w.string(format);
    w.bytes(data);
    Encoded {
        schema_name: "sensor_msgs/msg/CompressedImage",
        schema_text: format!("std_msgs/Header header\nstring format\nuint8[] data\n{}", header_msg()),
        data: w.finish(),
    }
}

pub struct CameraInfo<'a> {
    pub width: u32,
    pub height: u32,
    pub distortion_model: &'a str,
    pub d: &'a [f64],
    pub k: [f64; 9],
    pub r: [f64; 9],
    pub p: [f64; 12],
}

/// sensor_msgs/CameraInfo
pub fn camera_info(header: &Header, info: &CameraInfo) -> Encoded {
    let mut w = CdrWriter::new();
    w.header(header);
    w.u32(info.height);
    w.u32(info.width);
    w.string(info.distortion_model);
    w.u32(info.d.len() as u32);
    w.f64s(info.d);
    w.f64s(&info.k);
    w.f64s(&info.r);
    w.f64s(&info.p);
    w.u32(0); // binning_x
    w.u32(0); // binning_y
    for _ in 0..4 {
        w.u32(0); // roi: x_offset, y_offset, height, width
    }
    w.u8(0); // roi.do_rectify
    Encoded {
        schema_name: "sensor_msgs/msg/CameraInfo",
        schema_text: format!(
            "std_msgs/Header header\nuint32 height\nuint32 width\nstring distortion_model\nfloat64[] d\nfloat64[9] k\n\
             float64[9] r\nfloat64[12] p\nuint32 binning_x\nuint32 binning_y\nsensor_msgs/RegionOfInterest roi\n{}{}",
            header_msg(),
            dep("sensor_msgs/RegionOfInterest", "uint32 x_offset\nuint32 y_offset\nuint32 height\nuint32 width\nbool do_rectify\n")
        ),
        data: w.finish(),
    }
}

/// sensor_msgs/PointCloud2 of float32 x, y, z points.
pub fn point_cloud_xyz(header: &Header, points: &[[f32; 3]]) -> Encoded {
    let mut w = CdrWriter::new();
    w.header(header);
    w.u32(1); // height
    w.u32(points.len() as u32); // width
    w.u32(3);
    for (i, name) in ["x", "y", "z"].iter().enumerate() {
        w.string(name);
        w.u32(i as u32 * 4);
        w.u8(7); // FLOAT32
        w.u32(1);
    }
    w.u8(0); // is_bigendian
    w.u32(12); // point_step
    w.u32(12 * points.len() as u32); // row_step
    w.u32(12 * points.len() as u32);
    for point in points {
        for value in point {
            w.buffer.extend_from_slice(&value.to_le_bytes());
        }
    }
    w.u8(1); // is_dense
    Encoded {
        schema_name: "sensor_msgs/msg/PointCloud2",
        schema_text: format!(
            "std_msgs/Header header\nuint32 height\nuint32 width\nsensor_msgs/PointField[] fields\nbool is_bigendian\n\
             uint32 point_step\nuint32 row_step\nuint8[] data\nbool is_dense\n{}{}",
            dep("sensor_msgs/PointField", "string name\nuint32 offset\nuint8 datatype\nuint32 count\n"),
            header_msg()
        ),
        data: w.finish(),
    }
}

/// geometry_msgs/PoseStamped; `orientation` is x, y, z, w.
pub fn pose_stamped(header: &Header, position: [f64; 3], orientation: [f64; 4]) -> Encoded {
    let mut w = CdrWriter::new();
    w.header(header);
    w.f64s(&position);
    w.f64s(&orientation);
    Encoded {
        schema_name: "geometry_msgs/msg/PoseStamped",
        schema_text: format!(
            "std_msgs/Header header\ngeometry_msgs/Pose pose\n{}{}{}{}",
            dep("geometry_msgs/Pose", "geometry_msgs/Point position\ngeometry_msgs/Quaternion orientation\n"),
            point_msg(),
            quaternion_msg(),
            header_msg()
        ),
        data: w.finish(),
    }
}

pub struct Transform<'a> {
    pub parent: &'a str,
    pub child: &'a str,
    pub translation: [f64; 3],
    /// x, y, z, w
    pub rotation: [f64; 4],
}

/// tf2_msgs/TFMessage
pub fn tf(stamp_ns: u64, transforms: &[Transform]) -> Encoded {
    let mut w = CdrWriter::new();
    w.u32(transforms.len() as u32);
    for t in transforms {
        w.header(&Header { stamp_ns, frame_id: t.parent });
        w.string(t.child);
        w.f64s(&t.translation);
        w.f64s(&t.rotation);
    }
    Encoded {
        schema_name: "tf2_msgs/msg/TFMessage",
        schema_text: format!(
            "geometry_msgs/TransformStamped[] transforms\n{}{}{}{}{}",
            dep("geometry_msgs/TransformStamped", "std_msgs/Header header\nstring child_frame_id\ngeometry_msgs/Transform transform\n"),
            dep("geometry_msgs/Transform", "geometry_msgs/Vector3 translation\ngeometry_msgs/Quaternion rotation\n"),
            vector3_msg(),
            quaternion_msg(),
            header_msg()
        ),
        data: w.finish(),
    }
}

/// sensor_msgs/Imu; `orientation` is x, y, z, w. A covariance starting with -1 means "not measured".
pub fn imu(header: &Header, orientation: [f64; 4], angular_velocity: Option<[f64; 3]>, linear_acceleration: Option<[f64; 3]>) -> Encoded {
    let mut w = CdrWriter::new();
    w.header(header);
    w.f64s(&orientation);
    w.f64s(&[0.0; 9]);
    let unknown = {
        let mut c = [0.0; 9];
        c[0] = -1.0;
        c
    };
    w.f64s(&angular_velocity.unwrap_or_default());
    w.f64s(if angular_velocity.is_some() { &[0.0; 9] } else { &unknown });
    w.f64s(&linear_acceleration.unwrap_or_default());
    w.f64s(if linear_acceleration.is_some() { &[0.0; 9] } else { &unknown });
    Encoded {
        schema_name: "sensor_msgs/msg/Imu",
        schema_text: format!(
            "std_msgs/Header header\ngeometry_msgs/Quaternion orientation\nfloat64[9] orientation_covariance\n\
             geometry_msgs/Vector3 angular_velocity\nfloat64[9] angular_velocity_covariance\n\
             geometry_msgs/Vector3 linear_acceleration\nfloat64[9] linear_acceleration_covariance\n{}{}{}",
            quaternion_msg(),
            vector3_msg(),
            header_msg()
        ),
        data: w.finish(),
    }
}

pub struct Battery {
    pub voltage: f32,
    pub current: f32,
    pub temperature: f32,
    /// 0..1
    pub percentage: f32,
    pub cell_temperatures: Vec<f32>,
}

/// sensor_msgs/BatteryState (what Stash's cockpit branch publishes as `battery`).
pub fn battery_state(header: &Header, battery: &Battery) -> Encoded {
    let mut w = CdrWriter::new();
    w.header(header);
    w.f32(battery.voltage);
    w.f32(battery.temperature);
    w.f32(battery.current);
    w.f32(f32::NAN); // charge
    w.f32(f32::NAN); // capacity
    w.f32(f32::NAN); // design_capacity
    w.f32(battery.percentage);
    w.u8(if battery.current > 0.05 {
        1
    } else if battery.current < -0.05 {
        2
    } else {
        3
    }); // status: charging, discharging, not charging
    w.u8(0); // power_supply_health: unknown
    w.u8(2); // power_supply_technology: LION
    w.u8(1); // present
    w.u32(0); // cell_voltage[]
    w.u32(battery.cell_temperatures.len() as u32);
    for t in &battery.cell_temperatures {
        w.f32(*t);
    }
    w.string(""); // location
    w.string(""); // serial_number
    Encoded {
        schema_name: "sensor_msgs/msg/BatteryState",
        schema_text: format!(
            "std_msgs/Header header\nfloat32 voltage\nfloat32 temperature\nfloat32 current\nfloat32 charge\nfloat32 capacity\n\
             float32 design_capacity\nfloat32 percentage\nuint8 power_supply_status\nuint8 power_supply_health\n\
             uint8 power_supply_technology\nbool present\nfloat32[] cell_voltage\nfloat32[] cell_temperature\n\
             string location\nstring serial_number\n{}",
            header_msg()
        ),
        data: w.finish(),
    }
}

/// sensor_msgs/JointState (positions only).
pub fn joint_state(header: &Header, names: &[&str], positions: &[f64]) -> Encoded {
    let mut w = CdrWriter::new();
    w.header(header);
    w.u32(names.len() as u32);
    for name in names {
        w.string(name);
    }
    w.u32(positions.len() as u32);
    w.f64s(positions);
    w.u32(0); // velocity[]
    w.u32(0); // effort[]
    Encoded {
        schema_name: "sensor_msgs/msg/JointState",
        schema_text: format!(
            "std_msgs/Header header\nstring[] name\nfloat64[] position\nfloat64[] velocity\nfloat64[] effort\n{}",
            header_msg()
        ),
        data: w.finish(),
    }
}

/// std_msgs/String (the commands channel carries one JSON object per message).
pub fn string(text: &str) -> Encoded {
    let mut w = CdrWriter::new();
    w.string(text);
    Encoded { schema_name: "std_msgs/msg/String", schema_text: "string data\n".into(), data: w.finish() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twist_is_six_aligned_doubles() {
        let encoded = twist([0.25, 0.0, 0.0], [0.0, 0.0, -0.5]);
        assert_eq!(encoded.data.len(), 4 + 48);
        assert_eq!(&encoded.data[4..12], &0.25f64.to_le_bytes());
        assert_eq!(&encoded.data[44..52], &(-0.5f64).to_le_bytes());
    }

    #[test]
    fn joy_layout() {
        let encoded = joy(&Header { stamp_ns: 1_500_000_000, frame_id: "" }, &[0.5, -1.0], &[1, 0, 1]);
        let d = &encoded.data[4..];
        assert_eq!(&d[0..4], &1i32.to_le_bytes());
        assert_eq!(&d[4..8], &500_000_000u32.to_le_bytes());
        assert_eq!(&d[8..12], &1u32.to_le_bytes()); // "" + NUL
                                                    // after the NUL at 12 the axes count aligns to 16
        assert_eq!(&d[16..20], &2u32.to_le_bytes());
        assert_eq!(&d[20..24], &0.5f32.to_le_bytes());
        assert_eq!(&d[28..32], &3u32.to_le_bytes());
    }
}

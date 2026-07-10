// BLE wifi-provisioning packet protocol for Unitree robots.
//
// Faithful port of dimos' `dimos/robot/unitree/go2/cli/ble.py`. The protocol was
// reverse-engineered by the UniPwn project; this keeps only the legitimate
// provisioning flow used by the official Unitree app (no exploit payload).

use aes::Aes128;
use cfb_mode::cipher::{AsyncStreamCipher, KeyIvInit};

pub const UNITREE_NAME_PREFIXES: [&str; 5] = ["Go2_", "G1_", "B2_", "H1_", "X1_"];

#[allow(dead_code)]
pub const UNITREE_SERVICE_UUID: &str = "0000ffe0-0000-1000-8000-00805f9b34fb";
pub const NOTIFY_CHAR_UUID: &str = "0000ffe1-0000-1000-8000-00805f9b34fb";
pub const WRITE_CHAR_UUID: &str = "0000ffe2-0000-1000-8000-00805f9b34fb";

// Symmetric AES-CFB-128 key/iv burned into the firmware.
const AES_KEY: [u8; 16] = [
    0xdf, 0x98, 0xb7, 0x15, 0xd5, 0xc6, 0xed, 0x2b, 0x25, 0x81, 0x7b, 0x6f, 0x25, 0x54, 0x12, 0x4a,
];
const AES_IV: [u8; 16] = [
    0x28, 0x41, 0xae, 0x97, 0x41, 0x9c, 0x29, 0x73, 0x29, 0x6a, 0x0d, 0x4b, 0xdf, 0xe1, 0x9a, 0x4f,
];

pub const CHUNK_SIZE: usize = 14;
pub const HANDSHAKE_CONTENT: &[u8] = b"unitree";

// Instruction opcodes (TX opcode byte 0x52, RX 0x51).
pub const INST_HANDSHAKE: u8 = 1;
pub const INST_SERIAL: u8 = 2;
pub const INST_INIT_STA: u8 = 3;
pub const INST_SSID: u8 = 4;
pub const INST_PASSWORD: u8 = 5;
pub const INST_COUNTRY: u8 = 6;

type Aes128CfbEnc = cfb_mode::Encryptor<Aes128>;
type Aes128CfbDec = cfb_mode::Decryptor<Aes128>;

pub fn encrypt(data: &[u8]) -> Vec<u8> {
    let mut buf = data.to_vec();
    Aes128CfbEnc::new(&AES_KEY.into(), &AES_IV.into()).encrypt(&mut buf);
    buf
}

pub fn decrypt(data: &[u8]) -> Vec<u8> {
    let mut buf = data.to_vec();
    Aes128CfbDec::new(&AES_KEY.into(), &AES_IV.into()).decrypt(&mut buf);
    buf
}

fn checksum(bytes: &[u8]) -> u8 {
    let sum: u32 = bytes.iter().map(|&b| b as u32).sum();
    (sum.wrapping_neg() & 0xFF) as u8
}

/// Build an encrypted TX packet: 0x52, length, instruction, *payload, checksum.
pub fn build_packet(instruction: u8, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(payload.len() + 4);
    body.push(0x52);
    body.push((payload.len() + 4) as u8);
    body.push(instruction);
    body.extend_from_slice(payload);
    let check = checksum(&body);
    body.push(check);
    encrypt(&body)
}

/// Validate a decrypted RX packet against the expected instruction.
pub fn validate_response(response: &[u8], expected_inst: u8) -> bool {
    if response.len() < 5 {
        return false;
    }
    if response[0] != 0x51 {
        return false;
    }
    if response.len() != response[1] as usize {
        return false;
    }
    if response[2] != expected_inst {
        return false;
    }
    if checksum(&response[..response.len() - 1]) != response[response.len() - 1] {
        return false;
    }
    response[3] == 0x01
}

/// Recover the 16-char serial from BLE manufacturer data.
///
/// The Go2 packs its serial across the company-ID + payload field: the
/// company-ID's little-endian bytes spell the first two ASCII chars, the
/// payload spells the rest.
pub fn serial_from_manufacturer(company_id: u16, payload: &[u8]) -> Option<String> {
    let mut bytes = vec![(company_id & 0xFF) as u8, (company_id >> 8) as u8];
    bytes.extend_from_slice(payload);
    if bytes.iter().all(|&b| b.is_ascii()) {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cross-checked against the python reference implementation.
    #[test]
    fn packet_roundtrip_validates() {
        // A handshake TX packet, decrypted, is NOT a valid RX (opcode 0x52 vs 0x51),
        // but encrypt/decrypt must be inverse and checksum stable.
        let plain = [0x51u8, 0x05, INST_HANDSHAKE, 0x01, 0x00];
        let mut p = plain.to_vec();
        p[4] = checksum(&plain[..4]);
        assert!(validate_response(&p, INST_HANDSHAKE));
        let enc = encrypt(&p);
        assert_eq!(decrypt(&enc), p);
    }

    #[test]
    fn serial_decoding() {
        // company_id 0x3130 ("01" little-endian) + b"2345..." -> "0123..."
        let cid = u16::from_le_bytes([b'0', b'1']);
        assert_eq!(
            serial_from_manufacturer(cid, b"2345"),
            Some("012345".to_string())
        );
    }
}

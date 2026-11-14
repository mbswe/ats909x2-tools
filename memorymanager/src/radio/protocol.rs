//! The line protocol spoken by the ESP32 bridge firmware.
//!
//! The firmware documents the protocol in its `protocol.h`:
//! ASCII lines at 115200 baud, 256-byte blocks as hex, CRC-32 (zlib) checks.
//! This module only builds and parses lines; [`super::link`] does the I/O.

use std::collections::HashMap;

use crate::image::IMAGE_LEN;

/// Protocol version the app requires from the bridge.
pub const PROTOCOL_VERSION: u32 = 1;
/// Bytes per transfer block.
pub const BLOCK_LEN: usize = 256;
/// Blocks per image (47).
pub const BLOCK_COUNT: usize = IMAGE_LEN / BLOCK_LEN;
/// 16-byte packets the radio sends during DATA OUT (752).
pub const PACKET_COUNT: u32 = (IMAGE_LEN / 16) as u32;
/// Bytes the radio reads during DATA IN: two full passes.
pub const IN_TOTAL_BYTES: u32 = 2 * IMAGE_LEN as u32;

/// CRC-32 as used by zlib; `b"123456789"` gives `0xCBF43926`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

pub fn hex_encode(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut text = String::with_capacity(data.len() * 2);
    for &byte in data {
        text.push(DIGITS[usize::from(byte >> 4)] as char);
        text.push(DIGITS[usize::from(byte & 0x0F)] as char);
    }
    text
}

pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some((high * 16 + low) as u8)
        })
        .collect()
}

/// Direction of a transfer, named after the radio's menu functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// The radio's DATA OUT: radio → bridge → PC.
    Out,
    /// The radio's DATA IN: PC → bridge → radio.
    In,
}

impl Direction {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "out" => Some(Direction::Out),
            "in" => Some(Direction::In),
            _ => None,
        }
    }
}

/// Counters reported by the bridge during and after a transfer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// DATA OUT: distinct 16-byte packets received (of 752).
    pub packets: u32,
    /// DATA OUT: 256-byte blocks the radio verified (of 47).
    pub verify: u32,
    /// DATA IN: bytes the radio has read (two passes = 24,064).
    pub bytes: u32,
    /// DATA IN: complete passes over the image.
    pub passes: u32,
    /// Bits that read back differently at the radio's SDA contact.
    pub mismatches: u32,
    /// DATA IN: requests to read past the end of the image.
    pub past_end: u32,
    /// DATA OUT: CRC-32 of the received image.
    pub crc: Option<u32>,
    /// Address bytes for `0x51` the bridge has decoded: the radio is talking.
    pub seen: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeEvent {
    Progress(Direction, Stats),
    Done(Direction, Stats),
    Fail(Direction, String, Stats),
}

/// One line from the bridge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    /// Printed after every reset.
    Ready { proto: u32, firmware: String, boot: u32 },
    /// `OK <COMMAND> <rest…>`
    Ok { command: String, rest: Vec<String> },
    /// `ERR <COMMAND> <reason>`
    Err { command: String, reason: String },
    Event(BridgeEvent),
    /// `# …` diagnostics.
    Diag(String),
    /// Anything else, such as ESP32 boot messages.
    Other(String),
}

fn fields<'a>(words: impl Iterator<Item = &'a str>) -> HashMap<&'a str, &'a str> {
    words.filter_map(|word| word.split_once('=')).collect()
}

fn number(fields: &HashMap<&str, &str>, key: &str) -> u32 {
    fields.get(key).and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn hex_u32(text: &str) -> Option<u32> {
    if text.len() == 8 { u32::from_str_radix(text, 16).ok() } else { None }
}

fn stats(fields: &HashMap<&str, &str>) -> Stats {
    Stats {
        packets: number(fields, "packets"),
        verify: number(fields, "verify"),
        bytes: number(fields, "bytes"),
        passes: number(fields, "passes"),
        mismatches: number(fields, "mism"),
        past_end: number(fields, "past_end"),
        crc: fields.get("crc").and_then(|v| hex_u32(v)),
        seen: number(fields, "seen"),
    }
}

pub fn parse_line(text: &str) -> Line {
    let text = text.trim_end_matches(['\r', '\n']);
    let mut words = text.split_whitespace();
    let other = || Line::Other(text.to_owned());
    match words.next() {
        Some("OK") => match words.next() {
            Some(command) => Line::Ok {
                command: command.to_owned(),
                rest: words.map(str::to_owned).collect(),
            },
            None => other(),
        },
        Some("ERR") => match (words.next(), words.next()) {
            (Some(command), reason) => Line::Err {
                command: command.to_owned(),
                reason: reason.unwrap_or("").to_owned(),
            },
            _ => other(),
        },
        Some("EV") => {
            let kind = words.next();
            let fields = fields(words);
            let Some(dir) = fields.get("dir").and_then(|d| Direction::parse(d)) else {
                return other();
            };
            match kind {
                Some("PROGRESS") => Line::Event(BridgeEvent::Progress(dir, stats(&fields))),
                Some("DONE") => Line::Event(BridgeEvent::Done(dir, stats(&fields))),
                Some("FAIL") => {
                    let reason = fields.get("reason").copied().unwrap_or("unknown").to_owned();
                    Line::Event(BridgeEvent::Fail(dir, reason, stats(&fields)))
                }
                _ => other(),
            }
        }
        Some("READY") if words.next() == Some("bridge") => {
            let fields = fields(words);
            Line::Ready {
                proto: number(&fields, "proto"),
                firmware: fields.get("fw").copied().unwrap_or("?").to_owned(),
                boot: fields.get("boot").and_then(|v| hex_u32(v)).unwrap_or(0),
            }
        }
        Some(word) if word.starts_with('#') => Line::Diag(text.to_owned()),
        _ => other(),
    }
}

/// The bridge's reply to `HELLO`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub proto: u32,
    pub firmware: String,
    pub image_len: usize,
    pub boot: u32,
}

/// Parses the words after `OK HELLO`.
pub fn parse_hello(rest: &[String]) -> Option<Hello> {
    if rest.first().map(String::as_str) != Some("bridge") {
        return None;
    }
    let fields = fields(rest.iter().map(String::as_str));
    Some(Hello {
        proto: fields.get("proto")?.parse().ok()?,
        firmware: fields.get("fw")?.to_string(),
        image_len: fields.get("len")?.parse().ok()?,
        boot: hex_u32(fields.get("boot")?)?,
    })
}

pub fn put_line(block: usize, data: &[u8]) -> String {
    format!("PUT {block} {} {:08X}", hex_encode(data), crc32(data))
}

pub fn commit_line(crc: u32) -> String {
    format!("COMMIT {crc:08X}")
}

pub fn arm_line(direction: Direction, crc: u32) -> String {
    match direction {
        Direction::Out => "ARM OUT".to_owned(),
        Direction::In => format!("ARM IN {crc:08X}"),
    }
}

/// Parses the words after `OK GET`: block number, data, CRC. `None` if
/// malformed or if the data does not match its CRC.
pub fn parse_get(rest: &[String], expected_block: usize) -> Option<Vec<u8>> {
    let [block, hex, crc] = rest else { return None };
    if block.parse::<usize>().ok()? != expected_block {
        return None;
    }
    let data = hex_decode(hex)?;
    (data.len() == BLOCK_LEN && crc32(&data) == hex_u32(crc)?).then_some(data)
}

/// Parses `window_ms` from the words after `OK ARM`.
pub fn parse_arm_window_ms(rest: &[String]) -> u32 {
    number(&fields(rest.iter().map(String::as_str)), "window_ms")
}

/// Explains a bridge failure reason in plain words.
pub fn describe_failure(direction: Direction, reason: &str) -> String {
    let started = match direction {
        Direction::Out => "DATA OUT (FM + LIGHT, then ENTER)",
        Direction::In => "DATA IN (LIGHT + LW, then ENTER)",
    };
    match reason {
        "no_radio" => format!("The radio did not start within the time limit. Start {started} after arming."),
        "incomplete" => "The radio stopped before the transfer was complete.".to_owned(),
        "conflict" => "The radio sent the same data twice with different contents. Try again.".to_owned(),
        "wrong_direction" => match direction {
            Direction::Out => "The radio started DATA IN while the bridge was waiting for DATA OUT. \
                               Use FM + LIGHT, then ENTER to read the radio."
                .to_owned(),
            Direction::In => "The radio started DATA OUT while the bridge was waiting for DATA IN. \
                              Use LIGHT + LW, then ENTER to write the radio."
                .to_owned(),
        },
        "stalled" => "The I²C bus stalled, so the bridge let go of it. Check the wiring and try again.".to_owned(),
        "aborted" => "The transfer was cancelled.".to_owned(),
        other => format!("The bridge reported an error: {other}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn crc_and_hex_match_the_firmware() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(&[]), 0);
        assert_eq!(hex_encode(&[0x00, 0xAB, 0xFF]), "00ABFF");
        assert_eq!(hex_decode("00abFF"), Some(vec![0x00, 0xAB, 0xFF]));
        assert_eq!(hex_decode("0"), None);
        assert_eq!(hex_decode("0G"), None);
        assert_eq!(BLOCK_COUNT, 47);
        assert_eq!(PACKET_COUNT, 752);
        assert_eq!(IN_TOTAL_BYTES, 24_064);
    }

    #[test]
    fn parses_every_line_kind() {
        assert_eq!(
            parse_line("READY bridge proto=1 fw=1.0.0 boot=12345678\r"),
            Line::Ready { proto: 1, firmware: "1.0.0".into(), boot: 0x1234_5678 }
        );
        assert_eq!(
            parse_line("OK PUT 3"),
            Line::Ok { command: "PUT".into(), rest: words("3") }
        );
        assert_eq!(
            parse_line("ERR ARM bus_busy"),
            Line::Err { command: "ARM".into(), reason: "bus_busy".into() }
        );
        assert_eq!(
            parse_line("EV PROGRESS dir=out packets=312 verify=0 seen=400"),
            Line::Event(BridgeEvent::Progress(
                Direction::Out,
                Stats { packets: 312, seen: 400, ..Stats::default() }
            ))
        );
        assert_eq!(
            parse_line("EV DONE dir=out packets=752 verify=47 mism=0 crc=CBF43926"),
            Line::Event(BridgeEvent::Done(
                Direction::Out,
                Stats { packets: 752, verify: 47, crc: Some(0xCBF4_3926), ..Stats::default() }
            ))
        );
        assert_eq!(
            parse_line("EV DONE dir=in passes=2 bytes=24064 mism=0 past_end=0"),
            Line::Event(BridgeEvent::Done(
                Direction::In,
                Stats { passes: 2, bytes: 24_064, ..Stats::default() }
            ))
        );
        assert!(matches!(
            parse_line("EV FAIL dir=in reason=no_radio packets=0 verify=0 bytes=0 passes=0 mism=0"),
            Line::Event(BridgeEvent::Fail(Direction::In, reason, _)) if reason == "no_radio"
        ));
        assert_eq!(parse_line("# state=idle"), Line::Diag("# state=idle".into()));
        // ESP32 boot ROM noise and malformed lines are tolerated.
        for noise in ["ets Jun  8 2016 00:22:57", "rst:0x1 (POWERON_RESET)", "", "OK", "EV DONE", "READY"] {
            assert!(matches!(parse_line(noise), Line::Other(_)), "{noise:?}");
        }
    }

    #[test]
    fn builds_and_parses_commands() {
        let block = vec![0x5A; BLOCK_LEN];
        let put = put_line(4, &block);
        assert!(put.starts_with("PUT 4 5A5A"));
        assert!(put.ends_with(&format!(" {:08X}", crc32(&block))));
        assert_eq!(commit_line(0xAB), "COMMIT 000000AB");
        assert_eq!(arm_line(Direction::Out, 7), "ARM OUT");
        assert_eq!(arm_line(Direction::In, 0xCBF4_3926), "ARM IN CBF43926");

        let reply = words(&format!("4 {} {:08X}", hex_encode(&block), crc32(&block)));
        assert_eq!(parse_get(&reply, 4), Some(block.clone()));
        assert_eq!(parse_get(&reply, 5), None);
        let corrupt = words(&format!("4 {} {:08X}", hex_encode(&block), crc32(&block) ^ 1));
        assert_eq!(parse_get(&corrupt, 4), None);

        let hello = parse_hello(&words("bridge proto=1 fw=1.0.0 len=12032 boot=0000ABCD")).unwrap();
        assert_eq!(hello, Hello { proto: 1, firmware: "1.0.0".into(), image_len: 12_032, boot: 0xABCD });
        assert_eq!(parse_hello(&words("other proto=1")), None);
        assert_eq!(parse_arm_window_ms(&words("dir=out window_ms=120000")), 120_000);
    }

    #[test]
    fn failures_are_explained() {
        assert!(describe_failure(Direction::Out, "wrong_direction").contains("FM + LIGHT"));
        assert!(describe_failure(Direction::In, "no_radio").contains("LIGHT + LW"));
        assert!(describe_failure(Direction::In, "xyz").contains("xyz"));
    }
}

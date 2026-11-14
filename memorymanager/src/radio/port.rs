//! Finding and opening the bridge's USB serial port.

use std::time::Duration;

use serialport::{SerialPort, SerialPortType};

use super::link::{Link, LinkError, Timeouts};
use super::log::Logger;
use super::worker::{Connection, Connector};

pub const BAUD_RATE: u32 = 115_200;

/// A USB serial port that may be the bridge, best match first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub label: String,
    rank: u8,
}

/// Ranks USB-serial chips found on ESP32 boards. This board uses a CP2102.
fn rank(vid: u16, pid: u16) -> u8 {
    match (vid, pid) {
        (0x10C4, 0xEA60) => 0, // Silicon Labs CP210x
        (0x1A86, _) => 1,      // WCH CH340/CH343
        (0x0403, _) => 2,      // FTDI
        (0x303A, _) => 3,      // Espressif native USB
        _ => 4,
    }
}

/// USB serial ports, best match first. On macOS only `/dev/cu.*` is used:
/// opening `/dev/tty.*` waits for carrier detect.
pub fn candidates() -> Vec<Candidate> {
    let mut found: Vec<Candidate> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .filter(|port| !(cfg!(target_os = "macos") && port.port_name.starts_with("/dev/tty.")))
        .filter_map(|port| match port.port_type {
            SerialPortType::UsbPort(usb) => Some(Candidate {
                label: usb.product.unwrap_or_else(|| format!("USB {:04X}:{:04X}", usb.vid, usb.pid)),
                rank: rank(usb.vid, usb.pid),
                name: port.port_name,
            }),
            _ => None,
        })
        .collect();
    found.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.name.cmp(&b.name)));
    found
}

/// Opens a port with DTR and RTS released, so the ESP32's auto-reset circuit
/// lets the board run. Opening may still reset it once; the handshake waits.
pub fn open(name: &str) -> Result<Box<dyn SerialPort>, LinkError> {
    let mut port = serialport::new(name, BAUD_RATE)
        .timeout(Duration::from_millis(50))
        .dtr_on_open(false)
        .open()
        .map_err(|err| {
            let busy = err.to_string().to_lowercase().contains("busy");
            let hint = if busy { " (close the Arduino Serial Monitor or other programs using it)" } else { "" };
            LinkError::NotFound(format!("{name}: {err}{hint}"))
        })?;
    let io = |err: serialport::Error| LinkError::Io(err.into());
    port.write_request_to_send(false).map_err(io)?;
    port.write_data_terminal_ready(false).map_err(io)?;
    Ok(port)
}

/// Connects to the bridge on a given port, or tries each candidate in turn.
pub struct SerialConnector {
    pub port: Option<String>,
    pub timeouts: Timeouts,
    pub log: Logger,
}

impl SerialConnector {
    pub fn new(port: Option<String>, log: Logger) -> Self {
        Self { port, timeouts: Timeouts::default(), log }
    }

    fn try_port(&self, name: &str) -> Result<Connection<Box<dyn SerialPort>>, LinkError> {
        self.log.note(&format!("Opening {name}"));
        let mut link = Link::new(open(name)?, self.timeouts);
        link.set_logger(self.log.clone());
        let hello = link.handshake().map_err(|err| match err {
            LinkError::Timeout(_) => {
                LinkError::NotFound(format!("{name}: no answer (is the bridge firmware flashed?)"))
            }
            other => other,
        })?;
        Ok(Connection { link, hello, port_name: name.to_owned() })
    }
}

impl Connector for SerialConnector {
    type Port = Box<dyn SerialPort>;

    fn connect(&mut self) -> Result<Connection<Self::Port>, LinkError> {
        if let Some(name) = self.port.clone() {
            return self.try_port(&name);
        }
        let candidates = candidates();
        if candidates.is_empty() {
            return Err(LinkError::NotFound(
                "no USB serial port; connect the ESP32 by USB".to_owned(),
            ));
        }
        let mut problems = Vec::new();
        for candidate in &candidates {
            match self.try_port(&candidate.name) {
                Ok(connection) => return Ok(connection),
                // A bridge with the wrong firmware is the answer, not a reason to keep looking.
                Err(err @ LinkError::Version { .. }) => return Err(err),
                Err(err) => {
                    self.log.note(&format!("{}: {err}", candidate.name));
                    problems.push(err.to_string());
                }
            }
        }
        Err(LinkError::NotFound(problems.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_esp32_usb_chips_rank_first() {
        assert_eq!(rank(0x10C4, 0xEA60), 0);
        assert!(rank(0x1A86, 0x7523) < rank(0x1234, 0x5678));
        assert!(rank(0x0403, 0x6001) < rank(0x05AC, 0x8290));
    }
}

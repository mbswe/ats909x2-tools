//! Request/response handling over any byte transport (a serial port in the
//! app, an in-memory fake in tests).

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use super::log::Logger;
use super::protocol::{
    self, BLOCK_COUNT, BLOCK_LEN, BridgeEvent, Direction, Hello, Line, PROTOCOL_VERSION,
};
use crate::image::IMAGE_LEN;

#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Waiting for the reply to one command.
    pub reply: Duration,
    /// Waiting for `OK HELLO` after each attempt; covers a reset on port open.
    pub hello: Duration,
    pub hello_attempts: u32,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            reply: Duration::from_secs(2),
            hello: Duration::from_millis(900),
            hello_attempts: 4,
        }
    }
}

#[derive(Debug)]
pub enum LinkError {
    Io(io::Error),
    /// No reply in time; names the command.
    Timeout(String),
    /// The bridge speaks another protocol version or image size.
    Version { proto: u32, image_len: usize },
    /// The bridge answered `ERR <command> <reason>`.
    Refused { command: String, reason: String },
    /// The bridge printed `READY` again: it was reset and lost its state.
    Rebooted,
    /// Data failed its CRC check twice.
    Corrupt(String),
    /// No serial port answered as a bridge.
    NotFound(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::Io(err) => write!(f, "serial port error: {err}"),
            LinkError::Timeout(command) => write!(f, "the bridge did not answer {command}"),
            LinkError::Version { proto, image_len } => write!(
                f,
                "the ESP32 runs bridge protocol {proto} for {image_len}-byte images; \
                 flash the bridge firmware that goes with this editor (protocol {PROTOCOL_VERSION})"
            ),
            LinkError::Refused { command, reason } => match (command.as_str(), reason.as_str()) {
                ("ARM", "bus_busy") => write!(
                    f,
                    "the I²C bus is busy (SCL or SDA is LOW); make sure the ESP32 is connected to the \
                     radio's contacts and the radio is on and idle, not in a menu"
                ),
                ("ARM", "drive_low") => write!(
                    f,
                    "GPIO21 reads LOW although the bridge is not pulling it; make sure the ESP32 is \
                     connected to the radio and check the GPIO21 → 1 kΩ → SDA wire"
                ),
                _ => write!(f, "the bridge refused {command}: {reason}"),
            },
            LinkError::Rebooted => write!(f, "the ESP32 was reset during the transfer"),
            LinkError::Corrupt(what) => write!(f, "{what} failed its checksum twice"),
            LinkError::NotFound(details) => write!(f, "no bridge found: {details}"),
        }
    }
}

impl std::error::Error for LinkError {}

impl From<io::Error> for LinkError {
    fn from(err: io::Error) -> Self {
        LinkError::Io(err)
    }
}

pub struct Link<T> {
    port: T,
    buffer: Vec<u8>,
    pending: VecDeque<BridgeEvent>,
    timeouts: Timeouts,
    log: Logger,
}

impl<T: Read + Write> Link<T> {
    pub fn new(port: T, timeouts: Timeouts) -> Self {
        Self { port, buffer: Vec::new(), pending: VecDeque::new(), timeouts, log: Logger::disabled() }
    }

    /// Records every line sent and received from now on.
    pub fn set_logger(&mut self, log: Logger) {
        self.log = log;
    }

    /// The bridge's diagnostic report: pin levels and transfer counters.
    pub fn diag(&mut self) -> Result<Vec<String>, LinkError> {
        self.report("DIAG", self.timeouts.reply)
    }

    /// The bridge's record of its last 4096 bus interrupts (about 160 KB of
    /// text, some 14 seconds at 115200 baud).
    pub fn trace(&mut self) -> Result<Vec<String>, LinkError> {
        self.report("TRACE", self.timeouts.reply * 15)
    }

    /// Sends `command` and collects its `# …` lines until `OK <command>`.
    fn report(&mut self, command: &str, wait: Duration) -> Result<Vec<String>, LinkError> {
        self.send(command)?;
        let deadline = Instant::now() + wait;
        let mut lines = Vec::new();
        while let Some(text) = self.read_line(deadline)? {
            match protocol::parse_line(&text) {
                Line::Diag(text) => lines.push(text),
                Line::Ok { command: c, .. } if c == command => return Ok(lines),
                Line::Err { command: c, reason } if c == command => {
                    return Err(LinkError::Refused { command: c, reason });
                }
                Line::Event(event) => self.pending.push_back(event),
                Line::Ready { .. } => return Err(LinkError::Rebooted),
                _ => {}
            }
        }
        Err(LinkError::Timeout(command.to_owned()))
    }

    /// Says hello until the bridge answers, skipping boot noise. Opening a
    /// serial port can reset the ESP32; a `READY` line triggers an immediate retry.
    pub fn handshake(&mut self) -> Result<Hello, LinkError> {
        for _ in 0..self.timeouts.hello_attempts {
            self.send("HELLO")?;
            let deadline = Instant::now() + self.timeouts.hello;
            while let Some(text) = self.read_line(deadline)? {
                match protocol::parse_line(&text) {
                    Line::Ok { command, rest } if command == "HELLO" => {
                        let hello = protocol::parse_hello(&rest)
                            .ok_or_else(|| LinkError::Refused { command, reason: text.clone() })?;
                        if hello.proto != PROTOCOL_VERSION || hello.image_len != IMAGE_LEN {
                            return Err(LinkError::Version { proto: hello.proto, image_len: hello.image_len });
                        }
                        self.pending.clear();
                        return Ok(hello);
                    }
                    // Freshly booted: anything sent during boot was lost.
                    Line::Ready { .. } => break,
                    _ => {}
                }
            }
        }
        Err(LinkError::Timeout("HELLO".into()))
    }

    /// Uploads an image block by block, then commits it. Returns its CRC.
    pub fn upload(&mut self, image: &[u8], mut progress: impl FnMut(usize)) -> Result<u32, LinkError> {
        assert_eq!(image.len(), IMAGE_LEN, "images are {IMAGE_LEN} bytes");
        for (index, block) in image.chunks(BLOCK_LEN).enumerate() {
            let line = protocol::put_line(index, block);
            match self.request(&line, "PUT") {
                Err(LinkError::Refused { reason, .. }) if reason == "crc" => {
                    self.request(&line, "PUT").map_err(|err| match err {
                        LinkError::Refused { reason, .. } if reason == "crc" => {
                            LinkError::Corrupt(format!("upload block {index}"))
                        }
                        other => other,
                    })?;
                }
                result => {
                    result?;
                }
            }
            progress(index + 1);
        }
        let crc = protocol::crc32(image);
        self.request(&protocol::commit_line(crc), "COMMIT")?;
        Ok(crc)
    }

    /// Arms the bridge. `crc` is the committed image's CRC for DATA IN.
    /// Returns how long the bridge waits for the radio to start.
    pub fn arm(&mut self, direction: Direction, crc: u32) -> Result<Duration, LinkError> {
        self.pending.clear();
        let rest = self.request(&protocol::arm_line(direction, crc), "ARM")?;
        Ok(Duration::from_millis(u64::from(protocol::parse_arm_window_ms(&rest))))
    }

    /// Waits up to `wait` for the next transfer event.
    pub fn next_event(&mut self, wait: Duration) -> Result<Option<BridgeEvent>, LinkError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        let deadline = Instant::now() + wait;
        while let Some(text) = self.read_line(deadline)? {
            match protocol::parse_line(&text) {
                Line::Event(event) => return Ok(Some(event)),
                Line::Ready { .. } => return Err(LinkError::Rebooted),
                _ => {}
            }
        }
        Ok(None)
    }

    /// Disarms the bridge.
    pub fn abort(&mut self) -> Result<(), LinkError> {
        self.request("ABORT", "ABORT")?;
        self.pending.clear();
        Ok(())
    }

    /// Downloads the bridge's image block by block, checking each CRC.
    pub fn download(&mut self, mut progress: impl FnMut(usize)) -> Result<Vec<u8>, LinkError> {
        let mut image = Vec::with_capacity(IMAGE_LEN);
        for index in 0..BLOCK_COUNT {
            let line = format!("GET {index}");
            let block = match protocol::parse_get(&self.request(&line, "GET")?, index) {
                Some(block) => block,
                None => protocol::parse_get(&self.request(&line, "GET")?, index)
                    .ok_or_else(|| LinkError::Corrupt(format!("download block {index}")))?,
            };
            image.extend_from_slice(&block);
            progress(index + 1);
        }
        Ok(image)
    }

    /// Sends one command and returns the words after `OK <command>`.
    fn request(&mut self, line: &str, command: &str) -> Result<Vec<String>, LinkError> {
        self.send(line)?;
        let deadline = Instant::now() + self.timeouts.reply;
        while let Some(text) = self.read_line(deadline)? {
            match protocol::parse_line(&text) {
                Line::Ok { command: c, rest } if c == command => return Ok(rest),
                Line::Err { command: c, reason } if c == command => {
                    return Err(LinkError::Refused { command: c, reason });
                }
                Line::Event(event) => self.pending.push_back(event),
                Line::Ready { .. } => return Err(LinkError::Rebooted),
                _ => {}
            }
        }
        Err(LinkError::Timeout(command.to_owned()))
    }

    fn send(&mut self, line: &str) -> Result<(), LinkError> {
        self.log.sent(line);
        self.port.write_all(line.as_bytes())?;
        self.port.write_all(b"\n")?;
        self.port.flush()?;
        Ok(())
    }

    /// Next complete line, or `None` once `deadline` passes.
    fn read_line(&mut self, deadline: Instant) -> Result<Option<String>, LinkError> {
        let mut chunk = [0u8; 256];
        loop {
            if let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line).trim_end().to_owned();
                if !line.is_empty() {
                    self.log.received(&line);
                }
                return Ok(Some(line));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match self.port.read(&mut chunk) {
                Ok(0) => std::thread::sleep(Duration::from_millis(1)),
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(err) => return Err(LinkError::Io(err)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fake::{FakeBridge, FakeConfig};
    use super::*;

    fn quick() -> Timeouts {
        Timeouts { reply: Duration::from_millis(200), hello: Duration::from_millis(100), hello_attempts: 3 }
    }

    fn image(seed: u8) -> Vec<u8> {
        (0..IMAGE_LEN).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn handshake_skips_boot_noise_and_checks_the_version() {
        let fake = FakeBridge::new(FakeConfig { boot_noise: true, ..FakeConfig::default() });
        let mut link = Link::new(fake.clone(), quick());
        let hello = link.handshake().unwrap();
        assert_eq!((hello.proto, hello.firmware.as_str(), hello.image_len), (1, "1.0.0", IMAGE_LEN));

        let old = FakeBridge::new(FakeConfig { proto: 9, ..FakeConfig::default() });
        let err = Link::new(old, quick()).handshake().unwrap_err();
        assert!(matches!(err, LinkError::Version { proto: 9, .. }));
        assert!(err.to_string().contains("flash the bridge firmware"));

        let silent = FakeBridge::new(FakeConfig { silent: true, ..FakeConfig::default() });
        assert!(matches!(Link::new(silent, quick()).handshake(), Err(LinkError::Timeout(_))));
    }

    #[test]
    fn upload_commits_and_retries_a_corrupted_block_once() {
        let fake = FakeBridge::new(FakeConfig { corrupt_puts: 1, ..FakeConfig::default() });
        let mut link = Link::new(fake.clone(), quick());
        link.handshake().unwrap();
        let data = image(3);
        let mut seen = 0;
        let crc = link.upload(&data, |n| seen = n).unwrap();
        assert_eq!(seen, BLOCK_COUNT);
        assert_eq!(crc, protocol::crc32(&data));
        assert_eq!(fake.staged(), Some(data));

        let failing = FakeBridge::new(FakeConfig { corrupt_puts: 2, ..FakeConfig::default() });
        let mut link = Link::new(failing, quick());
        link.handshake().unwrap();
        assert!(matches!(link.upload(&image(1), |_| {}), Err(LinkError::Corrupt(_))));
    }

    #[test]
    fn download_retries_a_corrupted_block_once() {
        let fake = FakeBridge::new(FakeConfig { corrupt_gets: 1, radio_image: image(9), ..FakeConfig::default() });
        let mut link = Link::new(fake.clone(), quick());
        link.handshake().unwrap();
        link.arm(Direction::Out, 0).unwrap();
        while !matches!(link.next_event(Duration::from_millis(50)).unwrap(), Some(BridgeEvent::Done(..))) {}
        assert_eq!(link.download(|_| {}).unwrap(), image(9));

        let failing = FakeBridge::new(FakeConfig { corrupt_gets: 2, ..FakeConfig::default() });
        let mut link = Link::new(failing, quick());
        link.handshake().unwrap();
        assert!(matches!(link.download(|_| {}), Err(LinkError::Corrupt(_))));
    }

    #[test]
    fn refusals_reboots_and_silence_are_reported() {
        let busy = FakeBridge::new(FakeConfig { bus_busy: true, ..FakeConfig::default() });
        let mut link = Link::new(busy, quick());
        link.handshake().unwrap();
        let err = link.arm(Direction::Out, 0).unwrap_err();
        assert!(matches!(&err, LinkError::Refused { reason, .. } if reason == "bus_busy"));
        assert!(err.to_string().contains("bus is busy"));
        let drive = LinkError::Refused { command: "ARM".into(), reason: "drive_low".into() };
        assert!(drive.to_string().contains("GPIO21 → 1 kΩ → SDA"));

        let rebooting = FakeBridge::new(FakeConfig { reboot_when_armed: true, ..FakeConfig::default() });
        let mut link = Link::new(rebooting, quick());
        link.handshake().unwrap();
        link.arm(Direction::Out, 0).unwrap();
        assert!(matches!(link.next_event(Duration::from_millis(100)), Err(LinkError::Rebooted)));

        let drive = LinkError::Refused { command: "ARM".into(), reason: "drive_low".into() };
        assert!(drive.to_string().contains("GPIO21"));

        let idle = FakeBridge::new(FakeConfig { radio_never_starts: true, ..FakeConfig::default() });
        let mut link = Link::new(idle, quick());
        link.handshake().unwrap();
        link.arm(Direction::Out, 0).unwrap();
        assert_eq!(link.next_event(Duration::from_millis(30)).unwrap(), None);
        link.abort().unwrap();
    }
}

//! An in-memory stand-in for the ESP32 bridge and the radio, for tests.
//!
//! It speaks the same line protocol as the bridge firmware and plays a
//! scripted radio: every read that finds no pending output advances the
//! radio by one step (progress events, then DONE or FAIL).

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use super::protocol::{self, BLOCK_COUNT, BLOCK_LEN, Direction};
use crate::image::IMAGE_LEN;

#[derive(Clone, Debug)]
pub struct FakeConfig {
    pub proto: u32,
    /// Boot messages and READY before the first HELLO is answered.
    pub boot_noise: bool,
    /// Never answers anything.
    pub silent: bool,
    /// Answer this many PUTs with `ERR PUT crc`.
    pub corrupt_puts: u32,
    /// Send this many GET replies with a wrong CRC.
    pub corrupt_gets: u32,
    /// What the radio sends during DATA OUT.
    pub radio_image: Vec<u8>,
    pub bus_busy: bool,
    pub reboot_when_armed: bool,
    pub radio_never_starts: bool,
    /// The radio's transfer ends with `EV FAIL reason=…`.
    pub radio_fails: Option<String>,
    /// Blocks the radio verified during DATA OUT before one bit read wrong (47 = all).
    pub verified_blocks: u32,
    pub progress_steps: u32,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            proto: 1,
            boot_noise: false,
            silent: false,
            corrupt_puts: 0,
            corrupt_gets: 0,
            radio_image: vec![0; IMAGE_LEN],
            bus_busy: false,
            reboot_when_armed: false,
            radio_never_starts: false,
            radio_fails: None,
            verified_blocks: BLOCK_COUNT as u32,
            progress_steps: 3,
        }
    }
}

#[derive(Default)]
struct State {
    config: FakeConfig,
    input: Vec<u8>,
    output: VecDeque<u8>,
    booted: bool,
    blocks: Vec<Option<Vec<u8>>>,
    staged: Option<(Vec<u8>, u32)>,
    image: Vec<u8>,
    armed: Option<Direction>,
    step: u32,
    received_by_radio: Option<Vec<u8>>,
    commands: Vec<String>,
}

#[derive(Clone)]
pub struct FakeBridge(Arc<Mutex<State>>);

impl FakeBridge {
    pub fn new(config: FakeConfig) -> Self {
        let booted = !config.boot_noise;
        FakeBridge(Arc::new(Mutex::new(State {
            config,
            booted,
            blocks: vec![None; BLOCK_COUNT],
            image: vec![0; IMAGE_LEN],
            ..State::default()
        })))
    }

    /// The image last committed by an upload.
    pub fn staged(&self) -> Option<Vec<u8>> {
        self.0.lock().unwrap().staged.as_ref().map(|(data, _)| data.clone())
    }

    /// The image the radio read during the last successful DATA IN.
    pub fn received_by_radio(&self) -> Option<Vec<u8>> {
        self.0.lock().unwrap().received_by_radio.clone()
    }

    pub fn commands(&self) -> Vec<String> {
        self.0.lock().unwrap().commands.clone()
    }
}

impl State {
    fn say(&mut self, line: &str) {
        self.output.extend(line.as_bytes());
        self.output.extend(b"\r\n");
    }

    fn handle(&mut self, line: &str) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(&command) = words.first() else { return };
        self.commands.push(command.to_owned());
        if self.config.silent {
            return;
        }
        if !self.booted {
            // The first command is lost while the ESP32 boots after the port opened.
            self.booted = true;
            self.say("ets Jun  8 2016 00:22:57");
            self.say("rst:0x1 (POWERON_RESET),boot:0x13 (SPI_FAST_FLASH_BOOT)");
            self.say("READY bridge proto=1 fw=1.0.0 boot=00000001");
            return;
        }
        match (command, &words[1..]) {
            ("HELLO", _) => {
                let proto = self.config.proto;
                self.say(&format!("OK HELLO bridge proto={proto} fw=1.0.0 len={IMAGE_LEN} boot=00000001"));
            }
            ("PUT", [block, hex, crc]) => {
                if self.armed.is_some() {
                    return self.say("ERR PUT armed");
                }
                if self.config.corrupt_puts > 0 {
                    self.config.corrupt_puts -= 1;
                    return self.say("ERR PUT crc");
                }
                let block: usize = block.parse().unwrap();
                let data = protocol::hex_decode(hex).unwrap();
                assert_eq!(format!("{:08X}", protocol::crc32(&data)), *crc);
                self.blocks[block] = Some(data);
                self.say(&format!("OK PUT {block}"));
            }
            ("COMMIT", [crc]) => {
                if self.blocks.iter().any(Option::is_none) {
                    return self.say("ERR COMMIT missing");
                }
                let data: Vec<u8> = self.blocks.iter().flatten().flatten().copied().collect();
                let actual = protocol::crc32(&data);
                if format!("{actual:08X}") != *crc {
                    return self.say("ERR COMMIT crc");
                }
                self.staged = Some((data, actual));
                self.say(&format!("OK COMMIT crc={actual:08X}"));
            }
            ("ARM", [direction, rest @ ..]) => {
                if self.config.bus_busy {
                    return self.say("ERR ARM bus_busy");
                }
                if *direction == "IN" {
                    let Some((data, crc)) = &self.staged else { return self.say("ERR ARM not_staged") };
                    if rest.first() != Some(&format!("{crc:08X}").as_str()) {
                        return self.say("ERR ARM crc");
                    }
                    self.image = data.clone();
                    self.armed = Some(Direction::In);
                    self.say("OK ARM dir=in window_ms=120000");
                } else {
                    self.armed = Some(Direction::Out);
                    self.say("OK ARM dir=out window_ms=120000");
                }
                self.step = 0;
            }
            ("GET", [block]) => {
                let block: usize = block.parse().unwrap();
                let data = &self.image[block * BLOCK_LEN..(block + 1) * BLOCK_LEN];
                let mut crc = protocol::crc32(data);
                if self.config.corrupt_gets > 0 {
                    self.config.corrupt_gets -= 1;
                    crc ^= 1;
                }
                let line = format!("OK GET {block} {} {crc:08X}", protocol::hex_encode(data));
                self.say(&line);
            }
            ("DIAG", _) => {
                let state = match self.armed {
                    Some(Direction::Out) => "out",
                    Some(Direction::In) => "in",
                    None => "idle",
                };
                self.say(&format!("# state={state} SCL=HIGH SDA=HIGH drive=HIGH (released)"));
                self.say("# 0x51 address bytes: 0 | wrong direction: no");
                self.say("OK DIAG");
            }
            ("TRACE", _) => {
                self.say("# trace: last 2 of 2 interrupt calls");
                self.say("# +0 S1 D0 G1 L0 Idle>Address b0");
                self.say("# +36 S0 D0 G1 L0 Address>Address b0");
                self.say("OK TRACE");
            }
            ("ABORT", _) => {
                self.say("OK ABORT");
                if let Some(direction) = self.armed.take() {
                    let dir = if direction == Direction::Out { "out" } else { "in" };
                    self.say(&format!(
                        "EV FAIL dir={dir} reason=aborted packets=0 verify=0 bytes=0 passes=0 mism=0"
                    ));
                }
            }
            (other, _) => self.say(&format!("ERR {other} unknown")),
        }
    }

    /// One step of the scripted radio while armed.
    fn radio_step(&mut self) {
        let Some(direction) = self.armed else { return };
        if self.config.reboot_when_armed {
            self.armed = None;
            return self.say("READY bridge proto=1 fw=1.0.0 boot=00000002");
        }
        if self.config.radio_never_starts {
            return;
        }
        let dir = if direction == Direction::Out { "out" } else { "in" };
        if self.step < self.config.progress_steps {
            self.step += 1;
            let line = match direction {
                Direction::Out => format!("EV PROGRESS dir=out packets={} verify=0", self.step * 200),
                Direction::In => format!("EV PROGRESS dir=in bytes={} passes=0", self.step * 6000),
            };
            return self.say(&line);
        }
        self.armed = None;
        if let Some(reason) = self.config.radio_fails.clone() {
            return self.say(&format!(
                "EV FAIL dir={dir} reason={reason} packets=100 verify=0 bytes=0 passes=0 mism=0"
            ));
        }
        match direction {
            Direction::Out => {
                self.image = self.config.radio_image.clone();
                let crc = protocol::crc32(&self.image);
                let verify = self.config.verified_blocks;
                let mism = u32::from(verify < BLOCK_COUNT as u32);
                self.say(&format!("EV DONE dir=out packets=752 verify={verify} mism={mism} crc={crc:08X}"));
            }
            Direction::In => {
                self.received_by_radio = Some(self.image.clone());
                self.say("EV DONE dir=in passes=2 bytes=24064 mism=0 past_end=0");
            }
        }
    }
}

impl Read for FakeBridge {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self.0.lock().unwrap();
        if state.output.is_empty() {
            state.radio_step();
        }
        if state.output.is_empty() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "no data"));
        }
        let n = buf.len().min(state.output.len());
        for (slot, byte) in buf.iter_mut().zip(state.output.drain(..n)) {
            *slot = byte;
        }
        Ok(n)
    }
}

impl Write for FakeBridge {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.0.lock().unwrap();
        state.input.extend_from_slice(buf);
        while let Some(end) = state.input.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = state.input.drain(..=end).collect();
            let text = String::from_utf8_lossy(&line).trim().to_owned();
            state.handle(&text);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

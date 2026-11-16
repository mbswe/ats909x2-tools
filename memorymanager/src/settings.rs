//! Menu settings and timers, kept in MemoBank A's settings block
//! (`MEMORYMAP.md`, section 6).
//!
//! The menu settings are packed as bits into `0x0F08` and `0x0F09`. Each one
//! was decoded by changing it alone on the radio and reading the radio again.

/// Offset of the three timers' times: minutes, then hours, in binary-coded decimal.
const TIMER_TIMES: usize = 0x0F30;
/// Offset of the three timers: frequency word, mode byte, flags byte.
const TIMER_ENTRIES: usize = 0x0F40;
/// Number of alarm timers.
pub const TIMERS: u8 = 3;

/// The bits of the settings bytes that hold decoded settings. A difference in
/// any other bit of these bytes is an unknown difference.
pub const SETTINGS_BITS: [(usize, u8); 2] = [(0x0F08, 0b0011_0011), (0x0F09, 0b1111_1111)];

/// A menu setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Setting {
    MemoryBank,
    Backlight,
    FmStep,
    FmStereo,
    SoftMute,
    ScanDelay,
    SsbStep,
    Beep,
}

impl Setting {
    pub const ALL: [Setting; 8] = [
        Setting::MemoryBank,
        Setting::Backlight,
        Setting::FmStep,
        Setting::FmStereo,
        Setting::SoftMute,
        Setting::ScanDelay,
        Setting::SsbStep,
        Setting::Beep,
    ];

    /// The name the radio's menu uses.
    pub const fn label(self) -> &'static str {
        match self {
            Setting::MemoryBank => "Memory bank",
            Setting::Backlight => "Backlight",
            Setting::FmStep => "FM step",
            Setting::FmStereo => "FM stereo",
            Setting::SoftMute => "Soft mute",
            Setting::ScanDelay => "Scan delay",
            Setting::SsbStep => "SSB step",
            Setting::Beep => "Beep",
        }
    }

    /// Offset of the byte holding the setting.
    pub const fn address(self) -> usize {
        match self {
            Setting::ScanDelay | Setting::SsbStep | Setting::Beep => 0x0F08,
            _ => 0x0F09,
        }
    }

    /// Lowest bit of the setting within its byte.
    pub const fn shift(self) -> u8 {
        match self {
            Setting::MemoryBank | Setting::ScanDelay => 0,
            Setting::Backlight => 2,
            Setting::FmStep | Setting::SsbStep => 4,
            Setting::Beep => 5,
            Setting::FmStereo => 6,
            Setting::SoftMute => 7,
        }
    }

    /// Number of bits the setting takes.
    pub const fn width(self) -> u8 {
        match self {
            Setting::MemoryBank | Setting::Backlight | Setting::FmStep | Setting::ScanDelay => 2,
            Setting::FmStereo | Setting::SoftMute | Setting::SsbStep | Setting::Beep => 1,
        }
    }

    /// The setting's bits within its byte.
    pub const fn mask(self) -> u8 {
        ((1u8 << self.width()) - 1) << self.shift()
    }

    /// The values confirmed on the radio, as (code, text), in the order the
    /// editor steps through them.
    pub const fn values(self) -> &'static [(u8, &'static str)] {
        match self {
            Setting::MemoryBank => &[(0, "A"), (1, "B"), (2, "C")],
            Setting::Backlight => &[(0, "10 s"), (1, "20 s"), (2, "30 s")],
            Setting::FmStep => &[(1, "50 kHz"), (0, "100 kHz"), (2, "200 kHz")],
            Setting::FmStereo => &[(0, "auto"), (1, "mono")],
            Setting::SoftMute => &[(0, "off"), (1, "on")],
            // 1.5 s is presumably code 2, but that has not been seen yet.
            Setting::ScanDelay => &[(0, "0.5 s"), (1, "1.0 s"), (3, "2.0 s")],
            Setting::SsbStep => &[(1, "10 Hz"), (0, "20 Hz")],
            Setting::Beep => &[(0, "on"), (1, "off")],
        }
    }

    /// The code after a factory reset.
    pub const fn factory(self) -> u8 {
        match self {
            Setting::Backlight | Setting::ScanDelay => 1,
            _ => 0,
        }
    }

    /// A code as text: its value, or the bits when the value is not known.
    pub fn text(self, code: u8) -> String {
        if let Some((_, text)) = self.values().iter().find(|(known, _)| *known == code) {
            return (*text).to_owned();
        }
        if self == Setting::ScanDelay && code == 2 {
            return "1.5 s (untested)".to_owned();
        }
        format!("unknown ({code:0width$b})", width = usize::from(self.width()))
    }

    /// The next (or, with `forward` false, the previous) confirmed value.
    /// An unknown code steps to the first or last confirmed value.
    pub fn step(self, code: u8, forward: bool) -> u8 {
        let values = self.values();
        let next = match values.iter().position(|(known, _)| *known == code) {
            Some(at) if forward => (at + 1) % values.len(),
            Some(at) => (at + values.len() - 1) % values.len(),
            None if forward => 0,
            None => values.len() - 1,
        };
        values[next].0
    }

    /// Where the setting is, for display: `0x0F09 bits 2–3`.
    pub fn location(self) -> String {
        let (low, high) = (self.shift(), self.shift() + self.width() - 1);
        if low == high {
            format!("0x{:04X} bit {low}", self.address())
        } else {
            format!("0x{:04X} bits {low}–{high}", self.address())
        }
    }
}

/// One alarm timer as stored. Only timer 1 with an FM station has been
/// confirmed on the radio; the others follow the same layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timer {
    pub number: u8,
    /// Bit 7 of the flags.
    pub on: bool,
    /// Bit 6 of the flags: radio; clear means the buzzer.
    pub radio: bool,
    /// The low 2 bits of the flags: 1 is FM (confirmed); 0 LW, 2 MW and 3 SW
    /// on the older ATS-909X.
    pub band_code: u8,
    /// The time bytes as stored, binary-coded decimal.
    pub hours: u8,
    pub minutes: u8,
    /// The frequency word.
    pub word: u16,
    pub mode: u8,
}

impl Timer {
    /// Reads timer `number` (1–3) from an image's bytes.
    pub fn decode(bytes: &[u8], number: u8) -> Self {
        assert!((1..=TIMERS).contains(&number), "timers are numbered 1–{TIMERS}");
        let index = usize::from(number - 1);
        let time = TIMER_TIMES + 2 * index;
        let entry = TIMER_ENTRIES + 4 * index;
        let flags = bytes[entry + 3];
        Self {
            number,
            on: flags & 0x80 != 0,
            radio: flags & 0x40 != 0,
            band_code: flags & 0x03,
            hours: bytes[time + 1],
            minutes: bytes[time],
            word: u16::from_le_bytes([bytes[entry], bytes[entry + 1]]),
            mode: bytes[entry + 2],
        }
    }

    /// The time as `12:34`, or the raw bytes if they are not valid BCD.
    pub fn time_text(&self) -> String {
        let bcd = |b: u8| (b >> 4 <= 9 && b & 0x0F <= 9).then(|| (b >> 4) * 10 + (b & 0x0F));
        match (bcd(self.hours), bcd(self.minutes)) {
            (Some(h), Some(m)) if h < 24 && m < 60 => format!("{h:02}:{m:02}"),
            _ => format!("{:02X}:{:02X}?", self.hours, self.minutes),
        }
    }

    /// The station as text. Band codes other than FM are not confirmed yet.
    pub fn station_text(&self) -> String {
        let value = self.word & 0x7FFF;
        match self.band_code {
            1 => format!("FM {}.{:02} MHz", value / 100, value % 100),
            0 => format!("LW? {value} kHz"),
            2 => format!("MW? {value} kHz"),
            _ => format!("SW? {value} kHz"),
        }
    }

    /// `on, radio`, `off, buzzer` and so on.
    pub fn state_text(&self) -> String {
        format!("{}, {}", if self.on { "on" } else { "off" }, if self.radio { "radio" } else { "buzzer" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_has_its_own_bits() {
        let mut used = [0u8; 2];
        for setting in Setting::ALL {
            let byte = if setting.address() == 0x0F08 { 0 } else { 1 };
            assert_eq!(used[byte] & setting.mask(), 0, "{setting:?} overlaps another setting");
            used[byte] |= setting.mask();
            assert!(setting.values().iter().any(|(code, _)| *code == setting.factory()));
            for (code, _) in setting.values() {
                assert_eq!(code << setting.shift() & !setting.mask(), 0, "{setting:?} code {code}");
            }
        }
        assert_eq!(used, [SETTINGS_BITS[0].1, SETTINGS_BITS[1].1]);
    }

    #[test]
    fn values_step_round_and_unknown_codes_show_their_bits() {
        assert_eq!(Setting::Backlight.step(1, true), 2);
        assert_eq!(Setting::Backlight.step(2, true), 0);
        assert_eq!(Setting::Backlight.step(0, false), 2);
        assert_eq!(Setting::FmStep.step(1, true), 0, "50 → 100 kHz");
        assert_eq!(Setting::ScanDelay.step(1, true), 3, "1.0 → 2.0 s, skipping the untested 1.5 s");
        assert_eq!(Setting::ScanDelay.text(2), "1.5 s (untested)");
        assert_eq!(Setting::Backlight.text(3), "unknown (11)");
        assert_eq!(Setting::Backlight.step(3, true), 0);
        assert_eq!(Setting::Backlight.location(), "0x0F09 bits 2–3");
        assert_eq!(Setting::Beep.location(), "0x0F08 bit 5");
    }

    #[test]
    fn timers_decode_as_on_the_radio() {
        // Timer 1 set to 12:34, radio, FM 89.30 MHz (read of 2026-11-14).
        let mut bytes = vec![0u8; 0x1000];
        bytes[0x0F30..0x0F32].copy_from_slice(&[0x34, 0x12]);
        bytes[0x0F40..0x0F44].copy_from_slice(&[0xE2, 0x22, 0x00, 0xC1]);
        bytes[0x0F44..0x0F48].copy_from_slice(&[0x10, 0x27, 0x00, 0x01]);
        let one = Timer::decode(&bytes, 1);
        assert_eq!((one.time_text(), one.state_text(), one.station_text()), ("12:34".into(), "on, radio".into(), "FM 89.30 MHz".into()));
        let two = Timer::decode(&bytes, 2);
        assert_eq!((two.time_text(), two.state_text(), two.station_text()), ("00:00".into(), "off, buzzer".into(), "FM 100.00 MHz".into()));
        bytes[0x0F33] = 0x7A;
        assert_eq!(Timer::decode(&bytes, 2).time_text(), "7A:00?");
    }
}

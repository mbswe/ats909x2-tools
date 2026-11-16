//! Parser and editor for ATS-909X2 DATA OUT images.
//!
//! A supported image is exactly 12,032 bytes and mirrors EEPROM offsets
//! `0x0000–0x2EFF`. The loaded bytes are the backing buffer: every edit patches
//! only the bytes of the field being changed, so padding, metadata, other bands
//! and unknown bytes pass through untouched.
//!
//! The layout is described in `MEMORYMAP.md`. In short, with one-based page
//! and slot numbers:
//!
//! - a preset is a 16-bit little-endian word: bits 0–14 hold the frequency in
//!   the band's unit (zero means empty), bit 15 is the memory lock;
//! - SW keeps nine words per 32-byte page record, with one mode byte per
//!   preset at `+0x14`; the other bands keep one plain list of words, and MW
//!   a list of mode bytes at `0x0800`;
//! - names are ten bytes, three to a 32-byte record. SW names its pages; FM,
//!   MW, LW and AIR name each preset.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::settings::{SETTINGS_BITS, Setting, Timer};

/// Exact length of a supported image: EEPROM offsets `0x0000–0x2EFF`.
pub const IMAGE_LEN: usize = 0x2F00;
/// Shortwave memory pages per region.
pub const PAGES: u8 = 49;
/// Preset slots per shortwave page.
pub const SLOTS: u8 = 9;
/// Bytes per page-name field.
pub const NAME_LEN: usize = 10;
/// Lowest frequency accepted for new entries (ATS-909X2 SW band specification).
pub const SW_MIN_KHZ: u16 = 1711;
/// Highest frequency accepted for new entries (ATS-909X2 SW band specification).
pub const SW_MAX_KHZ: u16 = 29_999;

/// The text every image from the radio carries in MemoBank A's settings.
/// On the older ATS-909 the radio looks for its product string after a DATA
/// IN and restores factory defaults when it is missing, so an image without
/// it is never sent.
pub const PRODUCT_STRING: &[u8; 16] = b"SANGEANATS-909X2";
/// Offset of [`PRODUCT_STRING`] in the image.
pub const PRODUCT_STRING_AT: usize = 0x0F50;

const FREQUENCY_TABLE: usize = 0x0840;
const NAME_TABLE: usize = 0x0300;
const RECORD_LEN: usize = 0x20;
const NAMES_PER_RECORD: usize = 3;
/// Offset of the nine mode bytes within an SW page record.
const MODE_OFFSET: usize = 0x14;
/// Offset of MW's mode bytes within a bank: one per preset, in preset order.
const MW_MODE_TABLE: usize = 0x0800;
/// Bit 15 of a preset word: the preset is locked on the radio.
const LOCK_BIT: u16 = 0x8000;

/// A waveband with its own presets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Band {
    Fm,
    Mw,
    Lw,
    Sw,
    Air,
}

impl Band {
    /// In the order their frequency lists appear in the image.
    pub const ALL: [Band; 5] = [Band::Fm, Band::Mw, Band::Lw, Band::Sw, Band::Air];

    pub const fn label(self) -> &'static str {
        match self {
            Band::Fm => "FM",
            Band::Mw => "MW",
            Band::Lw => "LW",
            Band::Sw => "SW",
            Band::Air => "AIR",
        }
    }

    pub const fn index(self) -> usize {
        match self {
            Band::Fm => 0,
            Band::Mw => 1,
            Band::Lw => 2,
            Band::Sw => 3,
            Band::Air => 4,
        }
    }

    /// Memory pages of nine presets (operating manual, page 10).
    pub const fn pages(self) -> u8 {
        match self {
            Band::Fm => 4,
            Band::Mw => 3,
            Band::Lw => 1,
            Band::Sw => PAGES,
            Band::Air => 5,
        }
    }

    /// Offset of the band's first frequency word within a bank.
    const fn frequency_table(self) -> usize {
        match self {
            Band::Fm => 0x0760,
            Band::Mw => 0x07C0,
            Band::Lw => 0x0820,
            Band::Sw => FREQUENCY_TABLE,
            Band::Air => 0x0E60,
        }
    }

    /// Number of the band's first name, counting all names of a bank from 0.
    /// For SW these are page names, for the other bands preset names.
    const fn first_name(self) -> usize {
        match self {
            Band::Fm => 0,
            Band::Mw => 45,
            Band::Sw => 72,
            Band::Lw => 123,
            Band::Air => 132,
        }
    }

    /// Whether each preset has its own name. On SW only pages have names.
    pub const fn names_presets(self) -> bool {
        !matches!(self, Band::Sw)
    }

    /// Lowest frequency accepted for new entries, in kHz (operating manual, page 17).
    pub const fn min_khz(self) -> u32 {
        match self {
            Band::Fm => 64_000,
            Band::Mw => 520,
            Band::Lw => 100,
            Band::Sw => SW_MIN_KHZ as u32,
            Band::Air => 118_000,
        }
    }

    /// Highest frequency accepted for new entries, in kHz.
    pub const fn max_khz(self) -> u32 {
        match self {
            Band::Fm => 108_000,
            Band::Mw => 1710,
            Band::Lw => 519,
            Band::Sw => SW_MAX_KHZ as u32,
            Band::Air => 137_000,
        }
    }

    /// The radio's finest tuning step, in kHz (operating manual, page 9).
    pub const fn step_khz(self) -> u32 {
        match self {
            Band::Fm => 50,
            Band::Air => 5,
            Band::Mw | Band::Lw | Band::Sw => 1,
        }
    }

    /// Decimals a typed frequency may have: two for FM, three for AIR.
    pub const fn decimals(self) -> usize {
        match self {
            Band::Fm => 2,
            Band::Air => 3,
            Band::Mw | Band::Lw | Band::Sw => 0,
        }
    }

    /// Whether frequencies are entered and shown in MHz.
    pub const fn in_mhz(self) -> bool {
        matches!(self, Band::Fm | Band::Air)
    }

    /// The stored value for a frequency: FM in 10 kHz, AIR as kHz above
    /// 110 MHz, the other bands in kHz.
    const fn to_stored(self, khz: u32) -> u16 {
        (match self {
            Band::Fm => khz / 10,
            Band::Air => khz - 110_000,
            Band::Mw | Band::Lw | Band::Sw => khz,
        }) as u16
    }

    const fn stored_to_khz(self, stored: u16) -> u32 {
        match self {
            Band::Fm => stored as u32 * 10,
            Band::Air => stored as u32 + 110_000,
            Band::Mw | Band::Lw | Band::Sw => stored as u32,
        }
    }

    /// A frequency as text without its unit, the way it is typed: `91.30`,
    /// `121.500`, `9765`.
    pub fn number_text(self, khz: u32) -> String {
        match self {
            Band::Fm => format!("{}.{:02}", khz / 1000, khz % 1000 / 10),
            Band::Air => format!("{}.{:03}", khz / 1000, khz % 1000),
            Band::Mw | Band::Lw | Band::Sw => khz.to_string(),
        }
    }

    pub const fn unit(self) -> &'static str {
        if self.in_mhz() { "MHz" } else { "kHz" }
    }

    /// A frequency with its unit: `91.30 MHz`, `9765 kHz`.
    pub fn frequency_text(self, khz: u32) -> String {
        format!("{} {}", self.number_text(khz), self.unit())
    }

    /// The accepted range as text: `64.00–108.00 MHz`.
    pub fn range_text(self) -> String {
        format!("{}–{} {}", self.number_text(self.min_khz()), self.number_text(self.max_khz()), self.unit())
    }
}

/// One of the three repeated memory regions: MemoBank A, B and C at EEPROM
/// bases `0x0000`, `0x1000` and `0x2000`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Region {
    BankA,
    BankB,
    BankC,
}

impl Region {
    pub const ALL: [Region; 3] = [Region::BankA, Region::BankB, Region::BankC];

    pub const fn base(self) -> usize {
        match self {
            Region::BankA => 0x0000,
            Region::BankB => 0x1000,
            Region::BankC => 0x2000,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Region::BankA => "MemoBank A",
            Region::BankB => "MemoBank B",
            Region::BankC => "MemoBank C",
        }
    }

    pub const fn offset_label(self) -> &'static str {
        match self {
            Region::BankA => "0x0000",
            Region::BankB => "0x1000",
            Region::BankC => "0x2000",
        }
    }

    pub const fn index(self) -> usize {
        match self {
            Region::BankA => 0,
            Region::BankB => 1,
            Region::BankC => 2,
        }
    }
}

/// A one-based shortwave page number, 1–49.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Page(u8);

impl Page {
    pub fn new(number: u8) -> Result<Self, EditError> {
        if (1..=PAGES).contains(&number) {
            Ok(Self(number))
        } else {
            Err(EditError::PageOutOfRange(number))
        }
    }

    pub const fn number(self) -> u8 {
        self.0
    }

    pub fn all() -> impl Iterator<Item = Page> {
        (1..=PAGES).map(Page)
    }

    const fn index(self) -> usize {
        self.0 as usize - 1
    }
}

/// A one-based slot number within a page, 1–9.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(u8);

impl Slot {
    pub fn new(number: u8) -> Result<Self, EditError> {
        if (1..=SLOTS).contains(&number) {
            Ok(Self(number))
        } else {
            Err(EditError::SlotOutOfRange(number))
        }
    }

    pub const fn number(self) -> u8 {
        self.0
    }

    pub fn all() -> impl Iterator<Item = Slot> {
        (1..=SLOTS).map(Slot)
    }

    const fn index(self) -> usize {
        self.0 as usize - 1
    }
}

/// One preset: a position on a page of a band in a MemoBank.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PresetRef {
    pub region: Region,
    pub band: Band,
    page: Page,
    pub slot: Slot,
}

impl PresetRef {
    /// Fails when the band has fewer pages.
    pub fn new(region: Region, band: Band, page: Page, slot: Slot) -> Result<Self, EditError> {
        if page.number() > band.pages() {
            return Err(EditError::PageOutOfRange(page.number()));
        }
        Ok(Self { region, band, page, slot })
    }

    /// An SW preset; every page number is valid on SW.
    pub const fn sw(region: Region, page: Page, slot: Slot) -> Self {
        Self { region, band: Band::Sw, page, slot }
    }

    pub const fn page(self) -> Page {
        self.page
    }

    /// The preset's number within its band, counting from 0.
    const fn index(self) -> usize {
        SLOTS as usize * self.page.index() + self.slot.index()
    }

    /// Offset of the preset's two-byte word.
    pub const fn address(self) -> usize {
        match self.band {
            Band::Sw => frequency_address(self.region, self.page, self.slot),
            _ => self.region.base() + self.band.frequency_table() + 2 * self.index(),
        }
    }

    /// Offset of the preset's mode byte, on SW and MW. Where LW keeps its
    /// modes is not known yet; FM and AIR have no SSB, so no mode.
    pub const fn mode_address(self) -> Option<usize> {
        match self.band {
            Band::Sw => Some(
                self.region.base()
                    + FREQUENCY_TABLE
                    + RECORD_LEN * self.page.index()
                    + MODE_OFFSET
                    + self.slot.index(),
            ),
            Band::Mw => Some(self.region.base() + MW_MODE_TABLE + self.index()),
            Band::Fm | Band::Lw | Band::Air => None,
        }
    }

    /// The preset's own name. `None` on SW, where pages are named.
    pub const fn name(self) -> Option<NameRef> {
        if self.band.names_presets() { Some(NameRef::Preset(self)) } else { None }
    }
}

/// A ten-byte name: of an SW page, or of a preset on another band.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NameRef {
    SwPage(Region, Page),
    Preset(PresetRef),
}

impl NameRef {
    pub const fn region(self) -> Region {
        match self {
            NameRef::SwPage(region, _) => region,
            NameRef::Preset(at) => at.region,
        }
    }

    /// The name's number within its bank, counting all names from 0.
    const fn number(self) -> usize {
        match self {
            NameRef::SwPage(_, page) => Band::Sw.first_name() + page.index(),
            NameRef::Preset(at) => at.band.first_name() + at.index(),
        }
    }

    /// Offset of the ten name bytes.
    pub const fn address(self) -> usize {
        let number = self.number();
        self.region().base() + RECORD_LEN * (number / NAMES_PER_RECORD) + NAME_LEN * (number % NAMES_PER_RECORD)
    }
}

/// What a mode byte says about the preset's mode. The low bits (SSB fine
/// tuning) are not understood yet and are left alone.
pub fn mode_label(mode: u8) -> &'static str {
    match mode {
        0 => "AM",
        m if m & 0x80 != 0 && m & 0x40 != 0 => "USB",
        m if m & 0x80 != 0 => "LSB",
        _ => "?",
    }
}

/// Offset of a slot's two frequency bytes.
pub const fn frequency_address(region: Region, page: Page, slot: Slot) -> usize {
    region.base() + FREQUENCY_TABLE + RECORD_LEN * page.index() + 2 * slot.index()
}

/// Offset of a page's ten name bytes.
pub const fn page_name_address(region: Region, page: Page) -> usize {
    region.base()
        + NAME_TABLE
        + RECORD_LEN * (page.index() / NAMES_PER_RECORD)
        + NAME_LEN * (page.index() % NAMES_PER_RECORD)
}

/// Why a file could not be used as an image.
#[derive(Debug)]
pub enum ImageError {
    Io(io::Error),
    NotAFile,
    InvalidLength { actual: u64 },
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Io(err) => write!(f, "{err}"),
            ImageError::NotAFile => f.write_str("not a regular file"),
            ImageError::InvalidLength { actual } => write!(
                f,
                "expected exactly {IMAGE_LEN} bytes (EEPROM 0x0000–0x2EFF), found {actual}"
            ),
        }
    }
}

impl std::error::Error for ImageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ImageError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for ImageError {
    fn from(err: io::Error) -> Self {
        ImageError::Io(err)
    }
}

/// Why an edit was refused. The image is unchanged when one is returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    PageOutOfRange(u8),
    SlotOutOfRange(u8),
    EmptyFrequency,
    NotWholeKhz,
    NotANumber,
    FrequencyOutOfRange(String),
    NameInvalidChar(char),
    NameTooLong { chars: usize },
    /// Not a frequency the way this band's are written.
    FrequencyFormat(Band),
    OutOfBand { band: Band, input: String },
    OffStep(Band),
    /// The preset holds no frequency, so it cannot be locked.
    EmptyPreset,
    /// A code that has not been confirmed for this setting.
    UnknownSettingValue(Setting),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::PageOutOfRange(n) => write!(f, "page {n} is outside 1–{PAGES}"),
            EditError::SlotOutOfRange(n) => write!(f, "slot {n} is outside 1–{SLOTS}"),
            EditError::EmptyFrequency => f.write_str("enter a frequency in kHz"),
            EditError::NotWholeKhz => f.write_str("enter whole kHz, for example 9765"),
            EditError::NotANumber => f.write_str("frequency must contain digits only"),
            EditError::FrequencyOutOfRange(input) => write!(
                f,
                "{input} kHz is outside the SW range {SW_MIN_KHZ}–{SW_MAX_KHZ} kHz"
            ),
            EditError::NameInvalidChar(c) => {
                write!(f, "{c:?} is not allowed; use ASCII letters, digits and spaces")
            }
            EditError::NameTooLong { chars } => {
                write!(f, "name has {chars} characters; the limit is {NAME_LEN}")
            }
            EditError::FrequencyFormat(band) => write!(
                f,
                "enter {} in {}, for example {}",
                band.label(),
                band.unit(),
                band.number_text(example_khz(*band))
            ),
            EditError::OutOfBand { band, input } => write!(
                f,
                "{input} {} is outside the {} range {}",
                band.unit(),
                band.label(),
                band.range_text()
            ),
            EditError::OffStep(band) => write!(
                f,
                "{} frequencies go in steps of {} kHz",
                band.label(),
                band.step_khz()
            ),
            EditError::EmptyPreset => f.write_str("an empty position cannot be locked"),
            EditError::UnknownSettingValue(setting) => {
                write!(f, "that value has not been confirmed for {}", setting.label())
            }
        }
    }
}

impl std::error::Error for EditError {}

/// Parses user input as a whole-kHz shortwave frequency.
pub fn parse_frequency_khz(input: &str) -> Result<u16, EditError> {
    let text = input.trim();
    if text.is_empty() {
        return Err(EditError::EmptyFrequency);
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(if text.contains(['.', ',']) {
            EditError::NotWholeKhz
        } else {
            EditError::NotANumber
        });
    }
    text.parse::<u16>()
        .ok()
        .filter(|khz| (SW_MIN_KHZ..=SW_MAX_KHZ).contains(khz))
        .ok_or_else(|| EditError::FrequencyOutOfRange(text.to_owned()))
}

const fn example_khz(band: Band) -> u32 {
    match band {
        Band::Fm => 91_300,
        Band::Mw => 999,
        Band::Lw => 198,
        Band::Sw => 9765,
        Band::Air => 121_500,
    }
}

/// Parses user input as a frequency of `band`, in kHz. SW, MW and LW take
/// whole kHz; FM and AIR take MHz with up to two and three decimals.
pub fn parse_frequency(band: Band, input: &str) -> Result<u32, EditError> {
    if band == Band::Sw {
        return parse_frequency_khz(input).map(u32::from);
    }
    let text = input.trim();
    if text.is_empty() {
        return Err(EditError::EmptyFrequency);
    }
    let khz = if band.in_mhz() {
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        let decimals = band.decimals();
        let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        if whole.is_empty() || whole.len() > 3 || !digits(whole) || !digits(fraction) || fraction.len() > decimals {
            return Err(EditError::FrequencyFormat(band));
        }
        let mut thousandths = fraction.to_owned();
        while thousandths.len() < 3 {
            thousandths.push('0');
        }
        let parse = |part: &str| part.parse::<u32>().map_err(|_| EditError::FrequencyFormat(band));
        parse(whole)? * 1000 + parse(&thousandths)?
    } else {
        if text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(EditError::FrequencyFormat(band));
        }
        text.parse::<u32>().map_err(|_| EditError::FrequencyFormat(band))?
    };
    check_frequency(band, khz, text)?;
    Ok(khz)
}

fn check_frequency(band: Band, khz: u32, input: &str) -> Result<(), EditError> {
    if !(band.min_khz()..=band.max_khz()).contains(&khz) {
        return Err(if band == Band::Sw {
            EditError::FrequencyOutOfRange(input.to_owned())
        } else {
            EditError::OutOfBand { band, input: input.to_owned() }
        });
    }
    if !khz.is_multiple_of(band.step_khz()) {
        return Err(EditError::OffStep(band));
    }
    Ok(())
}

/// Whether a character may appear in a newly written page name.
pub fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == ' '
}

/// The byte the radio's own name editor stores for a blank (`@` in ASCII), for
/// example `BBC 1` in MemoBank A page 1 is stored as `BBC@1@@@@@`. Shown as a
/// space, but never written.
pub const RADIO_BLANK: u8 = 0x40;

/// The byte written for spaces and padding in page names: an ASCII space.
///
/// The factory names use `0x20`, and the radio displays it as a space between
/// words and as padding. If names written with `0x20` misbehave on a radio,
/// changing this to [`RADIO_BLANK`] switches every written blank to `0x40`.
pub const NAME_SPACE: u8 = 0x20;

/// Encodes a new page name as ten bytes: letters and digits as ASCII, each
/// space and the padding as [`NAME_SPACE`].
pub fn encode_page_name(name: &str) -> Result<[u8; NAME_LEN], EditError> {
    if let Some(c) = name.chars().find(|&c| !is_name_char(c)) {
        return Err(EditError::NameInvalidChar(c));
    }
    // Only ASCII remains, so the byte length equals the character count.
    if name.len() > NAME_LEN {
        return Err(EditError::NameTooLong { chars: name.len() });
    }
    let mut field = [NAME_SPACE; NAME_LEN];
    for (byte, c) in field.iter_mut().zip(name.bytes()) {
        *byte = if c == b' ' { NAME_SPACE } else { c };
    }
    Ok(field)
}

/// Renders name bytes as the radio shows them: both blank encodings (`0x20`
/// and `0x40`) as spaces, other printable ASCII as-is, anything else as `·`.
/// A zero byte also reads as a blank: the radio fills the name of a preset
/// stored without one with zeros.
pub fn name_text(field: &[u8; NAME_LEN]) -> String {
    field
        .iter()
        .map(|&b| match b {
            RADIO_BLANK | 0x00 => ' ',
            0x20..=0x7E => b as char,
            _ => '·',
        })
        .collect()
}

/// Differences between two images, from [`EepromImage::compare`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageDiff {
    /// Presets whose word or mode byte differs, on any band.
    pub presets: Vec<PresetRef>,
    /// SW page names and preset names whose ten bytes differ.
    pub names: Vec<NameRef>,
    /// Menu settings whose value differs.
    pub settings: Vec<Setting>,
    /// Differing bytes outside the preset, name and settings fields. The
    /// editor never changes these, so a non-zero count means another source.
    pub other_bytes: usize,
}

impl ImageDiff {
    pub fn is_empty(&self) -> bool {
        self.presets.is_empty() && self.names.is_empty() && self.settings.is_empty() && self.other_bytes == 0
    }
}

/// Every preset of every band and bank, in image order.
pub fn all_presets() -> impl Iterator<Item = PresetRef> {
    Region::ALL.into_iter().flat_map(|region| {
        Band::ALL.into_iter().flat_map(move |band| {
            (1..=band.pages()).flat_map(move |page| {
                Slot::all().map(move |slot| PresetRef { region, band, page: Page(page), slot })
            })
        })
    })
}

/// Every name of every bank: SW page names and the other bands' preset names.
pub fn all_names() -> impl Iterator<Item = NameRef> {
    let pages = Region::ALL
        .into_iter()
        .flat_map(|region| Page::all().map(move |page| NameRef::SwPage(region, page)));
    pages.chain(all_presets().filter_map(PresetRef::name))
}

/// An editable DATA OUT image.
///
/// Holds the working bytes and a snapshot of the bytes as last loaded or
/// saved, which drives change tracking.
#[derive(Clone, PartialEq, Eq)]
pub struct EepromImage {
    bytes: Box<[u8; IMAGE_LEN]>,
    saved: Box<[u8; IMAGE_LEN]>,
}

impl fmt::Debug for EepromImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EepromImage")
            .field("modified_bytes", &self.modified_byte_count())
            .finish_non_exhaustive()
    }
}

impl EepromImage {
    /// A new image made from this one: every preset of every band emptied
    /// (its word and, on SW, its mode byte) and every name blanked with
    /// [`NAME_SPACE`], in all three MemoBanks. All other bytes, including the
    /// settings and unknown ones, stay exactly as they are here, so a new
    /// image always carries the bytes of the radio it came from. The result
    /// counts as saved, so it is not reported as modified until edited.
    pub fn cleared(&self) -> Self {
        let mut bytes = *self.bytes;
        for preset in all_presets() {
            let at = preset.address();
            bytes[at..at + 2].fill(0);
            if let Some(mode) = preset.mode_address() {
                bytes[mode] = 0;
            }
        }
        for name in all_names() {
            let at = name.address();
            bytes[at..at + NAME_LEN].fill(NAME_SPACE);
        }
        Self {
            bytes: Box::new(bytes),
            saved: Box::new(bytes),
        }
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, ImageError> {
        let bytes: [u8; IMAGE_LEN] = data.try_into().map_err(|_| ImageError::InvalidLength {
            actual: data.len() as u64,
        })?;
        Ok(Self {
            bytes: Box::new(bytes),
            saved: Box::new(bytes),
        })
    }

    pub fn load(path: &Path) -> Result<Self, ImageError> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(ImageError::NotAFile);
        }
        if meta.len() != IMAGE_LEN as u64 {
            return Err(ImageError::InvalidLength { actual: meta.len() });
        }
        // Read one byte past the limit so a file that grew after the length check is still refused.
        let mut data = Vec::with_capacity(IMAGE_LEN + 1);
        file.take(IMAGE_LEN as u64 + 1).read_to_end(&mut data)?;
        Self::from_bytes(&data)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..]
    }

    /// Whether the image carries the radio's product string where a radio's
    /// export has it. Without it the image is not one to send to a radio.
    pub fn has_product_string(&self) -> bool {
        self.bytes[PRODUCT_STRING_AT..PRODUCT_STRING_AT + PRODUCT_STRING.len()] == PRODUCT_STRING[..]
    }

    // ---- Presets of any band ----

    /// The preset's frequency in kHz, or `None` for an empty position.
    pub fn frequency(&self, at: PresetRef) -> Option<u32> {
        frequency_of(at.band, read_u16(&self.bytes, at.address()))
    }

    /// The preset's frequency as last loaded or saved.
    pub fn saved_frequency(&self, at: PresetRef) -> Option<u32> {
        frequency_of(at.band, read_u16(&self.saved, at.address()))
    }

    /// Whether the preset is locked on the radio (Memory Lock).
    pub fn is_locked(&self, at: PresetRef) -> bool {
        read_u16(&self.bytes, at.address()) & LOCK_BIT != 0
    }

    /// The preset's mode byte, on SW.
    pub fn mode(&self, at: PresetRef) -> Option<u8> {
        at.mode_address().map(|address| self.bytes[address])
    }

    /// Stores a frequency. The lock and the mode byte stay as they are.
    pub fn set_frequency(&mut self, at: PresetRef, khz: u32) -> Result<(), EditError> {
        check_frequency(at.band, khz, &at.band.number_text(khz))?;
        let lock = read_u16(&self.bytes, at.address()) & LOCK_BIT;
        self.write_u16(at.address(), lock | at.band.to_stored(khz));
        Ok(())
    }

    /// Locks or unlocks a stored preset.
    pub fn set_locked(&mut self, at: PresetRef, locked: bool) -> Result<(), EditError> {
        let word = read_u16(&self.bytes, at.address());
        if word & !LOCK_BIT == 0 {
            return Err(EditError::EmptyPreset);
        }
        self.write_u16(at.address(), if locked { word | LOCK_BIT } else { word & !LOCK_BIT });
        Ok(())
    }

    /// Empties a position: the frequency and lock, the SW mode byte, and on
    /// the other bands the preset's name, which goes back to blanks.
    pub fn clear_preset(&mut self, at: PresetRef) {
        self.write_u16(at.address(), 0);
        if let Some(address) = at.mode_address() {
            self.bytes[address] = 0;
        }
        if let Some(name) = at.name() {
            let address = name.address();
            self.bytes[address..address + NAME_LEN].fill(NAME_SPACE);
        }
    }

    pub fn used_presets(&self, region: Region, band: Band, page: Page) -> usize {
        Slot::all()
            .filter_map(|slot| PresetRef::new(region, band, page, slot).ok())
            .filter(|&at| self.frequency(at).is_some())
            .count()
    }

    /// Whether the preset's word or mode byte differs from the saved bytes.
    pub fn is_preset_modified(&self, at: PresetRef) -> bool {
        self.range_modified(at.address(), 2)
            || at.mode_address().is_some_and(|address| self.range_modified(address, 1))
    }

    /// Restores the preset's word and mode byte as last loaded or saved.
    pub fn revert_preset(&mut self, at: PresetRef) {
        self.revert_range(at.address(), 2);
        if let Some(address) = at.mode_address() {
            self.revert_range(address, 1);
        }
    }

    pub fn name(&self, name: NameRef) -> [u8; NAME_LEN] {
        name_field(&self.bytes, name.address())
    }

    pub fn saved_name(&self, name: NameRef) -> [u8; NAME_LEN] {
        name_field(&self.saved, name.address())
    }

    pub fn set_name(&mut self, name: NameRef, text: &str) -> Result<(), EditError> {
        let field = encode_page_name(text)?;
        let at = name.address();
        self.bytes[at..at + NAME_LEN].copy_from_slice(&field);
        Ok(())
    }

    pub fn is_name_modified(&self, name: NameRef) -> bool {
        self.range_modified(name.address(), NAME_LEN)
    }

    pub fn revert_name(&mut self, name: NameRef) {
        self.revert_range(name.address(), NAME_LEN);
    }

    /// Whether anything on a band's page differs from the saved bytes: its
    /// presets, and its page name (SW) or preset names (other bands).
    pub fn is_band_page_modified(&self, region: Region, band: Band, page: Page) -> bool {
        if band == Band::Sw && self.is_name_modified(NameRef::SwPage(region, page)) {
            return true;
        }
        Slot::all().filter_map(|slot| PresetRef::new(region, band, page, slot).ok()).any(|at| {
            self.is_preset_modified(at) || at.name().is_some_and(|name| self.is_name_modified(name))
        })
    }

    pub fn is_band_modified(&self, region: Region, band: Band) -> bool {
        (1..=band.pages()).map(Page).any(|page| self.is_band_page_modified(region, band, page))
    }

    // ---- SW, by page and slot ----

    /// The slot's frequency in kHz, or `None` for an empty slot.
    pub fn frequency_khz(&self, region: Region, page: Page, slot: Slot) -> Option<u16> {
        self.frequency(PresetRef::sw(region, page, slot)).map(|khz| khz as u16)
    }

    /// The slot's frequency as last loaded or saved.
    pub fn saved_frequency_khz(&self, region: Region, page: Page, slot: Slot) -> Option<u16> {
        self.saved_frequency(PresetRef::sw(region, page, slot)).map(|khz| khz as u16)
    }

    pub fn set_frequency_khz(
        &mut self,
        region: Region,
        page: Page,
        slot: Slot,
        khz: u16,
    ) -> Result<(), EditError> {
        self.set_frequency(PresetRef::sw(region, page, slot), u32::from(khz))
    }

    pub fn clear_slot(&mut self, region: Region, page: Page, slot: Slot) {
        self.clear_preset(PresetRef::sw(region, page, slot));
    }

    pub fn used_slots(&self, region: Region, page: Page) -> usize {
        self.used_presets(region, Band::Sw, page)
    }

    pub fn page_name(&self, region: Region, page: Page) -> [u8; NAME_LEN] {
        name_field(&self.bytes, page_name_address(region, page))
    }

    pub fn saved_page_name(&self, region: Region, page: Page) -> [u8; NAME_LEN] {
        name_field(&self.saved, page_name_address(region, page))
    }

    pub fn set_page_name(&mut self, region: Region, page: Page, name: &str) -> Result<(), EditError> {
        let field = encode_page_name(name)?;
        let at = page_name_address(region, page);
        self.bytes[at..at + NAME_LEN].copy_from_slice(&field);
        Ok(())
    }

    pub fn is_slot_modified(&self, region: Region, page: Page, slot: Slot) -> bool {
        self.is_preset_modified(PresetRef::sw(region, page, slot))
    }

    pub fn is_page_name_modified(&self, region: Region, page: Page) -> bool {
        self.range_modified(page_name_address(region, page), NAME_LEN)
    }

    /// Whether the page's name or any of its slots differ from the saved bytes.
    pub fn is_page_modified(&self, region: Region, page: Page) -> bool {
        self.is_page_name_modified(region, page)
            || Slot::all().any(|slot| self.is_slot_modified(region, page, slot))
    }

    /// Whether any preset or name of the bank differs from the saved bytes.
    pub fn is_region_modified(&self, region: Region) -> bool {
        Band::ALL.into_iter().any(|band| self.is_band_modified(region, band))
    }

    /// Restores a slot's bytes as last loaded or saved.
    pub fn revert_slot(&mut self, region: Region, page: Page, slot: Slot) {
        self.revert_preset(PresetRef::sw(region, page, slot));
    }

    /// Restores a page name's bytes as last loaded or saved.
    pub fn revert_page_name(&mut self, region: Region, page: Page) {
        self.revert_range(page_name_address(region, page), NAME_LEN);
    }

    pub fn is_modified(&self) -> bool {
        self.bytes != self.saved
    }

    pub fn modified_byte_count(&self) -> usize {
        self.bytes
            .iter()
            .zip(self.saved.iter())
            .filter(|(a, b)| a != b)
            .count()
    }

    /// Field-by-field differences from `other` (current bytes of both).
    pub fn compare(&self, other: &EepromImage) -> ImageDiff {
        let mut diff = ImageDiff::default();
        let mut known = [false; IMAGE_LEN];
        let differs = |at: usize, len: usize| self.bytes[at..at + len] != other.bytes[at..at + len];
        for preset in all_presets() {
            let at = preset.address();
            known[at..at + 2].fill(true);
            let mut changed = differs(at, 2);
            if let Some(mode) = preset.mode_address() {
                known[mode] = true;
                changed |= differs(mode, 1);
            }
            if changed {
                diff.presets.push(preset);
            }
        }
        for name in all_names() {
            let at = name.address();
            known[at..at + NAME_LEN].fill(true);
            if differs(at, NAME_LEN) {
                diff.names.push(name);
            }
        }
        diff.settings = Setting::ALL
            .into_iter()
            .filter(|&setting| self.setting(setting) != other.setting(setting))
            .collect();
        // A settings byte counts as an unknown difference only when bits
        // outside the decoded settings differ.
        for (at, bits) in SETTINGS_BITS {
            known[at] = true;
            if (self.bytes[at] ^ other.bytes[at]) & !bits != 0 {
                diff.other_bytes += 1;
            }
        }
        diff.other_bytes += (0..IMAGE_LEN)
            .filter(|&at| !known[at] && self.bytes[at] != other.bytes[at])
            .count();
        diff
    }

    // ---- Menu settings and timers ----

    /// A menu setting's code, for example 1 for a 20 s backlight.
    pub fn setting(&self, setting: Setting) -> u8 {
        (self.bytes[setting.address()] & setting.mask()) >> setting.shift()
    }

    /// The setting's code as last loaded or saved.
    pub fn saved_setting(&self, setting: Setting) -> u8 {
        (self.saved[setting.address()] & setting.mask()) >> setting.shift()
    }

    /// Changes a setting to a confirmed value. The other bits of its byte stay.
    pub fn set_setting(&mut self, setting: Setting, code: u8) -> Result<(), EditError> {
        if !setting.values().iter().any(|(known, _)| *known == code) {
            return Err(EditError::UnknownSettingValue(setting));
        }
        let at = setting.address();
        self.bytes[at] = (self.bytes[at] & !setting.mask()) | (code << setting.shift());
        Ok(())
    }

    pub fn is_setting_modified(&self, setting: Setting) -> bool {
        self.setting(setting) != self.saved_setting(setting)
    }

    /// Restores a setting's bits as last loaded or saved.
    pub fn revert_setting(&mut self, setting: Setting) {
        let at = setting.address();
        self.bytes[at] = (self.bytes[at] & !setting.mask()) | (self.saved[at] & setting.mask());
    }

    /// Alarm timer 1–3.
    pub fn timer(&self, number: u8) -> Timer {
        Timer::decode(&self.bytes[..], number)
    }

    /// Writes the image to `path` atomically, verifies it by reading it back,
    /// and makes the written bytes the new saved state.
    ///
    /// On error the image keeps its unsaved changes.
    pub fn save(&mut self, path: &Path) -> io::Result<()> {
        write_atomically(path, &self.bytes[..])?;
        *self.saved = *self.bytes;
        Ok(())
    }

    fn write_u16(&mut self, at: usize, value: u16) {
        self.bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn range_modified(&self, at: usize, len: usize) -> bool {
        self.bytes[at..at + len] != self.saved[at..at + len]
    }

    fn revert_range(&mut self, at: usize, len: usize) {
        self.bytes[at..at + len].copy_from_slice(&self.saved[at..at + len]);
    }
}

fn read_u16(bytes: &[u8; IMAGE_LEN], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// The frequency in a preset word, without the lock bit; zero means empty.
fn frequency_of(band: Band, word: u16) -> Option<u32> {
    let stored = word & !LOCK_BIT;
    (stored != 0).then(|| band.stored_to_khz(stored))
}

fn name_field(bytes: &[u8; IMAGE_LEN], at: usize) -> [u8; NAME_LEN] {
    let mut field = [0; NAME_LEN];
    field.copy_from_slice(&bytes[at..at + NAME_LEN]);
    field
}

/// Writes to a temporary file next to the target, syncs it, renames it over
/// the target, then reads the target back and compares.
fn write_atomically(path: &Path, data: &[u8]) -> io::Result<()> {
    if path.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path is a directory"));
    }
    // Write through a symlink rather than replacing the link itself.
    let target = match fs::canonicalize(path) {
        Ok(resolved) => resolved,
        Err(err) if err.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(err) => return Err(err),
    };
    let existing = fs::metadata(&target).ok();
    if existing.as_ref().is_some_and(|meta| meta.permissions().readonly()) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "file is read-only"));
    }
    let file_name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let dir = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let mut temp_name = OsString::from(".");
    temp_name.push(file_name);
    temp_name.push(format!(".{}.tmp", std::process::id()));
    let temp = dir.join(temp_name);

    let written = (|| {
        let mut file = File::create(&temp)?;
        if let Some(meta) = &existing {
            file.set_permissions(meta.permissions())?;
        }
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written?;

    if fs::read(&target)? != data {
        return Err(io::Error::other("read-back after saving does not match the image"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(n: u8) -> Page {
        Page::new(n).unwrap()
    }

    fn slot(n: u8) -> Slot {
        Slot::new(n).unwrap()
    }

    #[test]
    fn frequency_addresses_match_confirmed_locations() {
        let r = Region::BankA;
        assert_eq!(frequency_address(r, page(1), slot(1)), 0x0840);
        assert_eq!(frequency_address(r, page(1), slot(9)), 0x0850);
        assert_eq!(frequency_address(r, page(48), slot(1)), 0x0E20);
        assert_eq!(frequency_address(r, page(49), slot(1)), 0x0E40);
        assert_eq!(frequency_address(r, page(49), slot(3)), 0x0E44);
        assert_eq!(frequency_address(Region::BankB, page(1), slot(1)), 0x1840);
        assert_eq!(frequency_address(Region::BankC, page(49), slot(9)), 0x2E50);
    }

    #[test]
    fn page_name_addresses_follow_three_per_record() {
        let r = Region::BankA;
        assert_eq!(page_name_address(r, page(1)), 0x0300);
        assert_eq!(page_name_address(r, page(2)), 0x030A);
        assert_eq!(page_name_address(r, page(3)), 0x0314);
        assert_eq!(page_name_address(r, page(4)), 0x0320);
        assert_eq!(page_name_address(r, page(5)), 0x032A);
        assert_eq!(page_name_address(r, page(49)), 0x0500);
        assert_eq!(page_name_address(Region::BankC, page(4)), 0x2320);
    }

    #[test]
    fn every_field_lies_inside_the_image() {
        for region in Region::ALL {
            for p in Page::all() {
                assert!(page_name_address(region, p) + NAME_LEN <= IMAGE_LEN);
                for s in Slot::all() {
                    assert!(frequency_address(region, p, s) + 2 <= IMAGE_LEN);
                }
            }
        }
    }

    #[test]
    fn page_and_slot_numbers_are_range_checked() {
        assert_eq!(Page::new(0), Err(EditError::PageOutOfRange(0)));
        assert_eq!(Page::new(50), Err(EditError::PageOutOfRange(50)));
        assert_eq!(Slot::new(0), Err(EditError::SlotOutOfRange(0)));
        assert_eq!(Slot::new(10), Err(EditError::SlotOutOfRange(10)));
        assert_eq!(Page::all().count(), 49);
        assert_eq!(Slot::all().count(), 9);
    }

    #[test]
    fn frequency_input_is_validated() {
        assert_eq!(parse_frequency_khz("9999"), Ok(9999));
        assert_eq!(parse_frequency_khz("  9765 \t"), Ok(9765));
        assert_eq!(parse_frequency_khz("1711"), Ok(1711));
        assert_eq!(parse_frequency_khz("29999"), Ok(29999));
        assert_eq!(parse_frequency_khz(""), Err(EditError::EmptyFrequency));
        assert_eq!(parse_frequency_khz("   "), Err(EditError::EmptyFrequency));
        assert_eq!(parse_frequency_khz("9.765"), Err(EditError::NotWholeKhz));
        assert_eq!(parse_frequency_khz("9,765"), Err(EditError::NotWholeKhz));
        assert_eq!(parse_frequency_khz("abc"), Err(EditError::NotANumber));
        assert_eq!(parse_frequency_khz("-5"), Err(EditError::NotANumber));
        assert_eq!(parse_frequency_khz("+9999"), Err(EditError::NotANumber));
        assert_eq!(parse_frequency_khz("9999kHz"), Err(EditError::NotANumber));
        assert_eq!(parse_frequency_khz("١٢٣٤"), Err(EditError::NotANumber));
        for out_of_range in ["0", "1710", "30000", "65535", "65536", "99999999999999999999"] {
            assert_eq!(
                parse_frequency_khz(out_of_range),
                Err(EditError::FrequencyOutOfRange(out_of_range.to_owned())),
                "{out_of_range}"
            );
        }
    }

    #[test]
    fn page_names_are_validated_and_padded() {
        assert_eq!(encode_page_name("BBC 1"), Ok(*b"BBC 1     "));
        assert_eq!(encode_page_name("ABCDEFGHIJ"), Ok(*b"ABCDEFGHIJ"));
        assert_eq!(encode_page_name("49 meter"), Ok(*b"49 meter  "));
        assert_eq!(encode_page_name(" A "), Ok(*b" A        "));
        assert_eq!(encode_page_name(""), Ok(*b"          "));
        assert_eq!(encode_page_name("ABCDEFGHIJK"), Err(EditError::NameTooLong { chars: 11 }));
        assert_eq!(encode_page_name("RAD.ROPA"), Err(EditError::NameInvalidChar('.')));
        assert_eq!(encode_page_name("BBC@1"), Err(EditError::NameInvalidChar('@')));
        assert_eq!(encode_page_name("RÄDIO"), Err(EditError::NameInvalidChar('Ä')));
        assert_eq!(encode_page_name("TAB\tX"), Err(EditError::NameInvalidChar('\t')));
    }

    #[test]
    fn name_text_shows_names_as_the_radio_does() {
        assert_eq!(name_text(b"BBC@1@@@@@"), "BBC 1     ");
        assert_eq!(name_text(b"90 meter  "), "90 meter  ");
        assert_eq!(name_text(b"RAD.ROPA  "), "RAD.ROPA  ");
        // A zero byte reads as a blank: the radio zeroes the name of a preset stored without one.
        assert_eq!(name_text(&[b'A', 0x00, 0xFF, b' ', 0x7F, b'Z', b' ', b' ', b' ', b' ']), "A · ·Z    ");
        assert_eq!(name_text(&[0; NAME_LEN]), "          ");
    }

    #[test]
    fn invalid_lengths_are_rejected() {
        for len in [0, 1, IMAGE_LEN - 1, IMAGE_LEN + 1, 0x4000] {
            let err = EepromImage::from_bytes(&vec![0; len]).unwrap_err();
            assert!(matches!(err, ImageError::InvalidLength { actual } if actual == len as u64));
        }
        assert!(EepromImage::from_bytes(&[0; IMAGE_LEN]).is_ok());
    }

    #[test]
    fn edits_track_modification_and_revert() {
        let mut image = EepromImage::from_bytes(&[0; IMAGE_LEN]).unwrap();
        let (r, p, s) = (Region::BankB, page(2), slot(3));
        assert!(!image.is_modified());
        image.set_frequency_khz(r, p, s, 15400).unwrap();
        assert!(image.is_modified());
        assert!(image.is_slot_modified(r, p, s));
        assert!(image.is_page_modified(r, p));
        assert!(image.is_region_modified(r));
        assert!(!image.is_region_modified(Region::BankA));
        assert_eq!(image.modified_byte_count(), 2);
        assert_eq!(image.saved_frequency_khz(r, p, s), None);
        image.revert_slot(r, p, s);
        assert!(!image.is_modified());

        image.set_page_name(r, p, "X").unwrap();
        assert!(image.is_page_name_modified(r, p));
        image.revert_page_name(r, p);
        assert!(!image.is_modified());
    }

    #[test]
    fn refused_edits_leave_bytes_untouched() {
        let mut image = EepromImage::from_bytes(&[0xAB; IMAGE_LEN]).unwrap();
        let (r, p, s) = (Region::BankA, page(1), slot(1));
        assert!(image.set_frequency_khz(r, p, s, 0).is_err());
        assert!(image.set_frequency_khz(r, p, s, 1710).is_err());
        assert!(image.set_frequency_khz(r, p, s, 30000).is_err());
        assert!(image.set_page_name(r, p, "TOO LONG NAME").is_err());
        assert!(image.set_page_name(r, p, "R.N").is_err());
        assert!(!image.is_modified());
        assert!(image.as_bytes().iter().all(|&b| b == 0xAB));
    }
}

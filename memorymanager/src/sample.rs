//! A made-up image for trying the editor without a radio, and for tests.
//!
//! The bytes are built here from the documented layout (`MEMORYMAP.md`); they
//! are not a copy of any radio's memory. [`demo`] cannot be written to a
//! radio: it carries a marker where a real image has the radio's product
//! string, and the editor refuses to send such an image.

use crate::image::{EepromImage, IMAGE_LEN, NAME_LEN, PRODUCT_STRING, PRODUCT_STRING_AT};

/// What [`demo`] holds in place of the product string.
pub const DEMO_MARKER: &[u8; 16] = b"NOT A RADIO DUMP";

const BANKS: [usize; 3] = [0x0000, 0x1000, 0x2000];
const SW_NAMES: usize = 0x0300;
const SW_PAGES: usize = 0x0840;

/// SW page names as a European radio has them from the factory, pages 3–49.
const PAGE_NAMES: [&str; 47] = [
    "BR", "DLF", "RAD.ROPA", "BBC -D-", "BBC -UK-", "RFI -D-", "RFI", "RADIO_NL", "R.VLAAND",
    "SCHWEDEN", "FINNLAND", "ROI_WIEN", "SRI_BERN", "RAI_ROM", "REE_MADR", "VATIKAN", "R_SLOWAK",
    "R_POLEN", "BUDAPEST", "RUSSLAND", "RUSSLAND", "R_KAIRO", "R_CHINA", "R_JAPAN", "R_BRASIL",
    "VOICE_AM", "WSHB", "120meter", "90 meter", "75 meter", "60 meter", "49 meter", "41 meter",
    "31 meter", "25 meter", "21 meter", "19 meter", "PAGE 40", "PAGE 41", "PAGE 42", "PAGE 43",
    "PAGE 44", "PAGE 45", "PAGE 46", "PAGE 47", "PAGE 48", "PAGE 49",
];

/// Broadcast bands the made-up SW presets are drawn from, in kHz.
const SW_BANDS: [(u16, u16); 8] = [
    (5900, 6200),
    (7200, 7450),
    (9400, 9900),
    (11600, 12100),
    (13570, 13870),
    (15100, 15800),
    (17480, 17900),
    (21450, 21850),
];

fn put_word(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_name(bytes: &mut [u8], at: usize, name: &[u8]) {
    bytes[at..at + NAME_LEN].fill(b' ');
    bytes[at..at + name.len()].copy_from_slice(name);
}

fn sw_name_at(base: usize, page: usize) -> usize {
    base + SW_NAMES + 32 * ((page - 1) / 3) + NAME_LEN * ((page - 1) % 3)
}

fn put_page(bytes: &mut [u8], base: usize, page: usize, presets: &[u16]) {
    for (slot, &khz) in presets.iter().enumerate() {
        put_word(bytes, base + SW_PAGES + 32 * (page - 1) + 2 * slot, khz);
    }
}

/// An image shaped like a radio's export, with the product string of a real
/// one. For tests of the write path; the editor itself only offers [`demo`].
pub fn radio_like() -> Vec<u8> {
    let mut bytes = vec![0u8; IMAGE_LEN];
    for base in BANKS {
        // Name records: three blank ten-byte names, then two zero bytes.
        for record in (0x0000..0x0760).step_by(32) {
            bytes[base + record..base + record + 30].fill(b' ');
        }
        for (index, name) in PAGE_NAMES.iter().enumerate() {
            put_name(&mut bytes, sw_name_at(base, index + 3), name.as_bytes());
        }
        // Factory presets at the start of FM, MW, LW and AIR.
        for (table, low, high) in [(0x0760, 8750, 10800), (0x07C0, 522, 1602), (0x0820, 153, 279), (0x0E60, 8000, 27000)] {
            put_word(&mut bytes, base + table, low);
            put_word(&mut bytes, base + table + 2, high);
        }
        // Made-up SW presets on pages 3–46, the same in every bank.
        for page in 3..=46usize {
            let count = 1 + (page * 5) % 9;
            let presets: Vec<u16> = (0..count)
                .map(|slot| {
                    let (low, high) = SW_BANDS[(page + slot) % SW_BANDS.len()];
                    low + ((page * 13 + slot * 7) as u16 % ((high - low) / 5 + 1)) * 5
                })
                .collect();
            put_page(&mut bytes, base, page, &presets);
        }
    }

    // MemoBank A: two pages renamed on the radio (blanks as 0x40) and a few presets.
    put_name(&mut bytes, sw_name_at(0, 1), b"BBC@1@@@@@");
    put_name(&mut bytes, sw_name_at(0, 2), b"BBC@2@@@@@");
    put_page(&mut bytes, 0, 1, &[9410, 9915, 17700, 17780, 21630, 15400, 15420, 17690, 15490]);
    put_page(&mut bytes, 0, 2, &[12095, 21565, 11645]);
    put_page(&mut bytes, 0, 47, &[11600]);
    put_page(&mut bytes, 0, 48, &[9780]);
    put_page(&mut bytes, 0, 49, &[9765, 9770, 9775]);
    // MemoBanks B and C keep the factory pages 1 and 2.
    for base in [0x1000, 0x2000] {
        put_name(&mut bytes, sw_name_at(base, 1), b"DW");
        put_name(&mut bytes, sw_name_at(base, 2), b"SWF_3");
        put_page(&mut bytes, base, 1, &[3995, 6075, 9545, 11795, 13690, 13780, 15275, 17560, 21560]);
        put_page(&mut bytes, base, 2, &[7265]);
        put_page(&mut bytes, base, 47, &[11600]);
        put_page(&mut bytes, base, 48, &[13500]);
        put_page(&mut bytes, base, 49, &[15100]);
    }

    // MemoBank A's settings block (MEMORYMAP.md, section 6).
    bytes[0x0F07..0x0F0A].copy_from_slice(&[0x04, 0x21, 0x04]);
    bytes[0x0F10..0x0F1F].fill(b' ');
    bytes[0x0F20..0x0F30].fill(b' ');
    bytes[0x0F36] = 21; // world-time city
    bytes[0x0F37] = 8; // local-time city
    for entry in (0x0F40..0x0F50).step_by(4) {
        bytes[entry..entry + 4].copy_from_slice(&[0x10, 0x27, 0x00, 0x01]);
    }
    bytes[PRODUCT_STRING_AT..PRODUCT_STRING_AT + PRODUCT_STRING.len()].copy_from_slice(PRODUCT_STRING);
    // The end of MemoBank B's range reads as erased memory.
    bytes[0x1F00..0x2000].fill(0xFF);
    bytes
}

/// [`radio_like`] with MemoBank A page 49, slot 1 changed from 9765 to 9999 kHz.
pub fn edited_9999() -> Vec<u8> {
    let mut bytes = radio_like();
    put_word(&mut bytes, 0x0E40, 9999);
    bytes
}

/// [`edited_9999`] after presets were stored on the radio on every band, the
/// way the radio writes them: FM page 1 slot 3 (91.30 MHz, locked), FM page 4
/// slot 9, MW page 1 slot 3 and page 3 slot 9, LW slot 4, AIR page 1 slot 3
/// and page 5 slot 9, and SW page 47 slot 1 at 14200 kHz in USB. The radio
/// zeroes the name of a preset stored without one.
pub fn with_presets_on_every_band() -> Vec<u8> {
    let mut bytes = edited_9999();
    for (word_at, value, name_at) in [
        (0x0764, 0x8000 | 9130, 0x0014),
        (0x07A6, 9490, 0x0174),
        (0x07C4, 999, 0x01F4),
        (0x07F4, 1503, 0x02F4),
        (0x0826, 198, 0x0540),
        (0x0E64, 11_500, 0x0594),
        (0x0EB8, 25_900, 0x0754),
    ] {
        put_word(&mut bytes, word_at, value);
        bytes[name_at..name_at + NAME_LEN].fill(0);
    }
    put_word(&mut bytes, 0x0E00, 14_200);
    bytes[0x0E14] = 0xD8;
    bytes
}

/// The image the editor opens with `--demo`: [`with_presets_on_every_band`],
/// marked so that it cannot be written to a radio.
pub fn demo() -> EepromImage {
    let mut bytes = with_presets_on_every_band();
    bytes[PRODUCT_STRING_AT..PRODUCT_STRING_AT + DEMO_MARKER.len()].copy_from_slice(DEMO_MARKER);
    EepromImage::from_bytes(&bytes).expect("the sample has the right length")
}

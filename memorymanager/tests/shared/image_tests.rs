// Tests of the image library. This file is included by `tests/image.rs`,
// which supplies the images to test with: `original_path()`, `edited_path()`
// (MemoBank A page 49 slot 1 changed from 9765 to 9999 kHz),
// `before_markers_path()` and `markers_path()` (presets stored on every band).

use std::fs;
use std::path::PathBuf;

use memory_manager::image::{
    Band, EditError, EepromImage, IMAGE_LEN, ImageError, NAME_LEN, NameRef, Page, PresetRef, Region,
    Slot, all_names, all_presets, frequency_address, mode_label, name_text, page_name_address,
    parse_frequency,
};

fn page(n: u8) -> Page {
    Page::new(n).unwrap()
}

fn slot(n: u8) -> Slot {
    Slot::new(n).unwrap()
}

/// Offsets where two buffers differ.
fn diff(a: &[u8], b: &[u8]) -> Vec<usize> {
    assert_eq!(a.len(), b.len());
    (0..a.len()).filter(|&i| a[i] != b[i]).collect()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        // A counter keeps names unique when parallel tests share a timestamp.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "memory-manager-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn fixtures_have_the_expected_shape() {
    let original = fs::read(original_path()).unwrap();
    let edited = fs::read(edited_path()).unwrap();
    assert_eq!(original.len(), IMAGE_LEN);
    assert_eq!(edited.len(), IMAGE_LEN);
    assert_eq!(diff(&original, &edited), vec![0x0E40, 0x0E41]);
}

#[test]
fn open_and_save_without_edits_reproduces_every_byte() {
    let original = fs::read(original_path()).unwrap();
    let mut image = EepromImage::load(&original_path()).unwrap();
    assert!(!image.is_modified());
    assert_eq!(image.as_bytes(), &original[..]);

    let dir = TempDir::new("roundtrip");
    let out = dir.path("copy.bin");
    image.save(&out).unwrap();
    assert_eq!(fs::read(&out).unwrap(), original);
    assert!(!image.is_modified());
}

#[test]
fn decodes_confirmed_presets_and_names() {
    let image = EepromImage::load(&original_path()).unwrap();
    let r = Region::BankA;
    let page1: Vec<_> = Slot::all().map(|s| image.frequency_khz(r, page(1), s)).collect();
    let expected = [9410, 9915, 17700, 17780, 21630, 15400, 15420, 17690, 15490];
    assert_eq!(page1, expected.map(Some).to_vec());
    assert_eq!(image.frequency_khz(r, page(48), slot(1)), Some(9780));
    assert_eq!(image.frequency_khz(r, page(48), slot(2)), None);
    assert_eq!(image.frequency_khz(r, page(49), slot(1)), Some(9765));
    assert_eq!(image.frequency_khz(r, page(49), slot(2)), Some(9770));
    assert_eq!(image.frequency_khz(r, page(49), slot(3)), Some(9775));
    assert_eq!(image.frequency_khz(r, page(49), slot(4)), None);
    assert_eq!(image.used_slots(r, page(49)), 3);

    assert_eq!(&image.page_name(r, page(1)), b"BBC@1@@@@@");
    assert_eq!(&image.page_name(r, page(2)), b"BBC@2@@@@@");
    assert_eq!(&image.page_name(r, page(3)), b"BR        ");
    assert_eq!(&image.page_name(r, page(4)), b"DLF       ");
    assert_eq!(&image.page_name(r, page(5)), b"RAD.ROPA  ");
    assert_eq!(&image.page_name(r, page(49)), b"PAGE 49   ");
    assert_eq!(&image.page_name(Region::BankB, page(1)), b"DW        ");
    assert_eq!(&image.page_name(Region::BankC, page(2)), b"SWF_3     ");
}

#[test]
fn editing_page_49_slot_1_reproduces_the_edited_fixture() {
    let original = fs::read(original_path()).unwrap();
    let edited = fs::read(edited_path()).unwrap();
    let mut image = EepromImage::load(&original_path()).unwrap();
    let (r, p, s) = (Region::BankA, page(49), slot(1));

    assert_eq!(image.frequency_khz(r, p, s), Some(9765));
    image.set_frequency_khz(r, p, s, 9999).unwrap();
    assert_eq!(image.frequency_khz(r, p, s), Some(9999));
    assert_eq!(image.saved_frequency_khz(r, p, s), Some(9765));
    assert!(image.is_modified());
    assert_eq!(image.modified_byte_count(), 2);

    assert_eq!(image.as_bytes(), &edited[..]);
    let changed = diff(&original, image.as_bytes());
    assert_eq!(changed, vec![0x0E40, 0x0E41]);
    assert_eq!(&original[0x0E40..0x0E42], &[0x25, 0x26]);
    assert_eq!(&image.as_bytes()[0x0E40..0x0E42], &[0x0F, 0x27]);

    let dir = TempDir::new("edit9999");
    let out = dir.path("edited.bin");
    image.save(&out).unwrap();
    assert_eq!(fs::read(&out).unwrap(), edited);
    assert!(!image.is_modified());
    // The loaded source file is never touched by edits or by saving elsewhere.
    assert_eq!(fs::read(original_path()).unwrap(), original);
}

#[test]
fn reverting_the_edit_restores_the_original_fixture() {
    let original = fs::read(original_path()).unwrap();
    let mut image = EepromImage::load(&edited_path()).unwrap();
    image.set_frequency_khz(Region::BankA, page(49), slot(1), 9765).unwrap();
    assert_eq!(image.as_bytes(), &original[..]);
}

#[test]
fn clearing_a_slot_writes_zero_and_nothing_else() {
    let original = fs::read(original_path()).unwrap();
    let mut image = EepromImage::load(&original_path()).unwrap();
    let (r, p, s) = (Region::BankA, page(49), slot(2));
    image.clear_slot(r, p, s);
    assert_eq!(image.frequency_khz(r, p, s), None);
    assert_eq!(diff(&original, image.as_bytes()), vec![0x0E42, 0x0E43]);
    assert_eq!(&image.as_bytes()[0x0E42..0x0E44], &[0, 0]);
    assert_eq!(image.used_slots(r, p), 2);
}

#[test]
fn regions_are_edited_independently() {
    let original = fs::read(original_path()).unwrap();
    let mut image = EepromImage::load(&original_path()).unwrap();
    image.set_frequency_khz(Region::BankB, page(1), slot(1), 6085).unwrap();
    image.set_frequency_khz(Region::BankC, page(49), slot(9), 29999).unwrap();
    let changed = diff(&original, image.as_bytes());
    assert_eq!(changed, vec![0x1840, 0x1841, 0x2E50, 0x2E51]);
    assert_eq!(&image.as_bytes()[0x1840..0x1842], &6085u16.to_le_bytes());
    assert_eq!(&image.as_bytes()[0x2E50..0x2E52], &29999u16.to_le_bytes());
    assert_eq!(image.frequency_khz(Region::BankA, page(1), slot(1)), Some(9410));
}

#[test]
fn renaming_pages_3_and_4_stays_inside_their_fields() {
    let original = fs::read(original_path()).unwrap();
    let mut image = EepromImage::load(&original_path()).unwrap();
    let r = Region::BankA;
    // Page 3 is the last field in the record at 0x0300, page 4 the first in the record at 0x0320.
    assert_eq!(page_name_address(r, page(3)), 0x0314);
    assert_eq!(page_name_address(r, page(4)), 0x0320);
    // Record padding between them.
    assert_eq!(&original[0x031E..0x0320], &[0x00, 0x00]);

    image.set_page_name(r, page(3), "ABCDEFGHIJ").unwrap();
    image.set_page_name(r, page(4), "Z9").unwrap();

    let bytes = image.as_bytes();
    assert_eq!(&bytes[0x0314..0x031E], b"ABCDEFGHIJ");
    assert_eq!(&bytes[0x031E..0x0320], &[0x00, 0x00]);
    assert_eq!(&bytes[0x0320..0x032A], b"Z9        ");
    assert_eq!(&bytes[0x030A..0x0314], b"BBC@2@@@@@");
    assert_eq!(&bytes[0x032A..0x0334], b"RAD.ROPA  ");

    let changed = diff(&original, bytes);
    assert!(changed.iter().all(|&a| (0x0314..0x031E).contains(&a) || (0x0320..0x032A).contains(&a)));
    // "BR" + 8 spaces → "ABCDEFGHIJ" changes all ten; "DLF" + 7 spaces → "Z9" + 8 spaces changes three.
    assert_eq!(changed.len(), 10 + 3);
    assert!(image.is_page_name_modified(r, page(3)));
    assert!(image.is_page_name_modified(r, page(4)));
    assert!(!image.is_page_name_modified(r, page(2)));
    assert!(!image.is_page_name_modified(r, page(5)));
}

#[test]
fn renaming_a_radio_edited_name_writes_ascii_spaces() {
    // The radio stored "BBC 1" as BBC@1@@@@@. Renaming it writes 0x20 blanks; it reads the same.
    let original = fs::read(original_path()).unwrap();
    let mut image = EepromImage::load(&original_path()).unwrap();
    image.set_page_name(Region::BankA, page(1), "BBC 1").unwrap();
    assert_eq!(&image.page_name(Region::BankA, page(1)), b"BBC 1     ");
    assert_eq!(name_text(&image.page_name(Region::BankA, page(1))), "BBC 1     ");
    // Only the six 0x40 bytes become 0x20.
    let changed = diff(&original, image.as_bytes());
    assert_eq!(changed, vec![0x0303, 0x0305, 0x0306, 0x0307, 0x0308, 0x0309]);
    assert!(changed.iter().all(|&a| original[a] == 0x40 && image.as_bytes()[a] == 0x20));
}

#[test]
fn renaming_the_last_page_leaves_blank_fields_and_padding() {
    let original = fs::read(original_path()).unwrap();
    let mut image = EepromImage::load(&original_path()).unwrap();
    image.set_page_name(Region::BankC, page(49), "TEST").unwrap();
    let at = page_name_address(Region::BankC, page(49));
    assert_eq!(at, 0x2500);
    assert_eq!(&image.as_bytes()[at..at + NAME_LEN], b"TEST      ");
    assert!(diff(&original, image.as_bytes()).iter().all(|&a| (0x2500..0x250A).contains(&a)));
    assert_eq!(&image.as_bytes()[0x250A..0x2520], &original[0x250A..0x2520]);
}

#[test]
fn rejected_input_leaves_the_image_unchanged() {
    let original = fs::read(original_path()).unwrap();
    let mut image = EepromImage::load(&original_path()).unwrap();
    let (r, p, s) = (Region::BankA, page(5), slot(1));
    assert!(image.set_frequency_khz(r, p, s, 0).is_err());
    assert!(image.set_frequency_khz(r, p, s, 30000).is_err());
    assert!(image.set_page_name(r, p, "RAD,ROPA").is_err());
    assert!(image.set_page_name(r, p, "ELEVEN CHAR").is_err());
    assert!(image.set_page_name(r, p, "ÅÄÖ").is_err());
    assert!(!image.is_modified());
    assert_eq!(image.as_bytes(), &original[..]);
}

#[test]
fn invalid_files_are_rejected_cleanly() {
    let dir = TempDir::new("invalid");
    let original = fs::read(original_path()).unwrap();
    for len in [0, 100, IMAGE_LEN - 1, IMAGE_LEN + 1, 0x4000] {
        let path = dir.path(&format!("len-{len}.bin"));
        let mut data = original.clone();
        data.resize(len, 0xFF);
        fs::write(&path, &data).unwrap();
        match EepromImage::load(&path) {
            Err(ImageError::InvalidLength { actual }) => assert_eq!(actual, len as u64),
            other => panic!("length {len}: expected InvalidLength, got {other:?}"),
        }
    }
    assert!(matches!(
        EepromImage::load(&dir.path("missing.bin")),
        Err(ImageError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound
    ));
    assert!(matches!(EepromImage::load(&dir.0), Err(ImageError::NotAFile)));
}

#[test]
fn failed_save_keeps_unsaved_changes() {
    let dir = TempDir::new("savefail");
    let mut image = EepromImage::load(&original_path()).unwrap();
    image.clear_slot(Region::BankA, page(1), slot(1));

    assert!(image.save(&dir.path("no-such-dir/out.bin")).is_err());
    assert!(image.save(&dir.0).is_err());
    assert!(image.is_modified());
    // No temporary files are left behind.
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
}

#[test]
fn saving_over_an_existing_file_replaces_it() {
    let dir = TempDir::new("overwrite");
    let out = dir.path("target.bin");
    fs::write(&out, b"old contents").unwrap();
    let mut image = EepromImage::load(&original_path()).unwrap();
    image.set_frequency_khz(Region::BankA, page(49), slot(1), 9999).unwrap();
    image.save(&out).unwrap();
    assert_eq!(fs::read(&out).unwrap(), fs::read(edited_path()).unwrap());
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
}

#[test]
fn addresses_are_consistent_with_the_fixture_tables() {
    // Every name field in all three regions holds printable ASCII in the capture.
    let image = EepromImage::load(&original_path()).unwrap();
    for region in Region::ALL {
        for p in Page::all() {
            let name = image.page_name(region, p);
            assert!(name.iter().all(|b| (0x20..=0x7E).contains(b)), "{region:?} page {}", p.number());
        }
        // Each 32-byte name record ends in two zero bytes.
        for record in 0..17 {
            let pad = region.base() + 0x0300 + 0x20 * record + 0x1E;
            assert_eq!(&image.as_bytes()[pad..pad + 2], &[0, 0]);
        }
        // Frequency records are 18 bytes of slots followed by a zero tail.
        for p in Page::all() {
            let tail = frequency_address(region, p, slot(9)) + 2;
            assert!(image.as_bytes()[tail..tail + 14].iter().all(|&b| b == 0));
        }
    }
}

#[test]
fn new_image_clears_presets_and_names_and_keeps_everything_else() {
    let original = fs::read(original_path()).unwrap();
    let blank = EepromImage::load(&original_path()).unwrap().cleared();
    assert!(!blank.is_modified());
    assert!(blank.has_product_string());

    // Byte ranges New is allowed to change, in each of the three banks: the
    // words of all 558 presets, the mode bytes of the 441 SW and 27 MW
    // presets, and the 49 page names and 117 preset names.
    let mut field = vec![false; IMAGE_LEN];
    for at in all_presets() {
        field[at.address()..at.address() + 2].fill(true);
        if let Some(mode) = at.mode_address() {
            field[mode] = true;
        }
        assert_eq!(blank.frequency(at), None, "{at:?}");
        assert!(!blank.is_locked(at));
    }
    for name in all_names() {
        field[name.address()..name.address() + NAME_LEN].fill(true);
        assert_eq!(&blank.name(name), b"          ", "{name:?}");
    }
    assert_eq!(field.iter().filter(|&&f| f).count(), 3 * (558 * 2 + 441 + 27 + (49 + 117) * 10));
    // The factory presets at the start of FM, MW, LW and AIR are gone too.
    assert_eq!(&original[0x0760..0x0764], &[0x2E, 0x22, 0x30, 0x2A], "87.50 and 108.00 MHz in the export");
    assert_eq!(&blank.as_bytes()[0x0760..0x0764], &[0; 4]);
    for (at, (&new, &old)) in blank.as_bytes().iter().zip(&original).enumerate() {
        if !field[at] {
            assert_eq!(new, old, "byte 0x{at:04X} outside the edited fields changed");
        }
    }
    // Spot checks: record padding, the product string and the erased-flash block survive.
    assert_eq!(&blank.as_bytes()[0x031E..0x0320], &[0, 0]);
    assert_eq!(&blank.as_bytes()[0x0F50..0x0F60], b"SANGEANATS-909X2");
    assert!(blank.as_bytes()[0x1F00..0x2000].iter().all(|&b| b == 0xFF));
    // The two unused name fields after page 49 were already blank and are untouched.
    assert_eq!(&blank.as_bytes()[0x050A..0x0520], &original[0x050A..0x0520]);
}

#[test]
fn new_image_round_trips_through_save_and_load() {
    let dir = TempDir::new("blank");
    let out = dir.path("new.bin");
    let mut blank = EepromImage::load(&original_path()).unwrap().cleared();
    blank.set_frequency_khz(Region::BankB, page(7), slot(4), 15400).unwrap();
    blank.save(&out).unwrap();
    let reloaded = EepromImage::load(&out).unwrap();
    assert_eq!(reloaded.as_bytes(), blank.as_bytes());
    assert_eq!(reloaded.frequency_khz(Region::BankB, page(7), slot(4)), Some(15400));
}

#[test]
fn compare_lists_changed_fields_and_other_bytes() {
    let original = EepromImage::load(&original_path()).unwrap();
    let edited = EepromImage::load(&edited_path()).unwrap();
    let diff = edited.compare(&original);
    assert_eq!(diff.presets, vec![PresetRef::sw(Region::BankA, page(49), slot(1))]);
    assert!(diff.names.is_empty());
    assert_eq!(diff.other_bytes, 0);
    assert!(original.compare(&original).is_empty());

    let blank = original.cleared();
    let diff = blank.compare(&original);
    // Preset names outside SW were already blank in the export.
    assert_eq!(diff.names.len(), 3 * 49);
    // Among the cleared presets: two factory presets on each of FM, MW, LW and AIR, per bank.
    let outside_sw = diff.presets.iter().filter(|at| at.band != Band::Sw).count();
    assert_eq!(outside_sw, 3 * 4 * 2);
    assert_eq!(diff.other_bytes, 0, "New never touches bytes outside the known fields");

    let mut bytes = fs::read(original_path()).unwrap();
    bytes[0x0F50] = b'X'; // product string
    bytes[0x0E52] ^= 1; // record tail after page 49's slots
    let foreign = EepromImage::from_bytes(&bytes).unwrap();
    assert_eq!(foreign.compare(&original).other_bytes, 2);
}

// ---- FM, MW, LW and AIR: the marker presets of 2026-11-13 (MEMORYMAP.md, section 8) ----

/// The radio as read after seven presets were stored on FM, MW, LW and AIR
/// (FM page 1 slot 3 locked), and SW page 47 slot 1 was set to 14200 kHz USB.
fn markers() -> EepromImage {
    EepromImage::load(&markers_path()).unwrap()
}

fn preset(band: Band, p: u8, s: u8) -> PresetRef {
    PresetRef::new(Region::BankA, band, page(p), slot(s)).unwrap()
}

#[test]
fn marker_presets_decode_on_every_band() {
    let image = markers();
    // (band, page, slot, kHz, address of the word, address of the name)
    let expected = [
        (Band::Fm, 1, 3, 91_300, 0x0764, 0x0014),
        (Band::Fm, 4, 9, 94_900, 0x07A6, 0x0174),
        (Band::Mw, 1, 3, 999, 0x07C4, 0x01F4),
        (Band::Mw, 3, 9, 1503, 0x07F4, 0x02F4),
        (Band::Lw, 1, 4, 198, 0x0826, 0x0540),
        (Band::Air, 1, 3, 121_500, 0x0E64, 0x0594),
        (Band::Air, 5, 9, 135_900, 0x0EB8, 0x0754),
    ];
    for (band, p, s, khz, address, name_address) in expected {
        let at = preset(band, p, s);
        assert_eq!(image.frequency(at), Some(khz), "{band:?} {p}/{s}");
        assert_eq!(at.address(), address, "{band:?} {p}/{s}");
        let name = at.name().expect("presets outside SW have names");
        assert_eq!(name.address(), name_address, "{band:?} {p}/{s}");
        // The radio zeroed the name when the preset was stored; it reads as blank.
        assert_eq!(image.name(name), [0; NAME_LEN]);
        assert_eq!(name_text(&image.name(name)), " ".repeat(NAME_LEN));
        // Only MW of these bands has mode bytes; the markers were stored in AM.
        match band {
            Band::Mw => assert_eq!(image.mode(at), Some(0)),
            _ => assert_eq!(at.mode_address(), None),
        }
    }
    // Only FM page 1 slot 3 was locked: bit 15 of its word.
    assert!(image.is_locked(preset(Band::Fm, 1, 3)));
    assert_eq!(&image.as_bytes()[0x0764..0x0766], &[0xAA, 0xA3]);
    assert!(!image.is_locked(preset(Band::Fm, 4, 9)));

    // The factory presets are still in positions 1 and 2 of each first page.
    for (band, low, high) in [
        (Band::Fm, 87_500, 108_000),
        (Band::Mw, 522, 1602),
        (Band::Lw, 153, 279),
        (Band::Air, 118_000, 137_000),
    ] {
        assert_eq!(image.frequency(preset(band, 1, 1)), Some(low));
        assert_eq!(image.frequency(preset(band, 1, 2)), Some(high));
        assert_eq!(image.used_presets(Region::BankA, band, page(1)), 3);
    }
    assert_eq!(image.frequency(preset(Band::Fm, 2, 1)), None);
}

#[test]
fn sw_preset_stored_in_usb_has_a_mode_byte() {
    let image = markers();
    let at = PresetRef::sw(Region::BankA, page(47), slot(1));
    assert_eq!(at.address(), 0x0E00);
    assert_eq!(at.mode_address(), Some(0x0E14));
    assert_eq!(image.frequency(at), Some(14_200));
    assert_eq!(image.mode(at), Some(0xD8));
    assert_eq!(mode_label(0xD8), "USB");
    assert_eq!(mode_label(0x98), "LSB");
    assert_eq!(mode_label(0), "AM");
    assert_eq!(at.name(), None, "SW names pages, not presets");
    // The neighbour was stored in AM.
    assert_eq!(image.mode(PresetRef::sw(Region::BankA, page(47), slot(2))), Some(0));
}

#[test]
fn the_marker_experiment_differs_only_in_known_fields() {
    let before = EepromImage::load(&before_markers_path()).unwrap();
    let after = markers();
    let diff = after.compare(&before);
    assert_eq!(diff.presets.len(), 8, "seven markers and SW page 47 slot 1");
    assert_eq!(diff.names.len(), 7);
    assert_eq!(diff.other_bytes, 0);
    assert!(diff.presets.contains(&PresetRef::sw(Region::BankA, page(47), slot(1))));
    assert!(diff.names.contains(&NameRef::Preset(preset(Band::Air, 5, 9))));
}

#[test]
fn name_numbers_follow_the_memory_map() {
    assert_eq!(NameRef::SwPage(Region::BankA, page(1)).address(), 0x0300);
    assert_eq!(NameRef::SwPage(Region::BankB, page(49)).address(), page_name_address(Region::BankB, page(49)));
    assert_eq!(NameRef::Preset(preset(Band::Fm, 1, 1)).address(), 0x0000);
    assert_eq!(NameRef::Preset(preset(Band::Mw, 1, 1)).address(), 0x01E0);
    assert_eq!(NameRef::Preset(preset(Band::Lw, 1, 1)).address(), 0x0520);
    assert_eq!(NameRef::Preset(preset(Band::Air, 1, 1)).address(), 0x0580);
    let bank_c = PresetRef::new(Region::BankC, Band::Air, page(5), slot(9)).unwrap();
    assert_eq!(bank_c.address(), 0x2EB8);
    assert_eq!(NameRef::Preset(bank_c).address(), 0x2754);
    // Bands have their own page counts.
    for (band, pages) in [(Band::Fm, 4), (Band::Mw, 3), (Band::Lw, 1), (Band::Sw, 49), (Band::Air, 5)] {
        assert_eq!(band.pages(), pages);
        assert!(PresetRef::new(Region::BankA, band, page(pages), slot(9)).is_ok());
        if pages < 49 {
            assert_eq!(
                PresetRef::new(Region::BankA, band, page(pages + 1), slot(1)),
                Err(EditError::PageOutOfRange(pages + 1))
            );
        }
    }
}

#[test]
fn editing_other_bands_writes_the_stored_units_and_keeps_the_lock() {
    let mut image = markers();
    let clean = image.clone();

    // FM is stored in 10 kHz; a locked preset stays locked.
    let fm13 = preset(Band::Fm, 1, 3);
    image.set_frequency(fm13, 99_900).unwrap();
    assert_eq!(&image.as_bytes()[0x0764..0x0766], &(0x8000u16 | 9990).to_le_bytes());
    assert!(image.is_locked(fm13));
    assert_eq!(image.frequency(fm13), Some(99_900));

    // AIR is stored as kHz above 110 MHz.
    let air = preset(Band::Air, 2, 1);
    image.set_frequency(air, 127_500).unwrap();
    assert_eq!(air.address(), 0x0E60 + 18);
    assert_eq!(&image.as_bytes()[air.address()..air.address() + 2], &17_500u16.to_le_bytes());

    image.set_frequency(preset(Band::Mw, 2, 5), 1179).unwrap();
    image.set_frequency(preset(Band::Lw, 1, 9), 252).unwrap();
    assert_eq!(image.frequency(preset(Band::Mw, 2, 5)), Some(1179));
    assert_eq!(image.frequency(preset(Band::Lw, 1, 9)), Some(252));

    // Nothing else moved: four words, FM13's low byte and high byte among them.
    let changed: Vec<usize> = (0..IMAGE_LEN).filter(|&i| image.as_bytes()[i] != clean.as_bytes()[i]).collect();
    assert!(changed.len() <= 8);
    let diff = image.compare(&clean);
    assert_eq!(diff.presets.len(), 4);
    assert!(diff.names.is_empty());
    assert_eq!(diff.other_bytes, 0);

    // Out of band or off the tuning step is refused, and nothing is written.
    let before = image.clone();
    assert!(matches!(image.set_frequency(fm13, 63_950), Err(EditError::OutOfBand { band: Band::Fm, .. })));
    assert_eq!(image.set_frequency(fm13, 91_330), Err(EditError::OffStep(Band::Fm)));
    assert_eq!(image.set_frequency(air, 121_503), Err(EditError::OffStep(Band::Air)));
    assert!(matches!(image.set_frequency(preset(Band::Mw, 1, 1), 1711), Err(EditError::OutOfBand { .. })));
    assert!(image == before);
}

#[test]
fn locking_and_clearing_presets() {
    let mut image = markers();
    let fm13 = preset(Band::Fm, 1, 3);
    image.set_locked(fm13, false).unwrap();
    assert_eq!(&image.as_bytes()[0x0764..0x0766], &9130u16.to_le_bytes());
    image.set_locked(fm13, true).unwrap();
    assert_eq!(&image.as_bytes()[0x0764..0x0766], &[0xAA, 0xA3]);
    assert_eq!(image.set_locked(preset(Band::Fm, 2, 1), true), Err(EditError::EmptyPreset));

    // Clearing empties the word (lock included) and blanks the preset's name.
    image.set_name(NameRef::Preset(fm13), "FM13").unwrap();
    assert_eq!(&image.name(NameRef::Preset(fm13)), b"FM13      ");
    image.clear_preset(fm13);
    assert_eq!(image.frequency(fm13), None);
    assert!(!image.is_locked(fm13));
    assert_eq!(&image.name(NameRef::Preset(fm13)), b"          ");

    // On SW the mode byte goes with the preset; the page name stays.
    let usb = PresetRef::sw(Region::BankA, page(47), slot(1));
    let page_name = image.name(NameRef::SwPage(Region::BankA, page(47)));
    image.clear_preset(usb);
    assert_eq!(image.frequency(usb), None);
    assert_eq!(image.mode(usb), Some(0));
    assert_eq!(image.name(NameRef::SwPage(Region::BankA, page(47))), page_name);
    image.revert_preset(usb);
    assert_eq!((image.frequency(usb), image.mode(usb)), (Some(14_200), Some(0xD8)));
}

#[test]
fn a_locked_sw_preset_still_reads_as_its_frequency() {
    // The lock is bit 15 of the word on every band (confirmed on FM).
    let mut image = EepromImage::load(&original_path()).unwrap();
    let at = PresetRef::sw(Region::BankA, page(49), slot(1));
    image.set_locked(at, true).unwrap();
    assert_eq!(&image.as_bytes()[0x0E40..0x0E42], &(0x8000u16 | 9765).to_le_bytes());
    assert_eq!(image.frequency_khz(Region::BankA, page(49), slot(1)), Some(9765));
    image.set_frequency_khz(Region::BankA, page(49), slot(1), 9999).unwrap();
    assert!(image.is_locked(at), "editing the frequency keeps the lock");
    assert_eq!(image.frequency(at), Some(9999));
}

#[test]
fn frequencies_are_typed_in_the_bands_own_unit() {
    assert_eq!(parse_frequency(Band::Fm, "91.3"), Ok(91_300));
    assert_eq!(parse_frequency(Band::Fm, "91.30"), Ok(91_300));
    assert_eq!(parse_frequency(Band::Fm, "108"), Ok(108_000));
    assert_eq!(parse_frequency(Band::Fm, " 64.00 "), Ok(64_000));
    assert_eq!(parse_frequency(Band::Fm, "91.33"), Err(EditError::OffStep(Band::Fm)));
    assert_eq!(parse_frequency(Band::Fm, "91.305"), Err(EditError::FrequencyFormat(Band::Fm)));
    assert_eq!(parse_frequency(Band::Fm, "91,3"), Err(EditError::FrequencyFormat(Band::Fm)));
    assert_eq!(parse_frequency(Band::Fm, ".5"), Err(EditError::FrequencyFormat(Band::Fm)));
    assert!(matches!(parse_frequency(Band::Fm, "63.95"), Err(EditError::OutOfBand { .. })));
    assert!(matches!(parse_frequency(Band::Fm, "108.05"), Err(EditError::OutOfBand { .. })));
    assert_eq!(parse_frequency(Band::Fm, ""), Err(EditError::EmptyFrequency));

    assert_eq!(parse_frequency(Band::Air, "121.5"), Ok(121_500));
    assert_eq!(parse_frequency(Band::Air, "135.900"), Ok(135_900));
    assert_eq!(parse_frequency(Band::Air, "118.005"), Ok(118_005));
    assert_eq!(parse_frequency(Band::Air, "121.503"), Err(EditError::OffStep(Band::Air)));
    assert!(matches!(parse_frequency(Band::Air, "117.995"), Err(EditError::OutOfBand { .. })));
    assert!(matches!(parse_frequency(Band::Air, "137.005"), Err(EditError::OutOfBand { .. })));

    assert_eq!(parse_frequency(Band::Mw, "999"), Ok(999));
    assert!(matches!(parse_frequency(Band::Mw, "519"), Err(EditError::OutOfBand { .. })));
    assert_eq!(parse_frequency(Band::Mw, "99.9"), Err(EditError::FrequencyFormat(Band::Mw)));
    assert_eq!(parse_frequency(Band::Lw, "198"), Ok(198));
    assert!(matches!(parse_frequency(Band::Lw, "99"), Err(EditError::OutOfBand { .. })));
    // SW keeps its own messages.
    assert_eq!(parse_frequency(Band::Sw, "9765"), Ok(9765));
    assert_eq!(parse_frequency(Band::Sw, "9765.5"), Err(EditError::NotWholeKhz));

    assert_eq!(Band::Fm.frequency_text(91_300), "91.30 MHz");
    assert_eq!(Band::Air.frequency_text(121_500), "121.500 MHz");
    assert_eq!(Band::Mw.frequency_text(999), "999 kHz");
    assert_eq!(Band::Fm.range_text(), "64.00–108.00 MHz");
    assert_eq!(Band::Air.range_text(), "118.000–137.000 MHz");
    assert_eq!(
        EditError::OutOfBand { band: Band::Fm, input: "63.95".into() }.to_string(),
        "63.95 MHz is outside the FM range 64.00–108.00 MHz"
    );
    assert_eq!(EditError::OffStep(Band::Air).to_string(), "AIR frequencies go in steps of 5 kHz");
    assert_eq!(EditError::FrequencyFormat(Band::Fm).to_string(), "enter FM in MHz, for example 91.30");
}

#[test]
fn mw_presets_have_mode_bytes_at_0x0800() {
    let mw = |p: u8, s: u8| PresetRef::new(Region::BankA, Band::Mw, page(p), slot(s)).unwrap();
    assert_eq!(mw(1, 1).mode_address(), Some(0x0800));
    assert_eq!(mw(1, 4).mode_address(), Some(0x0803));
    assert_eq!(mw(3, 9).mode_address(), Some(0x081A));
    let bank_b = PresetRef::new(Region::BankB, Band::Mw, page(2), slot(1)).unwrap();
    assert_eq!(bank_b.mode_address(), Some(0x1809));
    // FM and AIR have no SSB; where LW keeps its modes is not known.
    for band in [Band::Fm, Band::Lw, Band::Air] {
        assert_eq!(PresetRef::new(Region::BankA, band, page(1), slot(1)).unwrap().mode_address(), None);
    }

    // A preset stored in USB, the way the radio writes it.
    let mut bytes = fs::read(original_path()).unwrap();
    bytes[0x07C6..0x07C8].copy_from_slice(&522u16.to_le_bytes());
    bytes[0x0803] = 0xD8;
    let clean = EepromImage::from_bytes(&fs::read(original_path()).unwrap()).unwrap();
    let mut image = EepromImage::from_bytes(&bytes).unwrap();
    assert_eq!(image.mode(mw(1, 4)), Some(0xD8));
    assert_eq!(mode_label(0xD8), "USB");

    // The mode byte counts as part of the preset, never as an unknown byte.
    let diff = image.compare(&clean);
    assert_eq!(diff.presets, vec![mw(1, 4)]);
    assert_eq!(diff.other_bytes, 0);

    // Editing the frequency keeps the mode; clearing empties it; New clears it.
    image.set_frequency(mw(1, 4), 531).unwrap();
    assert_eq!(image.mode(mw(1, 4)), Some(0xD8));
    assert!(image.cleared().as_bytes()[0x0803] == 0);
    image.clear_preset(mw(1, 4));
    assert_eq!((image.frequency(mw(1, 4)), image.mode(mw(1, 4))), (None, Some(0)));
    image.revert_preset(mw(1, 4));
    assert_eq!((image.frequency(mw(1, 4)), image.mode(mw(1, 4))), (Some(522), Some(0xD8)));
}

#[test]
fn menu_settings_read_change_and_compare() {
    use memory_manager::settings::Setting;
    let original = EepromImage::load(&original_path()).unwrap();
    let mut image = original.clone();
    let text = |image: &EepromImage, s: Setting| s.text(image.setting(s));
    // Every image so far had memory bank A, FM step 100 kHz, auto stereo,
    // soft mute off, scan delay 1.0 s and SSB step 20 Hz.
    for (setting, value) in [
        (Setting::MemoryBank, "A"),
        (Setting::FmStep, "100 kHz"),
        (Setting::FmStereo, "auto"),
        (Setting::SoftMute, "off"),
        (Setting::ScanDelay, "1.0 s"),
        (Setting::SsbStep, "20 Hz"),
    ] {
        assert_eq!(text(&image, setting), value, "{setting:?}");
    }

    // Changing one setting moves only its bits.
    let before = image.as_bytes()[0x0F09];
    image.set_setting(Setting::Backlight, 2).unwrap();
    assert_eq!(text(&image, Setting::Backlight), "30 s");
    assert_eq!(image.as_bytes()[0x0F09] & !0b1100, before & !0b1100);
    image.set_setting(Setting::SoftMute, 1).unwrap();
    assert!(image.is_setting_modified(Setting::SoftMute));
    assert_eq!(
        image.set_setting(Setting::ScanDelay, 2),
        Err(EditError::UnknownSettingValue(Setting::ScanDelay)),
        "1.5 s has not been confirmed"
    );

    // A write confirmation lists them as settings, not as unknown bytes.
    let diff = image.compare(&original);
    assert_eq!(diff.settings, vec![Setting::Backlight, Setting::SoftMute]);
    assert_eq!(diff.other_bytes, 0);
    assert!(diff.presets.is_empty() && diff.names.is_empty());

    // A bit no setting uses still counts as unknown.
    let mut bytes = original.as_bytes().to_vec();
    bytes[0x0F08] ^= 0b0100_0000;
    let odd = EepromImage::from_bytes(&bytes).unwrap();
    let diff = odd.compare(&original);
    assert!(diff.settings.is_empty());
    assert_eq!(diff.other_bytes, 1);

    // Reverting puts back only that setting's bits; New keeps the settings.
    image.revert_setting(Setting::Backlight);
    assert!(!image.is_setting_modified(Setting::Backlight));
    assert!(image.is_setting_modified(Setting::SoftMute));
    assert_eq!(image.cleared().setting(Setting::SoftMute), 1);

    // The timers as stored: all off at 00:00, FM 100.00 MHz.
    for number in 1..=3 {
        let timer = original.timer(number);
        assert_eq!((timer.time_text(), timer.state_text()), ("00:00".to_owned(), "off, buzzer".to_owned()));
        assert_eq!(timer.station_text(), "FM 100.00 MHz");
    }
}

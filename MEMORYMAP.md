# Sangean ATS-909X2 — memory map of the DATA OUT image

This describes the 12,032-byte image that the radio's factory DATA OUT
function exports and DATA IN imports. It is the radio's EEPROM from `0x0000`
to `0x2EFF`.

Each entry carries one of three labels:

| Label | Meaning |
|---|---|
| **Confirmed** | Changed on the radio and seen in an export, or written with DATA IN and seen on the radio |
| **Inferred** | Fits the exports, the operating manual or the older ATS-909X layout, but no experiment has tested it |
| **Unknown** | Observed bytes with no explanation yet |

All multi-byte numbers are little-endian. All findings come from one radio.
The experiments behind them are listed in section 8.

> **An export is only trustworthy if the radio's verification passed.** While
> DATA OUT runs, the radio is tuned to FM. If that station sends RDS radio
> text, the radio writes the text into the buffer it is exporting from, 16
> bytes at offset `0x40–0x4F` of a 256-byte block, and its own verification
> then fails. Seen in two exports on 2026-11-14 (section 8). Tuning FM to an
> empty frequency before DATA OUT avoids it: of 15 reads made that way on
> 2026-11-15, 14 passed, and the one that failed stopped at block 30, not at
> the first block.

Several inferred entries lean on the EEPROM layout of the older ATS-909X as
reconstructed by the ATS 909 Hoggy project (section 9). The X2 uses other
addresses and longer names, but the same building blocks appear in its image.

## 1. Memory banks

The image holds the radio's three memory banks (MEMOBANK A, B and C in the
menu). Each bank has the same layout.

| Bank | Base | Exported range | Status |
|---|---|---|---|
| A | `0x0000` | `0x0000–0x0FFF` | Confirmed |
| B | `0x1000` | `0x1000–0x1FFF` | Confirmed: an FM preset stored in bank B appeared at `0x17A0`, its name at `0x1154` |
| C | `0x2000` | `0x2000–0x2EFF` | Confirmed by the radio's owner |

The export stops at `0x2EFF`, so the last 256 bytes of bank C's range are not
in the image. What the EEPROM holds from `0x2F00` to `0x3FFF` is unknown.

According to the manual (page 10), one bank stores 558 presets on 62 pages of
9 presets:

| Band | Pages | Presets |
|---|---:|---:|
| FM | 4 | 36 |
| MW | 3 | 27 |
| LW | 1 | 9 |
| SW | 49 | 441 |
| AIR | 5 | 45 |

## 2. Layout of one bank

Offsets are relative to the bank's base. Only SW uses 32-byte page records;
the other bands store their frequencies as one plain list.

| Offset | Size | Contents | Status |
|---|---|---|---|
| `0x0000–0x017F` | 36 names | FM preset names | Confirmed |
| `0x0180–0x01DF` | 9 names | Blank name fields with no known use. After a memory reset, `0x01A0–0x01AF` in bank A read `FF` | Unknown |
| `0x01E0–0x02FF` | 27 names | MW preset names | Confirmed |
| `0x0300–0x051F` | 51 names | SW page names, pages 1–49; the last two fields are unused | Confirmed |
| `0x0520–0x057F` | 9 names | LW preset names | Confirmed |
| `0x0580–0x075F` | 45 names | AIR preset names | Confirmed at two positions |
| `0x0760–0x07A7` | 36 words | FM frequencies | Confirmed |
| `0x07A8–0x07BF` | 24 bytes | Zero | Unknown |
| `0x07C0–0x07F5` | 27 words | MW frequencies | Confirmed |
| `0x07F6–0x07FF` | 10 bytes | Zero | Unknown |
| `0x0800–0x081A` | 27 bytes | MW mode bytes, one per preset | Confirmed for one preset |
| `0x081B–0x081F` | 5 bytes | Zero | Unknown |
| `0x0820–0x0831` | 9 words | LW frequencies | Confirmed |
| `0x0832–0x083F` | 14 bytes | Zero | Unknown |
| `0x0840–0x0E5F` | 49 records | SW presets, pages 1–49, with their mode bytes | Confirmed |
| `0x0E60–0x0EB9` | 45 words | AIR frequencies | Confirmed |
| `0x0EBA–0x0EFF` | 70 bytes | Zero | Unknown |
| `0x0F00–0x0FFF` | 256 bytes | Bank A: settings, timers and a product string (section 6). Bank B: all `FF`. Bank C: not exported | Partly confirmed |

### After a factory reset and a memory reset

Read on 2026-11-15, right after both resets:

- **All three banks are identical**, except 16 bytes at `0x01A0–0x01AF` in
  bank A, in the unused name fields, which read `FF`.
- **FM, MW, LW and AIR** hold only the two factory presets of section 3.
- **SW** holds 185 presets on its 49 pages, and the factory page names: `DW`,
  `SWF_3`, `BR`, `DLF` and so on, ending with `PAGE 40` to `PAGE 49`.
- **All names** of FM, MW, LW and AIR presets are ten `0x20`; all mode bytes
  are `00`; no preset is locked.
- **Settings** as listed in section 6: `0x0F07–0x0F09` = `04 01 04`, cities
  `0x15` and `0x08`, the three timers and the priority station at 100.00 MHz,
  switched off, at 00:00.

## 3. Frequencies

A frequency is one 16-bit word.

| Bits | Meaning | Status |
|---|---|---|
| 0–14 | The frequency, in the band's unit. `0` means the position is empty | Confirmed |
| 15 | The preset is locked (Memory Lock) | Confirmed on FM, SW and AIR |

| Band | Stored value | Example | Status |
|---|---|---|---|
| SW | kHz | `9765` = 9765 kHz | Confirmed |
| MW | kHz | `999` = 999 kHz | Confirmed |
| LW | kHz | `198` = 198 kHz | Confirmed |
| FM | 10 kHz | `9130` = 91.30 MHz | Confirmed |
| AIR | kHz minus 110,000 | `11500` = 121.500 MHz | Confirmed |

With the offset, every AIR frequency fits in 15 bits, which leaves the top bit
free for the lock. Locked examples: 91.30 MHz is `0xA3AA`, 9410 kHz is
`0xA4C2`, 118.000 MHz is `0x9F40`.

A radio that has never stored presets on a band holds two factory presets in
positions 1 and 2 of its first page: 87.50 and 108.00 MHz, 522 and 1602 kHz,
153 and 279 kHz, 118.000 and 137.000 MHz.

### Addresses

For page `p` and position `n`, both counted from 1:

| Band | Address of the frequency word |
|---|---|
| FM | `base + 0x0760 + 2 * (9 * (p - 1) + (n - 1))` |
| MW | `base + 0x07C0 + 2 * (9 * (p - 1) + (n - 1))` |
| LW | `base + 0x0820 + 2 * (n - 1)` |
| SW | `base + 0x0840 + 32 * (p - 1) + 2 * (n - 1)` |
| AIR | `base + 0x0E60 + 2 * (9 * (p - 1) + (n - 1))` |

SW pages 1, 47, 48 and 49 were checked on the radio; the pages between follow
from the spacing.

### The same frequency twice

When a frequency was stored on MW page 1 position 4 while position 1 already
held it, the radio did not keep two copies. It moved the existing preset to
position 4, together with its name, gave it the new mode (USB), and left
position 1 empty with ten `0x20` bytes as its name. **Confirmed on MW, within
one page.** Whether the radio does the same across pages, and what it does
with an image written by DATA IN that holds the same frequency twice, is
untested.

## 4. Mode bytes

A preset's mode byte says whether it was stored in AM or in single sideband.

| Band | Where | Status |
|---|---|---|
| SW | `+0x14` in the page record: nine bytes, one per position | Confirmed for one preset |
| MW | `base + 0x0800 + (9 * (p - 1) + (n - 1))` | Confirmed for one preset |
| LW | Unknown. The 14 zero bytes after the LW frequencies, or the 24 after the FM frequencies, would fit nine bytes | Unknown |
| FM, AIR | None expected: the radio offers SSB on SW, MW and LW only (manual, page 12) | Inferred |

| Value | Meaning | Status |
|---|---|---|
| `00` | AM | Confirmed |
| `D8` | USB. Both presets stored in USB so far (14200 kHz on SW, 522 kHz on MW) got this value | Confirmed |

In the ATS-909X layout bit 7 means SSB, bit 6 means USB (clear is LSB), and
the low 5 bits hold the SSB fine-tune offset. `D8` fits that: SSB, USB, and
`0x18` in the low bits. What `0x18` stands for on the X2 is not known; on the
ATS-909X the value for "no offset" is `0x0C`, and the X2 tunes SSB in 10 or
20 Hz steps (manual, page 12), not the ATS-909X's 40 Hz.

### SW page records

An SW page is a 32-byte record.

| Offset in record | Size | Contents | Status |
|---|---|---|---|
| `+0x00` | 18 bytes | Nine frequency words | Confirmed |
| `+0x12` | 2 bytes | Zero in every record seen. Padding in the ATS-909X layout | Inferred |
| `+0x14` | 9 bytes | One mode byte per preset | Confirmed for one preset |
| `+0x1D` | 3 bytes | Zero in every record seen. Padding in the ATS-909X layout | Inferred |

## 5. Names

A name is 10 bytes. Names are stored three to a 32-byte record, followed by
`00 00`. Counting all names of a bank from 0, name number `i` is at:

```
address = base + 32 * (i / 3) + 10 * (i % 3)
```

with integer division.

| Names | Name number `i` | Status |
|---|---|---|
| FM presets | `9 * (p - 1) + (n - 1)` | Confirmed on page 1 and page 4, in banks A and B |
| (unused) | 36–44 | Unknown |
| MW presets | `45 + 9 * (p - 1) + (n - 1)` | Confirmed on pages 1 and 3 |
| SW pages | `72 + (p - 1)` | Confirmed |
| LW presets | `123 + (n - 1)` | Confirmed at positions 1 and 4 |
| AIR presets | `132 + 9 * (p - 1) + (n - 1)` | Confirmed at page 1 position 3 and page 5 position 9 |

On SW only pages have names; on the other bands each preset has one (manual,
page 11). The order of the lists is the ATS-909X's (FM, MW, SW pages, LW),
with AIR added at the end.

A name field holds one of four things:

| Contents | Example | How it got there | Status |
|---|---|---|---|
| Ten `0x20` | | Never used, or the preset was moved away | Confirmed |
| Ten `0x00` | | Preset stored with the keys, no name | Confirmed |
| Name typed on the radio, blanks as `0x40` | `ABCDE@@@@@`, `999@@@@@@@` | Named with the radio's Edit button | Confirmed |
| RDS name: 8 characters, then `00 00` | `  POP   ` + `00 00` | FM preset stored while the station sent RDS | Confirmed |

- **Typed names** are stored in capitals: names reported as `abcde` and `aaa`
  are `ABCDE` and `AAA` in the image. The factory SW page names contain
  lowercase letters (`120meter`), and the radio shows them.
- **RDS names** are the station's 8-character programme service name, as
  broadcast, spaces included: `CITY FM `, or `  POP   ` for a name a station
  centres.
  The radio fills them in by itself. A preset stored before RDS was received
  gets ten `0x00` instead.
- **Blanks:** the radio shows `0x20`, `0x40` and `0x00` all as a space.
- **Characters:** ASCII. The exports contain capitals, lowercase letters,
  digits, space, `.`, `-` and `_`. For the classic ATS-909 the Hoggy sources
  list what the radio's own editor can produce: `A–Z`, `0–9`, `'`, `,`, `-`,
  `.`, `/` and `@`.

## 6. Bank A, `0x0F00–0x0FFF`: settings

The first 96 bytes line up field by field with the settings block at the end
of the ATS-909X layout, moved from `0x08A0` to `0x0F00`.

| Address | Size | Field | In the exports | Status |
|---|---:|---|---|---|
| `0x0F00` | 7 | Unknown there as well | Zero | Unknown |
| `0x0F07` | 1 | Unknown | `04` in every export | Unknown |
| `0x0F08` | 2 | Menu settings, as bits; see below | `01 04` after a factory reset | Confirmed |
| `0x0F0A` | 6 | Unknown | Zero | Unknown |
| `0x0F10` | 15 | Name of the user-defined city | Blank | Inferred |
| `0x0F1F` | 1 | Flags; bit 4 = show world time | `00` | Inferred |
| `0x0F20` | 16 | Padding | 16 blanks | Inferred |
| `0x0F30` | 6 | Times of the three timers, 2 bytes each: minutes, then hours, binary-coded decimal | Timer 1 at 12:34 is `34 12` | Confirmed for timer 1 |
| `0x0F36` | 1 | World-time city, as an index into a city list | `0x15` = 21, a valid index in the ATS-909X list of 43 cities | Inferred |
| `0x0F37` | 1 | Local-time city | `0x08` after a factory reset, Berlin in the ATS-909X list | Inferred |
| `0x0F38` | 1 | Length of the user-defined city name | `00` | Inferred |
| `0x0F39` | 1 | Time zone of the user-defined city, in half hours | `00` | Inferred |
| `0x0F3A` | 6 | Daylight-saving flags, one bit per city | Zero | Inferred |
| `0x0F40` | 12 | Three timers, 4 bytes each: frequency word, mode byte, flags byte | Timer 1 set to 89.30 MHz: `E2 22 00 C1` | Confirmed for timer 1 |
| `0x0F4C` | 4 | Priority station, same format | `10 27 00 01` | Inferred |
| `0x0F50` | 16 | Product string. On the classic ATS-909 the radio reads it after a DATA IN and restores factory defaults if it is missing | `SANGEANATS-909X2` | Confirmed (present in every export) |
| `0x0F60–0x0FFF` | 160 | Not present in the ATS-909X | Zero | Unknown |

**Timer flags** (byte 4 of a timer): bit 7 = timer on, bit 6 = radio (clear =
buzzer), low 2 bits = band. Timer 1 switched on with an FM station gave `C1`:
on, radio, band 1. Switched off it was `01`. This matches the ATS-909X, where
the band codes are 0 LW, 1 FM, 2 MW, 3 SW. **Confirmed for FM.** The X2 also
has AIR, so its other band codes may differ.

**Menu settings.** On 2026-11-15, after a factory reset, one menu setting was
changed per read. Each change moved only the bits of that setting:

| Setting | Where | Values | Factory value |
|---|---|---|---|
| Memory bank | `0x0F09` bits 0–1 | `00` A, `01` B, `10` C | A |
| Backlight | `0x0F09` bits 2–3 | `00` 10 s, `01` 20 s, `10` 30 s | 20 s |
| FM step | `0x0F09` bits 4–5 | `00` 100 kHz, `01` 50 kHz, `10` 200 kHz | 100 kHz |
| FM stereo | `0x0F09` bit 6 | 0 auto, 1 mono | auto |
| Soft mute | `0x0F09` bit 7 | 0 off, 1 on | off |
| Scan delay | `0x0F08` bits 0–1 | `00` 0.5 s, `01` 1.0 s, `11` 2.0 s; `10` is presumably 1.5 s | 1.0 s |
| SSB step | `0x0F08` bit 4 | 0 20 Hz, 1 10 Hz | 20 Hz |
| Beep | `0x0F08` bit 5 | 0 on, 1 off | on |

All confirmed, apart from scan delay 1.5 s. The 50 kHz FM step comes from the
reads of 2026-11-14. Those reads fit this table too: what looked like a second
beep bit was the memory bank, set to B for the first of them.

Bits 2, 3, 6 and 7 of `0x0F08` were 0 in every export.

**The clock** is probably not in the exported range: no byte has followed the
time of day across exports made hours and days apart.

Not located yet: clock source (RDS CT) and format, and FM range (manual,
pages 14–15). The unused bits of `0x0F08`, the byte at `0x0F07` and the zero
bytes from `0x0F60` are the candidates.

## 7. Open questions

- **Clock source and format, and FM range.** Where they are stored.
- **The 16 `FF` bytes** at `0x01A0` in bank A after a memory reset.
- **Mode bytes on LW**, and confirmation that FM and AIR have none.
- **The SSB fine-tune value.** What do the low bits of the mode byte mean on
  the X2?
- **Timers 2 and 3, the priority station, and timer band codes** for MW, LW,
  SW and AIR.
- **Duplicate frequencies.** Does the radio's move rule apply across pages,
  and how does it treat an image that holds the same frequency twice?
- **The nine name fields at `0x0180`.**

## 8. Experiments

Each experiment is a change on the radio, a read with MemoryManager (`R`), and
a comparison with the previous image.

### Marker presets, 2026-11-13 (done)

Seven presets were stored in MEMOBANK A, each at a page and position that the
frequency itself states, and the radio was read before and after. Every word
landed where the plain-list layout of section 3 puts it.

| Band | Page | Position | Frequency | Frequency word found at | Value | Name field changed at |
|---|---:|---:|---|---|---|---|
| FM | 1 | 3 | 91.30 MHz, locked | `0x0764` | `0xA3AA` = lock + 9130 | `0x0014` |
| FM | 4 | 9 | 94.90 MHz | `0x07A6` | 9490 | `0x0174` |
| MW | 1 | 3 | 999 kHz | `0x07C4` | 999 | `0x01F4` |
| MW | 3 | 9 | 1503 kHz | `0x07F4` | 1503 | `0x02F4` |
| LW | 1 | 4 | 198 kHz | `0x0826` | 198 | `0x0540` |
| AIR | 1 | 3 | 121.500 MHz | `0x0E64` | 11500 | `0x0594` |
| AIR | 5 | 9 | 135.900 MHz | `0x0EB8` | 25900 | `0x0754` |
| SW | 47 | 1 | 14200 kHz, USB | `0x0E00` | 14200 | (page name unchanged) |

The SW preset replaced 11600 kHz, and its mode byte at `0x0E14` changed from
`00` to `D8`. Each of the seven name fields changed from ten blanks to ten
`0x00` bytes. No other byte of the image changed: 86 bytes in all.

### Names, locks, modes, settings and a timer, 2026-11-14 (done)

Several changes were made between two reads. 139 bytes changed, and every one
of them is accounted for. The RDS station names are replaced here by made-up
ones of the same shape.

| Change on the radio | Where it showed | Bytes |
|---|---|---|
| FM page 1 refilled with nine stations | `0x0760–0x0771` | nine frequency words |
| (also on FM) | `0x0772`, page 2 position 1 | 100.50 MHz |
| FM page 1 position 1 named `ABCDE` | `0x0000` | `ABCDE@@@@@` |
| RDS names stored with four of the FM presets | `0x0020`, `0x002A`, `0x0040`, `0x004A` | four 8-character names such as `CITY FM ` and `  POP   `, each + `00 00` |
| The other FM presets, stored without a name | `0x000A`, `0x0034`, `0x0054`, `0x0060` | ten `00` each |
| LW position 1 named `AAA` | `0x0520` | `AAA@@@@@@@` |
| MW position 1 (522 kHz) named `999`, then 522 kHz stored at position 4 in USB | `0x07C0`, `0x07C6`, `0x0200`, `0x0803` | position 1 emptied; position 4 = 522 kHz with the name `999@@@@@@@` and mode `D8` |
| SW page 1 position 1 locked | `0x0841` | `24` → `A4` |
| AIR page 1 position 1 locked | `0x0E61` | `1F` → `9F` |
| Backlight 10 s, FM step 50 kHz, beep on | `0x0F08–0x0F09` | `21 04` → `01 11` |
| Timer 1: 12:34, radio, FM 89.30 MHz | `0x0F30`, `0x0F40` | `34 12`; `E2 22 00 C1` |
| Bank B, FM page 4 position 6: 99.40 MHz, with RDS name | `0x17A0`, `0x1154` | `D4 26`; an 8-character name + `00 00` |

### Beep and FM step, 2026-11-14 (done)

Two more reads, each after changing one setting: beep off, then FM step
100 kHz. Both times the radio's verification failed after the first block.
Apart from the settings bytes, the only differences from the previous export
were RDS radio text from the station the radio was tuned to, `Kropp & Själ`
followed by carriage returns (`0x0D`), in the RDS character set (`0x91` is
`ä`). It sat in the 16 bytes at offset `0x40–0x4F` of 26 and 29 of the 47
blocks. The radio's memory itself was not affected. The settings bytes lie
outside the damaged ranges.

### Menu settings, 2026-11-15 (done)

After a factory reset and a memory reset, 14 reads with FM tuned to an empty
frequency. The first two, without any change, were identical. Each of the
others changed one menu setting, and exactly one byte of the image changed:

| Read | Change | Byte | Before → after |
|---|---|---|---|
| 03 | Memory bank B | `0x0F09` | `0000 0100` → `0000 0101` |
| 04 | Memory bank C | `0x0F09` | `0000 0101` → `0000 0110` |
| 05 | Memory bank A | `0x0F09` | `0000 0110` → `0000 0100` |
| 06 | Backlight 10 s | `0x0F09` | `0000 0100` → `0000 0000` |
| 07 | Backlight 30 s | `0x0F09` | `0000 0000` → `0000 1000` |
| 08 | FM step 200 kHz | `0x0F09` | `0000 1000` → `0010 1000` |
| 09 | Beep off | `0x0F08` | `0000 0001` → `0010 0001` |
| 10 | FM mono | `0x0F09` | `0010 1000` → `0110 1000` |
| 11 | Soft mute on | `0x0F09` | `0110 1000` → `1110 1000` |
| 12 | SSB step 10 Hz | `0x0F08` | `0010 0001` → `0011 0001` |
| 13 | Scan delay 0.5 s | `0x0F08` | `0011 0001` → `0011 0000` |
| 14 | Scan delay 2.0 s | `0x0F08` | `0011 0000` → `0011 0011` |

### Still to do

Tune FM to an empty frequency before each DATA OUT.

| Change on the radio | What it shows |
|---|---|
| Clock format 12H; clock source RDS CT; another FM range | Where those settings are |
| Home city, world-time city, daylight saving, a user-defined city | Whether the city fields of section 6 are right |
| Timer 1 off; timer 2 on the buzzer; timer 3 on SW, AIR and LW stations | Timer flags and band codes |
| Store an LW preset in LSB | Where LW keeps its mode bytes, and the LSB value |
| Fine-tune an SSB preset and store it | The meaning of the low bits of the mode byte |
| Lock an MW and an LW preset | The lock bit on those bands |
| Type a name for an AIR preset | Typed names on AIR |
| Store a preset in MEMOBANK C | Confirms bank C with an export |
| Store a frequency that is already on another page | Whether the move rule works across pages |
| Tune each band to an unusual frequency without storing it | Whether the last frequency per band is stored |
| Select an SW meter band; change the bandwidth, squelch or display brightness | Whether those are stored |

## 9. Sources

- Exports of one ATS-909X2: two made on 2026-11-12, before and after changing
  a single SW preset from 9765 to 9999 kHz; two made on 2026-11-13, before and
  after storing the marker presets; several made on 2026-11-14 and 15 more on
  2026-11-15, after the changes listed in section 8. The exports themselves
  are not published.
- ATS 909 Hoggy project, `EEPROM.h` by Siarzhuk Zharski (MIT licence): the
  EEPROM structures of the ATS-909 and ATS-909X. Source tree:
  <https://sourceforge.net/p/ats909hoggy/ats909hoggy/ci/default/tree/>. The
  repository also holds the adapter firmware, the editor's source (which shows
  how the lock bit, mode bytes, timers, priority station and city list are
  used) and a 2012 article on the classic ATS-909's DATA IN and DATA OUT.
- Sangean ATS-909X2 operating manual, pages 9–17:
  <https://www.rxtx.fi/storage/product_files/3/401003-ATS-909X2_GB_Manual_7670554683a3a8cfa40782a2d1939855.pdf>

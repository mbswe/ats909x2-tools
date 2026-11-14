# ats909x2-tools

Tools for managing the station memory of a **Sangean ATS-909X2** world-band
radio from a computer:

- **MemoryManager** — a terminal editor for the radio's presets on FM, MW, LW,
  SW and AIR: frequencies, names and locks, in all three memory banks.
- **Bridge** — firmware for an ESP32 that connects the computer to the radio's
  factory transfer contacts, so the editor can read the radio's memory and
  write it back.

This is an independent hobby project. It is not affiliated with or endorsed by
Sangean.

```
 computer ── USB ── ESP32 bridge ── two contacts + ground ── ATS-909X2
 MemoryManager        bridge/                                DATA OUT / DATA IN
```

## Status

Everything here was worked out by observing one radio. Treat it as
experimental.

| | State |
|---|---|
| Reading the radio (DATA OUT) | Works; the radio ends with `VERIFY` and `OK` |
| Writing the radio (DATA IN) | Has worked, but has been tested far less than reading |
| Editing SW frequencies | A changed preset has been written to the radio and read back correctly |
| Editing names, locks, and presets on FM, MW, LW and AIR | The layout is confirmed by reading the radio. Writing such edits back has not been tested yet |
| Radio settings (clock, timers, menu) | Not editable; passed through unchanged |
| Platforms | Developed and used on macOS. Linux has not been tried, nor has Windows |

## Read this before you write to your radio

- **A write replaces the radio's whole memory:** all presets and names of all
  bands in all three memory banks.
- **Read your radio first and keep that file.** It is your backup, and the
  editor needs it: every image you write should start from your own radio's
  image, because the image also holds bytes whose meaning is not known.
- **Only use a read the radio verified.** The radio must end a read with
  `VERIFY` and `OK`. If it shows `FAILED`, parts of the file may not be its
  memory (see below); the editor names such a file `…-unverified.bin` and
  will not write it.
- **Never start DATA IN on the radio unless the editor tells you to.** On the
  older ATS-909, starting it with nothing connected erases the memory and
  restores the factory presets. Whether the X2 does the same has not been
  tested; assume it does.
- **Keep the connection steady during a transfer.** A lead that slips makes
  the radio stop part-way. Running the write again has repaired that so far.
- There is no warranty of any kind. You use this at your own risk.

## What you need

- A Sangean ATS-909X2.
- An ESP32 development board with a classic **ESP32-WROOM-32** module and a
  USB serial chip. Other ESP32 families (S2, S3, C3 and so on) are not
  supported: the firmware relies on the timing and registers of the classic
  ESP32.
- Two 1 kΩ resistors and three wires.
- A way to reach the two factory contacts on the underside of the radio, and
  ground.
- [Rust](https://www.rust-lang.org/tools/install) 1.88 or newer, for the editor.
- The [Arduino IDE](https://www.arduino.cc/en/software) or `arduino-cli` with
  the "esp32 by Espressif Systems" boards package (built with version 3.3),
  for the firmware.

## 1. Build the bridge

Wire the ESP32 to the radio:

```
     ESP32 dev board                                   ATS-909X2
    (ESP32-WROOM-32)                          contacts on the underside

                          R1  1 kΩ
    GPIO22  SCL in   ─────[ 1k ]──────────────────────  SCL contact
                                                        (TP300)
                          R2  1 kΩ
    GPIO21  SDA out  ─────[ 1k ]──────────┬───────────  SDA contact
                                          │             (TP301)
    GPIO34  SDA in   ─────────────────────┘

    GND              ─────────────────────────────────  GND

    USB  ──────  computer             nothing connects to the radio's supply
```

Then flash the firmware. In the Arduino IDE, open `bridge/bridge.ino`, choose
the board "ESP32 Dev Module" and upload. Or from a terminal:

```sh
arduino-cli compile --upload --fqbn esp32:esp32:esp32 -p <serial port> bridge
```

[`bridge/README.md`](bridge/README.md) has the parts list, what each pin does,
electrical notes and a self-test.

## 2. Build the editor

```sh
cd memorymanager
cargo run --release
```

To look around without a radio, `cargo run --release -- --demo` opens a
made-up image. It cannot be written to a radio.

## 3. Read your radio

1. Plug the ESP32 into USB **before** switching the radio on. (Unplug it last.)
2. **Tune FM to an empty frequency**, with no station. The radio exports on
   FM, and RDS radio text from a station can get mixed into the export.
3. Start the editor and press `R`.
4. When the editor says so, on the radio: **hold FM + LIGHT, then press
   ENTER**. The radio shows `DATA OUT`, then `VERIFY`, then `OK`.
5. The image opens in the editor. Save it. Keep an untouched copy as a backup.

The editor finds the bridge's serial port by itself. `--port <serial port>`
names it instead. Close the Arduino Serial Monitor first; it keeps the port
busy.

## 4. Edit

| Key | Does |
|---|---|
| `Tab` / `Shift+Tab` | next / previous memory bank (A, B, C) |
| `b` / `B` | next / previous band: FM, MW, LW, SW, AIR |
| `←` `→` `↑` `↓` | move between the page list and the presets |
| `Enter` or a digit | edit the frequency |
| `r` | name: the page on SW, the selected preset on the other bands |
| `L` | lock or unlock the selected preset |
| `x` | clear the preset |
| `u` | put back the saved value |
| `s` / `S` | save / save as |
| `o` | open an image |
| `n` | new image: a copy of the open one with all presets and names cleared |
| `R` / `W` | read the radio / write the radio |
| `?` | all keys |

Frequencies are typed the way the radio shows them: FM and AIR in MHz
(`91.30`, `121.500`), MW, LW and SW in whole kHz.

An edit changes only the bytes of the field you edit. Everything else in the
image, including what is not understood, goes back to the radio as it came.

## 5. Write to your radio

1. Read the radio in the same session if you can: the editor then lists
   exactly which presets and names the write will change.
2. Save the image and press `W`. Check the confirmation, then press `y`.
3. When the editor says so, on the radio: **hold LIGHT + LW, then press
   ENTER**. The radio reads the image twice and shows `OK`.
4. Press `v` to read the radio back and compare.

The editor refuses to send an image that does not carry the radio's product
string, such as the demo image or a file that is not a radio's export.

## When something goes wrong

| Message | Likely cause |
|---|---|
| The radio shows `FAILED` after `VERIFY` when reading | The radio was tuned to an FM station sending RDS radio text, and some of that text got into the export. Tune FM to an empty frequency and read again. Do not use the failed file |
| No bridge found / no answer | The firmware is not flashed, USB is not connected, or the Serial Monitor holds the port |
| The I²C bus is busy | The radio is not idle, or a wire is shorted |
| GPIO21 reads LOW | GPIO21 is not connected to the SDA contact through its resistor |
| Wrong direction | DATA IN was started on the radio while reading, or DATA OUT while writing |
| The radio did not start | The keys were not pressed within two minutes, or the leads are not touching the contacts |
| The radio stopped before the transfer was complete | A lead lost contact. Run the transfer again |

Every transfer is logged with the bridge's own diagnostics, in
`~/Library/Logs/MemoryManager/radio.log` on macOS and
`~/.local/state/memory-manager/radio.log` elsewhere.

## How it works

The radio has a factory function for copying its memory to and from an
external memory chip on two contacts. The bridge takes the place of that chip.
The radio clocks every bit itself and expects an answer within microseconds,
so the bridge cannot pass the traffic on to the computer as it happens.
Instead it keeps the whole 12,032-byte image in its own memory and talks to
the computer only before and after the radio's transfer.

- [`MEMORYMAP.md`](MEMORYMAP.md) describes the image: where every preset and
  name is, what is confirmed, and what is still unknown.
- `bridge/protocol.h` documents the serial protocol between editor and bridge.
- `bridge/poller.h` explains how the bridge keeps up with the radio's timing.

## Tests

Neither needs a radio or an ESP32:

```sh
cd memorymanager && cargo test          # editor and transfer code
bridge/test/run-host-sim.sh             # the firmware against a simulated radio
```

## Layout

```
memorymanager/   the editor (Rust)
bridge/          the ESP32 firmware (Arduino) and its simulation
MEMORYMAP.md     the layout of the radio's memory image
```

## License

[MIT](LICENSE). You may use, change and share this freely, as long as the
copyright notice and licence text stay with it. If you build something on it,
a link back to this repository is appreciated.

# Bridge: ESP32 between the computer and the ATS-909X2

The bridge is an ESP32 development board that stands in for the radio's
factory accessory. It holds one memory image and answers the radio's DATA OUT
and DATA IN transfers on the two factory contacts. The computer talks to it
over USB.

## Schematic

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

Inside the radio, each contact reaches the I²C bus through its own 2.2 kΩ
resistor (R358 for SCL, R359 for SDA in the service schematic).

## Parts

| Part | Value | Notes |
|---|---|---|
| ESP32 development board | Classic ESP32-WROOM-32 with a USB serial chip | Tested with a CP2102 board |
| R1 | 1 kΩ | In the SCL wire. GPIO22 only listens, so the value is not critical |
| R2 | 1 kΩ | In the SDA wire. Must be small enough for GPIO21 to pull SDA LOW: 50 kΩ did not work |
| Wires | 3 | SCL, SDA and ground |

## What each pin does

| ESP32 pin | Direction | Role |
|---|---|---|
| GPIO22 | Input | Reads the radio's clock. The bridge never drives SCL |
| GPIO21 | Open-drain output | Pulls SDA LOW through R2, or lets go of it |
| GPIO34 | Input only | Reads SDA at the radio's contact. It must connect on the radio side of R2 |
| GND | | Common ground |

GPIO21 reads LOW at its own pin whenever it pulls LOW, whatever the line at
the radio does. GPIO34 shows the level the radio sees, and the firmware takes
every SDA reading from it. Without that wire the bridge does not work.

## Flashing

In the Arduino IDE, open `bridge.ino`, choose the board "ESP32 Dev Module" and
upload. With `arduino-cli`, from the folder above this one:

```sh
arduino-cli compile --upload --fqbn esp32:esp32:esp32 -p <serial port> bridge
```

The firmware was built with version 3.3 of the "esp32 by Espressif Systems"
boards package. It supports the classic ESP32 only.

## Electrical notes

- **Levels.** The radio's bus idles at about 2.7–2.8 V. The ESP32 runs at
  3.3 V.
- **Pull-ups.** While a transfer is armed, the firmware switches on the
  ESP32's internal pull-ups (about 45 kΩ to 3.3 V) on GPIO22 and GPIO21.
  Without them the rising edges were too slow at the ESP32's inputs. At most
  about 70 µA flows into a line the radio holds LOW.
- **Power order.** Plug the ESP32 into USB before switching the radio on, and
  unplug it last. An unpowered ESP32 can load the two lines.
- **Leads.** Keep them short and apart. Probes held by hand work for a
  transfer of half a minute, but a lead that slips stops the transfer; a fixed
  connection is better.
- **Which contact is which.** TP300 is SCL and TP301 is SDA in the service
  schematic. Check the contacts on your radio with a meter before wiring.

Two changes that might help but have not been tried: external pull-up
resistors of about 10 kΩ from each contact to the ESP32's 3V3 pin, and about
10 kΩ in series with GPIO34 as protection.

## Talking to it by hand

The bridge speaks plain text lines at 115200 baud (`protocol.h`). In a serial
monitor, `HELLO` shows the firmware version and `DIAG` prints the counters and
timing measurements of the last transfer.

## Tests without a radio

- **Simulation.** `test/run-host-sim.sh` builds the decoder, session and
  protocol code for the computer and runs them against a simulated radio that
  clocks bit by bit, including glitches, slow edges and lost processor time.
- **Self-test on the board.** `SPIN <ms>` keeps interrupts masked for that
  long, the way an armed transfer does, and answers `OK SPIN`. It checks that
  the board survives a long transfer. `SPIN <ms> RAW` leaves the way an
  earlier firmware did and resets the board for durations between about 9 and
  18 seconds; it is there to demonstrate that fault.

## How it keeps up with the radio

The radio clocks every bit itself and raises the clock again as little as
12.5 µs after a fall. While armed, the firmware therefore does nothing but
watch the two lines, with interrupts masked, and sets SDA the moment it sees
the clock fall. `poller.h` describes the timing problems found on the way and
how each is handled.

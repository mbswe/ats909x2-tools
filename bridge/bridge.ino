/*
 * Sangean ATS-909X2 transfer bridge for MemoryManager (classic ESP32-WROOM-32).
 *
 * Emulates the radio's factory transfer accessory, I²C target 0x51, and keeps
 * one 12,032-byte image (EEPROM 0x0000–0x2EFF) in RAM:
 *   - ARM OUT, then DATA OUT on the radio (hold FM + LIGHT, press ENTER):
 *     the radio sends its memory here and verifies it. MemoryManager then
 *     downloads it with GET.
 *   - PUT/COMMIT an image, ARM IN, then DATA IN on the radio (hold LIGHT + LW,
 *     press ENTER): the radio reads the image twice (transfer and verify).
 * The serial protocol is documented in protocol.h. MemoryManager speaks it;
 * the Serial Monitor (115200 baud, newline) works for manual tests and DIAG.
 *
 * Wiring (see README.md for a schematic):
 *   GPIO21 -- 1 kΩ ---------- radio SDA contact (TP301), open-drain output
 *   GPIO34 ------------------ same SDA contact, radio side of the 1 kΩ (sense)
 *   GPIO22 -- 1 kΩ ---------- radio SCL contact (TP300), input only; the value
 *                             is not critical (the earlier sniffer used 50 kΩ)
 *   GND --------------------- radio GND
 * Plug in USB before turning the radio on, and unplug it last.
 *
 * One loop on core 1, no other tasks, and no serial output while armed.
 * While armed, the bus is polled with interrupts masked (poller.h).
 *
 * Pull-ups: while armed, the ESP32's internal
 * pull-ups (about 45 kΩ to 3.3 V) are on for GPIO22 (SCL) and GPIO21 (SDA).
 * With hand-held leads the lines rose so slowly at these inputs that clock
 * pulses were seen for only about 3.5 µs, or not at all; the radio's HIGH
 * level, about 2.7 V, is close to the ESP32's input threshold. The pull-ups
 * add at most about 70 µA into a line the radio holds LOW. They are off
 * whenever the bridge is not armed.
 * Nothing is written to flash.
 */

#include <Arduino.h>
#include "driver/gpio.h"
#include "esp_cpu.h"
#include "esp_random.h"
#include "soc/gpio_struct.h"

#include "bus_engine.h"
#include "poller.h"
#include "protocol.h"
#include "session.h"

using namespace bridge;

BusEngine engine;
Session session(engine);
Staging staging;
LoopSide loopSide(engine, session, staging);

// While armed, the bus is polled (poller.h) instead of interrupt-driven.
PollState pollState;

PinLevels readPins() {
  const uint32_t lowPins = GPIO.in;
  return {(lowPins & SCL_MASK) != 0, (GPIO.in1.val & SENSE_MASK) != 0,
          (lowPins & DRIVE_MASK) != 0, (GPIO.out & DRIVE_MASK) != 0};
}

class InterruptMask : public Critical {
 public:
  void enter() override { noInterrupts(); }
  void exit() override { interrupts(); }
};
InterruptMask interruptMask;

class SerialOutput : public Output {
 public:
  void line(const char *text) override { Serial.println(text); }
};
SerialOutput serialOut;

// Internal pull-ups on SCL and on the SDA drive pin (which reaches the SDA
// contact through its 1 kΩ). GPIO34, the SDA sense pin, has none.
bool pullupsOn = false;
void setPullups(bool on) {
  if (on == pullupsOn) return;
  pullupsOn = on;
  if (on) {
    gpio_pullup_en(gpio_num_t(SCL_PIN));
    gpio_pullup_en(gpio_num_t(DRIVE_PIN));
  } else {
    gpio_pullup_dis(gpio_num_t(SCL_PIN));
    gpio_pullup_dis(gpio_num_t(DRIVE_PIN));
  }
}

// Waits up to `timeoutMs` for SCL, SDA and the drive pin to stay HIGH for
// 200 µs: a real gap in the radio's own traffic to its tuner and LED driver,
// rather than the HIGH half of a clock pulse (about 35 µs).
PinLevels waitForIdleBus(uint32_t timeoutMs) {
  const uint32_t startMs = millis();
  PinLevels pins = readPins();
  bool idle = false;
  uint32_t idleSinceUs = 0;
  while (millis() - startMs < timeoutMs) {
    pins = readPins();
    if (!(pins.sclHigh && pins.sdaHigh && pins.localHigh)) {
      idle = false;
      continue;
    }
    const uint32_t nowUs = micros();
    if (!idle) {
      idle = true;
      idleSinceUs = nowUs;
    } else if (nowUs - idleSinceUs >= 200) {
      return pins;
    }
  }
  return pins;
}

// Commands reach the loop side directly: everything runs in loop().
class DirectLoopPort : public LoopPort {
 public:
  void call(const LoopRequest &request, Output &out) override {
    const bool arming =
        request.command == LoopCommand::ArmIn || request.command == LoopCommand::ArmOut;
    if (arming) setPullups(true);
    const PinLevels pins = arming ? waitForIdleBus(500) : readPins();
    loopSide.handle(request, pins, millis(), out, interruptMask);
    if (!loopSide.armed()) setPullups(false);
  }
};
DirectLoopPort loopPort;
CommandSide *commands = nullptr;

char line[MAX_LINE + 1];
size_t lineLength = 0;
bool lineOverflow = false;

// Self-test: keeps interrupts masked for `ms` the way an armed transfer does,
// with the watchdog stand-in, then leaves the same way. "RAW" leaves without
// setting the tick timer, as the firmware did until 2026-11-13.
void spinMasked(uint32_t ms, bool raw) {
  const uint32_t perMs = engine.ticksPerUs * 1000;
  pollMask();
  pollFeedWatchdog();
  uint32_t mark = pollTicks(), fed = mark, elapsed = 0;
  while (elapsed < ms) {
    const uint32_t now = pollTicks();
    if (now - mark >= perMs) {
      mark += perMs;
      ++elapsed;
    }
    if (now - fed >= WATCHDOG_FEED_MS * perMs) {
      fed = now;
      pollFeedWatchdog();
    }
  }
  if (raw) {
    pollUnmask();
  } else {
    pollLeave(perMs);
  }
}

void readSerial() {
  while (Serial.available() > 0) {
    const int c = Serial.read();
    if (c == '\n') {
      if (lineOverflow) {
        serialOut.line("ERR LINE too_long");
      } else {
        line[lineLength] = '\0';
        commands->armed = loopSide.armed();
        if (!strncmp(line, "SPIN ", 5)) {
          if (loopSide.armed()) {
            serialOut.line("ERR SPIN armed");
          } else {
            const uint32_t ms = strtoul(line + 5, nullptr, 10);
            const bool raw = strstr(line, " RAW") != nullptr;
            spinMasked(ms > 60000 ? 60000 : ms, raw);
            serialOut.line("OK SPIN");
          }
        } else {
          commands->handleLine(line, serialOut);
        }
      }
      lineLength = 0;
      lineOverflow = false;
    } else if (c != '\r' && c >= 0) {
      if (lineLength < MAX_LINE) {
        line[lineLength++] = char(c);
      } else {
        lineOverflow = true;
      }
    }
  }
}

void setup() {
  pinMode(SCL_PIN, INPUT);
  pinMode(SENSE_PIN, INPUT);
  pinMode(DRIVE_PIN, INPUT);
  sdaRelease();
  pinMode(DRIVE_PIN, OUTPUT_OPEN_DRAIN);
  engine.ticksPerUs = getCpuFrequencyMhz();
  engine.interruptKind = "polling, internal pull-ups while armed";

  Serial.setRxBufferSize(1024);
  Serial.begin(115200);
  static CommandSide commandSide(staging, loopPort, esp_random());
  commands = &commandSide;
  commands->greet(serialOut);
}

void loop() {
  readSerial();
  loopSide.poll(millis(), serialOut);
  if (loopSide.armed()) {
    // Polls until the transfer ends, or returns briefly for serial input
    // while the bus is idle. No delay() while armed: a sleeping loop would
    // miss the start of the radio's next transaction.
    const PollResult result = pollArmed(engine, session, pollState, millis());
    if (!loopSide.armed()) setPullups(false);
    loopSide.report(result.event, serialOut);
    return;
  }
  setPullups(false);
  delay(1);
}

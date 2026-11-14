/*
 * Loop-level supervision of a transfer: arming, completion, timeouts and
 * progress. Runs on the same core as the bus interrupt, never inside it.
 * Pure C++ so the host simulation can drive it with a simulated clock.
 */
#pragma once

#include "bus_engine.h"

namespace bridge {

constexpr uint32_t ARM_WINDOW_MS = 120000;     // waiting for the radio to start
constexpr uint32_t INCOMPLETE_QUIET_MS = 3000;  // radio stopped mid-transfer
constexpr uint32_t OUT_DONE_QUIET_MS = 1500;    // all packets in, verify ended
constexpr uint32_t IN_DONE_QUIET_MS = 1000;     // two passes served
// Never hold SDA low longer. A real EEPROM holds its ACK until SCL falls,
// however long that takes; the radio sometimes keeps SCL HIGH after a
// packet's last ACK for more than the 2 ms an earlier version allowed.
constexpr uint32_t SDA_HOLD_TIMEOUT_US = 50000;
constexpr uint32_t PROGRESS_INTERVAL_MS = 250;

enum class EventKind : uint8_t { None, Progress, Done, Fail };

struct Event {
  EventKind kind = EventKind::None;
  Mode dir = Mode::Off;
  const char *reason = "";
  // Out
  uint16_t packets = 0;
  uint8_t conflicts = 0;
  uint8_t verify = 0;
  // In
  uint32_t bytes = 0;
  uint8_t passes = 0;
  uint32_t pastEnd = 0;
  // Both
  uint32_t mismatches = 0;
  uint32_t seen = 0;  // address bytes for 0x51
};

class Session {
 public:
  explicit Session(BusEngine &engine) : engine_(engine) {}

  // Call with interrupts masked.
  bool arm(Mode mode, bool sclHigh, bool sdaHigh, bool localHigh, uint32_t nowMs) {
    if (!engine_.arm(mode, sclHigh, sdaHigh, localHigh)) return false;
    armedAtMs_ = nowMs;
    lastActivityMs_ = nowMs;
    lastActivity_ = 0;
    started_ = false;
    lastProgressMs_ = nowMs;
    lastProgressKey_ = 0;
    return true;
  }

  // Call with interrupts masked. Returns the final event for an armed run.
  Event abort() {
    if (engine_.mode == Mode::Off) return Event{};
    return finish(EventKind::Fail, "aborted");
  }

  bool armed() const { return engine_.mode != Mode::Off; }

  // Progress events while armed. Off by default: serial traffic during a
  // transfer is not worth any risk to the bus interrupt's timing.
  void reportProgress(bool on) { reportProgress_ = on; }

  // Call regularly from the loop. Returns at most one event. Runs from IRAM,
  // like the polling loop that calls it while armed.
  Event tick(uint32_t nowMs);

 private:
  BusEngine &engine_;
  uint32_t armedAtMs_ = 0;
  uint32_t lastActivityMs_ = 0;
  uint32_t lastActivity_ = 0;
  bool started_ = false;
  bool reportProgress_ = false;
  uint32_t lastProgressMs_ = 0;
  uint32_t lastProgressKey_ = 0;

  // Whether the current low pulse on SDA has lasted too long. The pulse start
  // is read before the clock, and the same pulse must still be active
  // afterwards: the interrupt may start a new pulse at any moment, and a start
  // time later than "now" would wrap around to a huge duration.
  __attribute__((always_inline)) bool sdaHeldTooLong() const {
    if (!engine_.drivingLow) return false;
    const uint32_t started = engine_.lowStartedTicks;
    const uint32_t held = nowTicks() - started;
    return held >= SDA_HOLD_TIMEOUT_US * engine_.ticksPerUs && engine_.drivingLow &&
           engine_.lowStartedTicks == started;
  }

  __attribute__((always_inline)) Event snapshot(EventKind kind, const char *reason) const {
    Event event;
    event.kind = kind;
    event.dir = engine_.mode;
    event.reason = reason;
    event.packets = engine_.packetCount;
    event.conflicts = engine_.conflictBlocks;
    event.verify = engine_.verifyCount;
    event.bytes = engine_.bytesClocked;
    event.passes = engine_.passes();
    event.pastEnd = engine_.pastEndRequests;
    event.mismatches = engine_.bitMismatches;
    event.seen = engine_.activity;
    return event;
  }

  __attribute__((always_inline)) Event finish(EventKind kind, const char *reason) {
    const Event event = snapshot(kind, reason);
    engine_.disarm();
    return event;
  }
};

// Outside the class body so it can live in IRAM (see bus_engine.h).
Event IRAM_ATTR Session::tick(uint32_t nowMs) {
  if (engine_.mode == Mode::Off) return Event{};

  if (sdaHeldTooLong()) return finish(EventKind::Fail, "stalled");
  if (engine_.wrongDirection) return finish(EventKind::Fail, "wrong_direction");

  const uint32_t activity = engine_.activity;
  if (activity != lastActivity_) {
    lastActivity_ = activity;
    lastActivityMs_ = nowMs;
    started_ = true;
  }
  const uint32_t quietMs = nowMs - lastActivityMs_;
  if (!started_) {
    if (nowMs - armedAtMs_ >= ARM_WINDOW_MS) return finish(EventKind::Fail, "no_radio");
    return Event{};
  }

  if (engine_.mode == Mode::Out) {
    const bool packetsComplete =
        engine_.packetCount == PACKET_COUNT && engine_.conflictBlocks == 0;
    if (packetsComplete && engine_.verifyCount == BLOCK_COUNT) {
      return finish(EventKind::Done, "");
    }
    if (packetsComplete && quietMs >= OUT_DONE_QUIET_MS) return finish(EventKind::Done, "");
    if (quietMs >= INCOMPLETE_QUIET_MS) {
      return finish(EventKind::Fail, engine_.conflictBlocks ? "conflict" : "incomplete");
    }
  } else {
    if (engine_.passes() >= 2 && quietMs >= IN_DONE_QUIET_MS) return finish(EventKind::Done, "");
    if (quietMs >= INCOMPLETE_QUIET_MS) return finish(EventKind::Fail, "incomplete");
  }

  if (reportProgress_ && nowMs - lastProgressMs_ >= PROGRESS_INTERVAL_MS) {
    // Includes raw 0x51 activity, so a radio that talks without storing
    // packets still shows up as progress.
    const uint32_t key = (engine_.mode == Mode::Out
                              ? (uint32_t(engine_.packetCount) << 8) | engine_.verifyCount
                              : engine_.bytesClocked) ^
                         (engine_.activity << 20);
    if (key != lastProgressKey_) {
      lastProgressMs_ = nowMs;
      lastProgressKey_ = key;
      return snapshot(EventKind::Progress, "");
    }
  }
  return Event{};
}

}  // namespace bridge

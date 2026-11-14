/*
 * Serial protocol between MemoryManager and the bridge (115200 8N1, ASCII lines).
 *
 * PC -> bridge                       bridge -> PC
 *   HELLO                              OK HELLO bridge proto=1 fw=1.0.0 len=12032 boot=XXXXXXXX
 *   STATUS                             OK STATUS state=idle|out|in staged=0|1 staged_crc=X image_crc=X
 *   PUT <block> <512 hex> <crc>        OK PUT <block>        | ERR PUT <reason>
 *   COMMIT <crc>                       OK COMMIT crc=X       | ERR COMMIT <reason>
 *   ARM IN <crc>                       OK ARM dir=in window_ms=120000  | ERR ARM <reason>
 *   ARM OUT                            OK ARM dir=out window_ms=120000 | ERR ARM <reason>
 *   GET <block>                        OK GET <block> <512 hex> <crc>  | ERR GET <reason>
 *   ABORT                              OK ABORT
 *   DIAG                               "# ..." lines, then OK DIAG
 *   TRACE                              "# ..." lines (last 4096 bus interrupts), then OK TRACE
 *
 * Sent by the bridge on its own:
 *   READY bridge proto=1 fw=1.0.0 boot=X       after reset
 *   EV PROGRESS dir=out packets=N verify=N seen=N   only if enabled with
 *   EV PROGRESS dir=in bytes=N passes=N seen=N      Session::reportProgress; the
 *                                                   firmware keeps the line silent
 *                                                   while armed (seen = 0x51 bytes)
 *   EV DONE dir=out packets=752 verify=47 mism=0 crc=X
 *   EV DONE dir=in passes=2 bytes=24064 mism=0 past_end=0
 *   EV FAIL dir=out|in reason=R packets=N verify=N bytes=N passes=N mism=N seen=N
 *
 * Blocks are 256 bytes, numbered 0–46. CRCs are CRC-32 (zlib) as 8 hex digits.
 * The bridge refuses PUT, COMMIT, ARM and GET while armed; send only ABORT then.
 */
#pragma once

#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "session.h"

namespace bridge {

constexpr unsigned PROTOCOL_VERSION = 1;
constexpr const char *FIRMWARE_VERSION = "1.0.0";
constexpr size_t MAX_LINE = 640;  // PUT lines are ~530 characters

// ---- CRC-32 (zlib / IEEE 802.3), "123456789" -> CBF43926 ----
inline uint32_t crc32(const uint8_t *data, size_t len) {
  uint32_t crc = 0xFFFFFFFFu;
  for (size_t i = 0; i < len; ++i) {
    crc ^= data[i];
    for (int bit = 0; bit < 8; ++bit) crc = (crc >> 1) ^ (0xEDB88320u & (0u - (crc & 1u)));
  }
  return ~crc;
}

inline void hexEncode(const uint8_t *data, size_t len, char *out) {
  static const char digits[] = "0123456789ABCDEF";
  for (size_t i = 0; i < len; ++i) {
    out[2 * i] = digits[data[i] >> 4];
    out[2 * i + 1] = digits[data[i] & 0x0F];
  }
  out[2 * len] = '\0';
}

inline int hexValue(char c) {
  if (c >= '0' && c <= '9') return c - '0';
  if (c >= 'a' && c <= 'f') return c - 'a' + 10;
  if (c >= 'A' && c <= 'F') return c - 'A' + 10;
  return -1;
}

// Decodes exactly 2 * len hex characters.
inline bool hexDecode(const char *hex, size_t len, uint8_t *out) {
  if (strlen(hex) != 2 * len) return false;
  for (size_t i = 0; i < len; ++i) {
    const int high = hexValue(hex[2 * i]);
    const int low = hexValue(hex[2 * i + 1]);
    if (high < 0 || low < 0) return false;
    out[i] = uint8_t((high << 4) | low);
  }
  return true;
}

inline bool parseCrc(const char *text, uint32_t &crc) {
  if (strlen(text) != 8) return false;
  uint8_t bytes[4];
  if (!hexDecode(text, 4, bytes)) return false;
  crc = (uint32_t(bytes[0]) << 24) | (uint32_t(bytes[1]) << 16) | (uint32_t(bytes[2]) << 8) | bytes[3];
  return true;
}

inline bool parseBlock(const char *text, uint8_t &block) {
  if (!*text || strlen(text) > 2) return false;
  for (const char *p = text; *p; ++p) {
    if (!isdigit(static_cast<unsigned char>(*p))) return false;
  }
  const int value = atoi(text);
  if (value < 0 || value >= BLOCK_COUNT) return false;
  block = uint8_t(value);
  return true;
}

inline const char *dirName(Mode mode) {
  return mode == Mode::Out ? "out" : mode == Mode::In ? "in" : "idle";
}

// ---- Output sinks ----
class Output {
 public:
  virtual void line(const char *text) = 0;
  virtual ~Output() = default;
};

// Collects lines separated by '\n' into a fixed buffer.
class BufferOutput : public Output {
 public:
  BufferOutput(char *buffer, size_t size) : buffer_(buffer), size_(size) { clear(); }
  void clear() {
    used_ = 0;
    buffer_[0] = '\0';
  }
  void line(const char *text) override {
    const size_t len = strlen(text);
    if (used_ + len + 2 > size_) return;
    memcpy(buffer_ + used_, text, len);
    used_ += len;
    buffer_[used_++] = '\n';
    buffer_[used_] = '\0';
  }
  const char *text() const { return buffer_; }

 private:
  char *buffer_;
  size_t size_;
  size_t used_ = 0;
};

// Masks the bus interrupt around engine changes (a no-op on the host).
class Critical {
 public:
  virtual void enter() = 0;
  virtual void exit() = 0;
  virtual ~Critical() = default;
};

struct PinLevels {
  bool sclHigh;
  bool sdaHigh;     // GPIO34, at the radio's SDA contact
  bool localHigh;   // GPIO21 pad, which sits behind the 1 kΩ to the contact
  bool released = true;  // GPIO21 output latch: true when not pulling SDA low
};

inline void formatEvent(const Event &event, uint32_t imageCrc, char *out, size_t size) {
  const char *dir = dirName(event.dir);
  switch (event.kind) {
    case EventKind::Progress:
      if (event.dir == Mode::Out) {
        snprintf(out, size, "EV PROGRESS dir=out packets=%u verify=%u seen=%lu", unsigned(event.packets),
                 unsigned(event.verify), (unsigned long)event.seen);
      } else {
        snprintf(out, size, "EV PROGRESS dir=in bytes=%lu passes=%u seen=%lu", (unsigned long)event.bytes,
                 unsigned(event.passes), (unsigned long)event.seen);
      }
      break;
    case EventKind::Done:
      if (event.dir == Mode::Out) {
        snprintf(out, size, "EV DONE dir=out packets=%u verify=%u mism=%lu crc=%08lX",
                 unsigned(event.packets), unsigned(event.verify), (unsigned long)event.mismatches,
                 (unsigned long)imageCrc);
      } else {
        snprintf(out, size, "EV DONE dir=in passes=%u bytes=%lu mism=%lu past_end=%lu",
                 unsigned(event.passes), (unsigned long)event.bytes,
                 (unsigned long)event.mismatches, (unsigned long)event.pastEnd);
      }
      break;
    case EventKind::Fail:
      snprintf(out, size,
               "EV FAIL dir=%s reason=%s packets=%u verify=%u bytes=%lu passes=%u mism=%lu seen=%lu",
               dir, event.reason, unsigned(event.packets), unsigned(event.verify),
               (unsigned long)event.bytes, unsigned(event.passes), (unsigned long)event.mismatches,
               (unsigned long)event.seen);
      break;
    case EventKind::None:
      out[0] = '\0';
      break;
  }
}

// ---- Requests the serial side hands to the loop, which owns the engine ----
enum class LoopCommand : uint8_t { Status, ArmIn, ArmOut, Abort, Get, Diag, Trace };

struct LoopRequest {
  LoopCommand command;
  uint8_t block;
};

class LoopPort {
 public:
  virtual void call(const LoopRequest &request, Output &out) = 0;
  virtual ~LoopPort() = default;
};

// Upload staging, owned by the serial side. The loop copies it into the
// engine only while handling ARM IN, when the serial side is waiting.
struct Staging {
  uint8_t data[IMAGE_LEN];
  bool block[BLOCK_COUNT];
  bool committed = false;
  uint32_t crc = 0;

  void reset() {
    memset(block, 0, sizeof(block));
    committed = false;
    crc = 0;
  }
};

// Runs on the loop's core: applies requests and supervises the session.
class LoopSide {
 public:
  LoopSide(BusEngine &engine, Session &session, Staging &staging)
      : engine_(engine), session_(session), staging_(staging) {}

  void handle(const LoopRequest &request, const PinLevels &pins, uint32_t nowMs, Output &out,
              Critical &critical) {
    char line[MAX_LINE];
    switch (request.command) {
      case LoopCommand::Status:
        snprintf(line, sizeof(line), "OK STATUS state=%s staged=%u staged_crc=%08lX image_crc=%08lX",
                 dirName(engine_.mode), staging_.committed ? 1u : 0u, (unsigned long)staging_.crc,
                 (unsigned long)crc32(engine_.image, IMAGE_LEN));
        out.line(line);
        break;
      case LoopCommand::ArmIn:
      case LoopCommand::ArmOut: {
        const Mode mode = request.command == LoopCommand::ArmIn ? Mode::In : Mode::Out;
        if (session_.armed()) {
          out.line("ERR ARM armed");
          break;
        }
        if (mode == Mode::In) {
          if (!staging_.committed) {
            out.line("ERR ARM not_staged");
            break;
          }
          memcpy(engine_.image, staging_.data, IMAGE_LEN);
        }
        critical.enter();
        const bool armed = session_.arm(mode, pins.sclHigh, pins.sdaHigh, pins.localHigh, nowMs);
        critical.exit();
        if (!armed) {
          // With SCL and SDA idle, a LOW drive pin that is not pulling means
          // GPIO21 is not connected to the SDA contact through its 1 kΩ.
          const bool driveFault = pins.sclHigh && pins.sdaHigh && !pins.localHigh;
          out.line(driveFault ? "ERR ARM drive_low" : "ERR ARM bus_busy");
          break;
        }
        snprintf(line, sizeof(line), "OK ARM dir=%s window_ms=%lu", dirName(mode),
                 (unsigned long)ARM_WINDOW_MS);
        out.line(line);
        break;
      }
      case LoopCommand::Abort: {
        critical.enter();
        const Event event = session_.abort();
        critical.exit();
        out.line("OK ABORT");
        emit(event, out);
        break;
      }
      case LoopCommand::Get: {
        if (session_.armed()) {
          out.line("ERR GET armed");
          break;
        }
        const uint8_t *data = engine_.image + size_t(request.block) * BLOCK_LEN;
        char hex[2 * BLOCK_LEN + 1];
        hexEncode(data, BLOCK_LEN, hex);
        snprintf(line, sizeof(line), "OK GET %u %s %08lX", unsigned(request.block), hex,
                 (unsigned long)crc32(data, BLOCK_LEN));
        out.line(line);
        break;
      }
      case LoopCommand::Diag:
        diag(pins, out);
        out.line("OK DIAG");
        break;
      case LoopCommand::Trace:
        if (session_.armed()) {
          out.line("ERR TRACE armed");
          break;
        }
        trace(out);
        out.line("OK TRACE");
        break;
    }
  }

  // Call regularly. Writes at most one event line. No interrupt masking is
  // needed: the interrupt preempts this code on the same core, 32-bit reads
  // are atomic, and disarm() clears `mode` first, after which the interrupt
  // returns immediately.
  void poll(uint32_t nowMs, Output &out) {
    emit(session_.tick(nowMs), out);
  }

  // Prints an event obtained elsewhere (the poller ticks the session itself).
  void report(const Event &event, Output &out) { emit(event, out); }

  bool armed() const { return session_.armed(); }

 private:
  BusEngine &engine_;
  Session &session_;
  Staging &staging_;

  void emit(const Event &event, Output &out) {
    if (event.kind == EventKind::None) return;
    const uint32_t crc =
        event.kind == EventKind::Done && event.dir == Mode::Out ? crc32(engine_.image, IMAGE_LEN) : 0;
    char line[MAX_LINE];
    formatEvent(event, crc, line, sizeof(line));
    out.line(line);
  }

  // One line per interrupt call: µs since the previous call, levels after the
  // call (S = SCL, D = SDA at the contact, G = GPIO21 pad, L = bridge pulling
  // SDA low), decoder phase before > after, and bits of the current byte.
  void trace(Output &out) {
    char line[MAX_LINE];
    const uint32_t head = engine_.traceHead;
    const uint32_t count = head < BusEngine::TRACE_LEN ? head : BusEngine::TRACE_LEN;
    snprintf(line, sizeof(line), "# trace: last %lu of %lu interrupt calls", (unsigned long)count,
             (unsigned long)head);
    out.line(line);
    const uint32_t perUs = engine_.ticksPerUs ? engine_.ticksPerUs : 1;
    uint32_t previous = 0;
    for (uint32_t i = 0; i < count; ++i) {
      const BusEngine::TraceEntry &e = engine_.trace[(head - count + i) % BusEngine::TRACE_LEN];
      const unsigned long dt = i == 0 ? 0 : (unsigned long)((e.ticks - previous) / perUs);
      previous = e.ticks;
      snprintf(line, sizeof(line), "# +%lu S%u D%u G%u L%u %s>%s b%u", dt, e.pins & 1,
               (e.pins >> 1) & 1, (e.pins >> 2) & 1, (e.pins >> 3) & 1,
               BusEngine::phaseName(e.before), BusEngine::phaseName(e.after), unsigned(e.bits));
      out.line(line);
    }
  }

  void diag(const PinLevels &pins, Output &out) {
    char line[MAX_LINE];
    snprintf(line, sizeof(line), "# state=%s SCL=%s SDA=%s drive=%s (%s)", dirName(engine_.mode),
             pins.sclHigh ? "HIGH" : "LOW", pins.sdaHigh ? "HIGH" : "LOW",
             pins.localHigh ? "HIGH" : "LOW", pins.released ? "released" : "pulling low");
    out.line(line);
    snprintf(line, sizeof(line), "# 0x51 address bytes: %lu | wrong direction: %s",
             (unsigned long)engine_.activity, engine_.wrongDirection ? "yes" : "no");
    out.line(line);
    const uint32_t perUs = engine_.ticksPerUs ? engine_.ticksPerUs : 1;
    const PollStats &poll = engine_.poll;
    // Hundredths of a µs, so sub-µs widths stay visible.
    auto centi = [perUs](uint64_t ticks) { return (unsigned long)(ticks * 100 / perUs); };
    const unsigned long period = poll.samples ? centi(poll.sampledTicks / poll.samples) : 0;
    const unsigned long gap = centi(poll.maxGapTicks);
    const unsigned long upkeep = centi(poll.maxUpkeepTicks);
    const unsigned long stepTime = centi(poll.maxStepTicks);
    snprintf(line, sizeof(line),
             "# sampling: %s | %lu samples, %lu.%02lu µs apart | %lu gaps over 1.5 µs, longest "
             "%lu.%02lu µs | decoder step up to %lu.%02lu µs | %lu upkeeps, up to %lu.%02lu µs | "
             "watchdog feeds %lu | serial checks %lu",
             engine_.interruptKind, (unsigned long)poll.samples, period / 100, period % 100,
             (unsigned long)poll.longGaps, gap / 100, gap % 100, stepTime / 100, stepTime % 100,
             (unsigned long)poll.upkeeps, upkeep / 100, upkeep % 100,
             (unsigned long)poll.watchdogFeeds, (unsigned long)poll.serialReturns);
    out.line(line);
    const unsigned long slowest = centi(poll.maxAnswerTicks);
    snprintf(line, sizeof(line),
             "# answers to an SCL fall: %lu at first sight, %lu after the filter | time from the "
             "last look before the fall (1 µs steps, last = longer): %lu %lu %lu %lu %lu %lu %lu "
             "%lu %lu %lu | slowest %lu.%02lu µs | taken back %lu | corrected %lu",
             (unsigned long)poll.earlyAnswers, (unsigned long)poll.plainAnswers,
             (unsigned long)poll.answerTime[0], (unsigned long)poll.answerTime[1],
             (unsigned long)poll.answerTime[2], (unsigned long)poll.answerTime[3],
             (unsigned long)poll.answerTime[4], (unsigned long)poll.answerTime[5],
             (unsigned long)poll.answerTime[6], (unsigned long)poll.answerTime[7],
             (unsigned long)poll.answerTime[8], (unsigned long)poll.answerTime[9], slowest / 100,
             slowest % 100, (unsigned long)poll.earlyUndone, (unsigned long)poll.earlyWrong);
    out.line(line);
    snprintf(line, sizeof(line),
             "# SDA up before the clock rise by (1 µs steps, last = longer): %lu %lu %lu %lu %lu %lu "
             "%lu %lu %lu %lu",
             (unsigned long)poll.sdaLead[0], (unsigned long)poll.sdaLead[1],
             (unsigned long)poll.sdaLead[2], (unsigned long)poll.sdaLead[3],
             (unsigned long)poll.sdaLead[4], (unsigned long)poll.sdaLead[5],
             (unsigned long)poll.sdaLead[6], (unsigned long)poll.sdaLead[7],
             (unsigned long)poll.sdaLead[8], (unsigned long)poll.sdaLead[9]);
    out.line(line);
    const unsigned long setup =
        poll.minSetupTicks == 0xFFFFFFFF ? 0 : centi(poll.minSetupTicks);
    snprintf(line, sizeof(line),
             "# SDA rise after the bridge lets go (1 µs steps, last = longer): %lu %lu %lu %lu %lu "
             "%lu %lu %lu %lu %lu | up at least %lu.%02lu µs before the clock | not up in time %lu | "
             "taken with the clock %lu | late rises %lu",
             (unsigned long)poll.sdaRise[0], (unsigned long)poll.sdaRise[1],
             (unsigned long)poll.sdaRise[2], (unsigned long)poll.sdaRise[3],
             (unsigned long)poll.sdaRise[4], (unsigned long)poll.sdaRise[5],
             (unsigned long)poll.sdaRise[6], (unsigned long)poll.sdaRise[7],
             (unsigned long)poll.sdaRise[8], (unsigned long)poll.sdaRise[9], setup / 100,
             setup % 100, (unsigned long)poll.sdaLate, (unsigned long)poll.sdaWithClock,
             (unsigned long)engine_.lateRises);
    out.line(line);
    const unsigned long shortestLow =
        poll.minSclLowTicks == 0xFFFFFFFF ? 0 : centi(poll.minSclLowTicks);
    snprintf(line, sizeof(line), "# shortest SCL LOW %lu.%02lu µs | SCL LOW under 6 µs: %lu",
             shortestLow / 100, shortestLow % 100, (unsigned long)poll.shortLows);
    out.line(line);
    if (engine_.bitMismatches) {
      snprintf(line, sizeof(line),
               "# first bit mismatch: image offset 0x%04lX, bit %u of the byte, bridge sent %s",
               (unsigned long)engine_.mismatchOffset, unsigned(engine_.mismatchBit),
               engine_.mismatchSentHigh ? "HIGH (line read LOW)" : "LOW (line read HIGH)");
      out.line(line);
    }
    const unsigned long shortest =
        poll.minSclHighTicks == 0xFFFFFFFF ? 0 : centi(poll.minSclHighTicks);
    snprintf(line, sizeof(line),
             "# SCL HIGH widths (0.5 µs steps, last = longer): %lu %lu %lu %lu %lu %lu %lu %lu %lu "
             "%lu | shortest %lu.%02lu µs",
             (unsigned long)poll.sclHigh[0], (unsigned long)poll.sclHigh[1],
             (unsigned long)poll.sclHigh[2], (unsigned long)poll.sclHigh[3],
             (unsigned long)poll.sclHigh[4], (unsigned long)poll.sclHigh[5],
             (unsigned long)poll.sclHigh[6], (unsigned long)poll.sclHigh[7],
             (unsigned long)poll.sclHigh[8], (unsigned long)poll.sclHigh[9], shortest / 100,
             shortest % 100);
    out.line(line);
    const struct {
      const char *name;
      const uint32_t *bins;
    } glitches[] = {{"SCL dips", poll.sclDips},
                    {"SCL spikes", poll.sclSpikes},
                    {"SDA dips", poll.sdaDips},
                    {"SDA spikes", poll.sdaSpikes}};
    for (const auto &glitch : glitches) {
      snprintf(line, sizeof(line),
               "# rejected %s (0.1 µs steps, last = longer): %lu %lu %lu %lu %lu %lu %lu %lu %lu %lu",
               glitch.name, (unsigned long)glitch.bins[0], (unsigned long)glitch.bins[1],
               (unsigned long)glitch.bins[2], (unsigned long)glitch.bins[3],
               (unsigned long)glitch.bins[4], (unsigned long)glitch.bins[5],
               (unsigned long)glitch.bins[6], (unsigned long)glitch.bins[7],
               (unsigned long)glitch.bins[8], (unsigned long)glitch.bins[9]);
      out.line(line);
    }
    snprintf(line, sizeof(line), "# SCL and SDA accepted in one step: %lu",
             (unsigned long)engine_.mergedEdges);
    out.line(line);
    snprintf(line, sizeof(line),
             "# edges %lu (SCL %lu, SDA %lu) | STARTs %lu | STOPs %lu (counted while armed)",
             (unsigned long)engine_.edges, (unsigned long)engine_.sclEdges,
             (unsigned long)engine_.sdaEdges, (unsigned long)engine_.starts, (unsigned long)engine_.stops);
    out.line(line);
    for (unsigned address = 0; address < 128; ++address) {
      const uint32_t writes = engine_.addressCounts[address][0];
      const uint32_t reads = engine_.addressCounts[address][1];
      if (writes || reads) {
        snprintf(line, sizeof(line), "# address 0x%02X: writes %lu, reads %lu", address,
                 (unsigned long)writes, (unsigned long)reads);
        out.line(line);
      }
    }
    snprintf(line, sizeof(line),
             "# out: packets %u/%u | conflicting blocks %u | verified blocks %u/%u | odd writes %lu "
             "| overlength %lu | refused reads %lu",
             unsigned(engine_.packetCount), unsigned(PACKET_COUNT), unsigned(engine_.conflictBlocks),
             unsigned(engine_.verifyCount), unsigned(BLOCK_COUNT), (unsigned long)engine_.oddWrites,
             (unsigned long)engine_.overlengthWrites, (unsigned long)engine_.refusedReads);
    out.line(line);
    snprintf(line, sizeof(line),
             "# in: pointer writes %lu | passes %u | extra write bytes %lu | past end %lu",
             (unsigned long)engine_.pointerWrites, unsigned(engine_.passes()),
             (unsigned long)engine_.extraWriteBytes, (unsigned long)engine_.pastEndRequests);
    out.line(line);
    snprintf(line, sizeof(line),
             "# bytes clocked %lu | GPIO34 bit mismatches %lu | driven ACK slots %lu | SDA LOW at "
             "contact %lu",
             (unsigned long)engine_.bytesClocked, (unsigned long)engine_.bitMismatches,
             (unsigned long)engine_.drivenAckSlots, (unsigned long)engine_.contactLowAckSlots);
    out.line(line);
    snprintf(line, sizeof(line), "# staged=%u staged_crc=%08lX", staging_.committed ? 1u : 0u,
             (unsigned long)staging_.crc);
    out.line(line);
  }
};

// Runs on the serial side: parses lines, owns the upload staging.
class CommandSide {
 public:
  CommandSide(Staging &staging, LoopPort &loop, uint32_t bootId)
      : staging_(staging), loop_(loop), bootId_(bootId) {
    staging_.reset();
  }

  // Set by the loop so uploads are refused while the engine is armed.
  volatile bool armed = false;

  void greet(Output &out) {
    char text[96];
    snprintf(text, sizeof(text), "READY bridge proto=%u fw=%s boot=%08lX", PROTOCOL_VERSION,
             FIRMWARE_VERSION, (unsigned long)bootId_);
    out.line(text);
  }

  // `line` is modified in place.
  void handleLine(char *line, Output &out) {
    char *args[4];
    size_t count = 0;
    for (char *token = strtok(line, " \t\r"); token && count < 4; token = strtok(nullptr, " \t\r")) {
      args[count++] = token;
    }
    if (count == 0) return;
    for (char *p = args[0]; *p; ++p) *p = char(toupper(static_cast<unsigned char>(*p)));
    const char *command = args[0];
    char text[160];

    if (!strcmp(command, "HELLO")) {
      snprintf(text, sizeof(text), "OK HELLO bridge proto=%u fw=%s len=%u boot=%08lX",
               PROTOCOL_VERSION, FIRMWARE_VERSION, unsigned(IMAGE_LEN), (unsigned long)bootId_);
      out.line(text);
    } else if (!strcmp(command, "PUT")) {
      put(args, count, out);
    } else if (!strcmp(command, "COMMIT")) {
      commit(args, count, out);
    } else if (!strcmp(command, "ARM")) {
      arm(args, count, out);
    } else if (!strcmp(command, "GET")) {
      uint8_t block = 0;
      if (count != 2 || !parseBlock(args[1], block)) {
        out.line("ERR GET args");
      } else if (armed) {
        out.line("ERR GET armed");
      } else {
        loop_.call({LoopCommand::Get, block}, out);
      }
    } else if (!strcmp(command, "STATUS")) {
      loop_.call({LoopCommand::Status, 0}, out);
    } else if (!strcmp(command, "ABORT")) {
      loop_.call({LoopCommand::Abort, 0}, out);
    } else if (!strcmp(command, "DIAG")) {
      loop_.call({LoopCommand::Diag, 0}, out);
    } else if (!strcmp(command, "TRACE")) {
      loop_.call({LoopCommand::Trace, 0}, out);
    } else {
      snprintf(text, sizeof(text), "ERR %.16s unknown", command);
      out.line(text);
    }
  }

 private:
  Staging &staging_;
  LoopPort &loop_;
  uint32_t bootId_;

  void put(char **args, size_t count, Output &out) {
    uint8_t block = 0;
    uint32_t crc = 0;
    if (armed) {
      out.line("ERR PUT armed");
      return;
    }
    if (count != 4 || !parseBlock(args[1], block) || !parseCrc(args[3], crc)) {
      out.line("ERR PUT args");
      return;
    }
    uint8_t data[BLOCK_LEN];
    if (!hexDecode(args[2], BLOCK_LEN, data)) {
      out.line("ERR PUT hex");
      return;
    }
    if (crc32(data, BLOCK_LEN) != crc) {
      out.line("ERR PUT crc");
      return;
    }
    memcpy(staging_.data + size_t(block) * BLOCK_LEN, data, BLOCK_LEN);
    staging_.block[block] = true;
    staging_.committed = false;
    char text[32];
    snprintf(text, sizeof(text), "OK PUT %u", unsigned(block));
    out.line(text);
  }

  void commit(char **args, size_t count, Output &out) {
    uint32_t crc = 0;
    if (armed) {
      out.line("ERR COMMIT armed");
      return;
    }
    if (count != 2 || !parseCrc(args[1], crc)) {
      out.line("ERR COMMIT args");
      return;
    }
    for (uint8_t block = 0; block < BLOCK_COUNT; ++block) {
      if (!staging_.block[block]) {
        out.line("ERR COMMIT missing");
        return;
      }
    }
    if (crc32(staging_.data, IMAGE_LEN) != crc) {
      out.line("ERR COMMIT crc");
      return;
    }
    staging_.committed = true;
    staging_.crc = crc;
    char text[40];
    snprintf(text, sizeof(text), "OK COMMIT crc=%08lX", (unsigned long)crc);
    out.line(text);
  }

  void arm(char **args, size_t count, Output &out) {
    if (count < 2) {
      out.line("ERR ARM args");
      return;
    }
    for (char *p = args[1]; *p; ++p) *p = char(toupper(static_cast<unsigned char>(*p)));
    if (armed) {
      out.line("ERR ARM armed");
      return;
    }
    if (!strcmp(args[1], "OUT") && count == 2) {
      loop_.call({LoopCommand::ArmOut, 0}, out);
      return;
    }
    uint32_t crc = 0;
    if (strcmp(args[1], "IN") || count != 3 || !parseCrc(args[2], crc)) {
      out.line("ERR ARM args");
      return;
    }
    if (!staging_.committed) {
      out.line("ERR ARM not_staged");
      return;
    }
    if (crc != staging_.crc) {
      out.line("ERR ARM crc");
      return;
    }
    loop_.call({LoopCommand::ArmIn, 0}, out);
  }
};

}  // namespace bridge

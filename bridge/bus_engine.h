/*
 * I²C target engine for the ATS-909X2 factory transfer port (target 0x51).
 *
 * It grew out of two single-purpose test sketches, one for each direction:
 *   - Out mode: the radio's DATA OUT writes
 *     752 packets (2-byte offset + 16 bytes), then verifies them with 47
 *     pointer writes and 256-byte reads. Reads are answered only from
 *     complete, conflict-free blocks.
 *   - In mode: the radio's DATA IN reads the image,
 *     normally in two passes of 47 x 256 bytes. Read-only: only the 2-byte
 *     pointer may be written.
 *
 * The radio is always the controller and owns SCL. The engine only pulls SDA
 * low (open drain) for ACK slots and 0 bits, and only while armed.
 *
 * No Arduino calls outside the small platform section, so the host
 * simulation (test/host_sim.cpp) compiles exactly this code.
 */
#pragma once

#include <stdint.h>
#include <string.h>

#ifdef ARDUINO
#include <Arduino.h>
#include "esp_cpu.h"
#include "soc/gpio_struct.h"
#else
#ifndef IRAM_ATTR
#define IRAM_ATTR
#endif
#endif

namespace bridge {

constexpr uint16_t IMAGE_LEN = 0x2F00;  // EEPROM 0x0000–0x2EFF
constexpr uint16_t BLOCK_LEN = 256;
constexpr uint8_t BLOCK_COUNT = IMAGE_LEN / BLOCK_LEN;  // 47
constexpr uint8_t PACKET_LEN = 16;
constexpr uint16_t PACKET_COUNT = IMAGE_LEN / PACKET_LEN;  // 752
constexpr uint16_t COMPLETE_BLOCK = 0xFFFF;  // 16 packets per block
constexpr uint8_t TARGET_ADDRESS = 0x51;
constexpr uint8_t WRITE_ADDRESS_BYTE = TARGET_ADDRESS << 1;
constexpr uint8_t READ_ADDRESS_BYTE = WRITE_ADDRESS_BYTE | 1;
constexpr uint8_t MAX_WRITE_LEN = 2 + PACKET_LEN;
constexpr uint32_t NO_POINTER = 0x10000;

// ---- Platform: SDA drive on GPIO21 (open drain) and a tick counter. ----
// On the ESP32 ticks are CPU cycles, which are readable from IRAM without
// touching flash (micros() lives in flash). The host simulation uses µs.
#ifdef ARDUINO
constexpr uint8_t DRIVE_PIN = 21;
constexpr uint8_t SCL_PIN = 22;
constexpr uint8_t SENSE_PIN = 34;
constexpr uint32_t DRIVE_MASK = 1UL << DRIVE_PIN;
constexpr uint32_t SCL_MASK = 1UL << SCL_PIN;
constexpr uint32_t SENSE_MASK = 1UL << (SENSE_PIN - 32);
// Always inlined into the IRAM interrupt path; no separate sections.
static inline __attribute__((always_inline)) void sdaRelease() { GPIO.out_w1ts = DRIVE_MASK; }
static inline __attribute__((always_inline)) void sdaPullLow() { GPIO.out_w1tc = DRIVE_MASK; }
static inline __attribute__((always_inline)) uint32_t nowTicks() {
  return esp_cpu_get_cycle_count();
}

#else
extern bool simTargetLow;
extern bool simScl;
extern bool simControllerLow;
extern uint32_t simMicros;
static inline void sdaRelease() { simTargetLow = false; }
static inline void sdaPullLow() { simTargetLow = true; }
static inline uint32_t nowTicks() { return simMicros; }

#endif

enum class Mode : uint8_t { Off, Out, In };

// Pin bits used by the poller.
constexpr uint8_t PIN_SCL = 1;
constexpr uint8_t PIN_SDA = 2;
constexpr uint8_t PIN_DRIVE = 4;

// Measurements from polling the bus while armed (poller.h), for DIAG.
constexpr uint8_t WIDTH_BINS = 10;
struct PollStats {
  uint32_t samples = 0;         // pin samples taken (wraps after some minutes)
  uint64_t sampledTicks = 0;    // time spent sampling
  uint32_t maxGapTicks = 0;     // longest time between two samples, own work excluded
  uint32_t longGaps = 0;        // such gaps over 1.5 µs: time taken from this core
  uint32_t maxStepTicks = 0;    // longest decoder step
  // Answers: SCL falls at which the bridge sets SDA (ACK, or a bit it sends).
  // Time from the last look at the pins before the fall to SDA being set, in
  // 1 µs steps (last bin: longer). The radio raises SCL again 12.5 µs after
  // a fall at the earliest.
  uint32_t answerTime[WIDTH_BINS] = {};
  uint32_t maxAnswerTicks = 0;
  uint32_t earlyAnswers = 0;  // given the moment the fall was first seen
  uint32_t plainAnswers = 0;  // given once the fall had passed the glitch filter
  uint32_t earlyUndone = 0;   // early answer taken back: the fall was a glitch
  uint32_t earlyWrong = 0;    // early answer differed from the decoder's, and was corrected
  // When the bridge sends a 1 after a 0 it lets go of SDA and the line rises
  // by itself. Time from letting go to seeing SDA HIGH, in 1 µs steps (last
  // bin: longer), and how long before the clock rise it was up.
  uint32_t sdaRise[WIDTH_BINS] = {};
  uint32_t sdaLead[WIDTH_BINS] = {};  // SDA up this long before the clock rise, 1 µs steps
  uint32_t minSetupTicks = 0xFFFFFFFF;
  uint32_t sdaLate = 0;       // not yet HIGH when the clock rise was first seen
  uint32_t sdaWithClock = 0;  // own SDA change still settling, taken with the clock rise
  uint32_t minSclLowTicks = 0xFFFFFFFF;
  uint32_t shortLows = 0;     // SCL LOW for under 6 µs: not a real clock phase
  uint32_t maxUpkeepTicks = 0;  // longest upkeep step (clock, watchdog, session check)
  uint32_t upkeeps = 0;
  uint32_t watchdogFeeds = 0;
  uint32_t serialReturns = 0;   // returns to loop() for serial input
  // Level changes that reverted before the glitch filter accepted them, by
  // duration in 0.1 µs steps (last bin: longer).
  uint32_t sclDips[WIDTH_BINS] = {};    // SCL briefly LOW while HIGH
  uint32_t sclSpikes[WIDTH_BINS] = {};  // SCL briefly HIGH while LOW
  uint32_t sdaDips[WIDTH_BINS] = {};
  uint32_t sdaSpikes[WIDTH_BINS] = {};
  // Accepted SCL HIGH pulses by width in 0.5 µs steps (last bin: longer).
  uint32_t sclHigh[WIDTH_BINS] = {};
  uint32_t minSclHighTicks = 0xFFFFFFFF;
};

class BusEngine {
 public:
  // Out: the image received from the radio. In: the image served to it.
  uint8_t image[IMAGE_LEN] = {};

  // Read by the loop on the same core; single 32-bit reads are atomic.
  volatile Mode mode = Mode::Off;
  volatile bool drivingLow = false;
  // The bridge has let go of SDA and the line has not been seen HIGH since.
  // Cleared at the next SCL fall.
  bool awaitingRise = false;
  // The bridge's own 1-bit came up only after the clock rise (see step).
  uint32_t lateRises = 0;
  // True when a level now showing on SDA is the bridge's own doing: it is
  // pulling the line low, or has just let go of it.
  __attribute__((always_inline)) bool ownSdaChange(bool sdaHigh) const {
    return sdaHigh ? awaitingRise : bool(drivingLow);
  }
  // True right after the bridge let go of SDA to send a 1.
  __attribute__((always_inline)) bool sendingHigh() const {
    return awaitingRise && phase == Phase::ReadData;
  }

  // What the bridge will do with SDA at the next SCL fall. It is known as
  // soon as the clock has risen, so the poller can act the moment it first
  // sees the fall; onSclFall does the same again once the fall has passed
  // the glitch filter. Must mirror onSclFall exactly.
  enum class FallAction : uint8_t {
    None,
    PullLow,   // ACK, or a 0 bit
    Release,   // let go: end of an ACK, or the controller's turn
    SendHigh,  // let go to send a 1 bit
  };
  __attribute__((always_inline)) FallAction fallAction() const {
    if (phase == Phase::AckSetup) {
      return (byteKind != ByteKind::ReadAddress || replyToRead) ? FallAction::PullLow
                                                                : FallAction::None;
    }
    if (phase == Phase::AckRelease) {
      if (byteKind == ByteKind::ReadAddress && replyToRead) return bitAction(readByte(), 0);
      return drivingLow ? FallAction::Release : FallAction::None;
    }
    if (phase == Phase::ReadData) {
      return bits == 8 ? FallAction::Release : bitAction(readByte(), bits);
    }
    if (phase == Phase::ReadAckFall && controllerAck) {
      if (mode == Mode::Out) {
        if (readIndex + 1 == BLOCK_LEN) return FallAction::None;
        return bitAction(image[readOffset + readIndex + 1], 0);
      }
      if (sourceOffset >= IMAGE_LEN) return FallAction::None;
      return bitAction(image[sourceOffset], 0);
    }
    return FallAction::None;
  }
  volatile uint32_t lowStartedTicks = 0;
  uint32_t ticksPerUs = 1;  // the ESP32 sets its CPU MHz
  volatile uint32_t activity = 0;  // address bytes for 0x51 seen while armed
  volatile bool wrongDirection = false;

  // Out mode
  uint16_t receivedPackets[BLOCK_COUNT] = {};
  bool conflictingBlock[BLOCK_COUNT] = {};
  bool verifiedBlock[BLOCK_COUNT] = {};
  volatile uint16_t packetCount = 0;
  volatile uint8_t conflictBlocks = 0;
  volatile uint8_t verifyCount = 0;
  volatile uint32_t oddWrites = 0;
  volatile uint32_t overlengthWrites = 0;
  volatile uint32_t refusedReads = 0;

  // In mode
  uint16_t servedBytes[BLOCK_COUNT] = {};
  volatile uint32_t pointerWrites = 0;
  volatile uint32_t extraWriteBytes = 0;
  volatile uint32_t pastEndRequests = 0;

  // Both
  volatile uint32_t bytesClocked = 0;
  volatile uint32_t bitMismatches = 0;
  // The first bit that read back wrong while sending: where, and what was sent.
  uint32_t mismatchOffset = 0;
  uint8_t mismatchBit = 0;
  bool mismatchSentHigh = false;
  volatile uint32_t drivenAckSlots = 0;
  volatile uint32_t contactLowAckSlots = 0;

  // Diagnostics: bus activity while armed, for DIAG.
  volatile uint32_t edges = 0;     // accepted pin changes handed to the decoder
  volatile uint32_t sclEdges = 0;  // near zero means SCL is not connected
  volatile uint32_t sdaEdges = 0;
  volatile uint32_t mergedEdges = 0;  // SCL and SDA changed in one step
  PollStats poll;
  const char *interruptKind = "simulated";
  volatile uint32_t starts = 0;  // START and repeated START conditions
  volatile uint32_t stops = 0;
  uint32_t addressCounts[128][2] = {};  // [7-bit address][0 = write, 1 = read]

  // Call with interrupts masked. Requires an idle bus: SCL, SDA at the
  // contact and the local drive pin all HIGH.
  bool arm(Mode newMode, bool sclHigh, bool sdaHigh, bool localHigh) {
    if (newMode == Mode::Off || !(sclHigh && sdaHigh && localHigh)) return false;
    resetCounters();
    if (newMode == Mode::Out) memset(image, 0, sizeof(image));
    previousSclHigh = sclHigh;
    previousSdaHigh = sdaHigh;
    phase = Phase::Idle;
    mode = newMode;
    return true;
  }

  // Call with interrupts masked.
  __attribute__((always_inline)) void disarm() {
    mode = Mode::Off;
    release();
    phase = Phase::Idle;
  }

  // Complete passes over the image while serving DATA IN.
  __attribute__((always_inline)) uint8_t passes() const {
    uint16_t least = 0xFFFF;
    for (uint8_t b = 0; b < BLOCK_COUNT; ++b) {
      if (servedBytes[b] < least) least = servedBytes[b];
    }
    return least / BLOCK_LEN;
  }

  // Hands the decoder a new pin state: any SCL or SDA-contact change.
  void onBusChange(bool sclHigh, bool sdaHigh, bool localHigh);

  // The pin state the decoder last saw (PIN_SCL | PIN_SDA).
  uint8_t seenPins() const {
    return (previousSclHigh ? PIN_SCL : 0) | (previousSdaHigh ? PIN_SDA : 0);
  }

  // Takes the bus as it is now, without reading anything into how it got
  // there. Used when watching resumes after a break: whatever transaction
  // is under way is skipped, and decoding starts at the next START.
  void resync(bool sclHigh, bool sdaHigh) {
    if (sclHigh == previousSclHigh && sdaHigh == previousSdaHigh) return;
    previousSclHigh = sclHigh;
    previousSdaHigh = sdaHigh;
    if (!drivingLow) phase = Phase::Idle;
    awaitingRise = false;
  }

  // Trace of the most recent interrupt calls while armed, for TRACE. It stops
  // with the bridge disarming, so after a failure it ends at the failure.
  struct TraceEntry {
    uint32_t ticks;
    uint8_t pins;    // bit 0 SCL, 1 SDA at the contact, 2 GPIO21 pad, 3 bridge pulling SDA low
    uint8_t before;  // decoder phase before and after the call
    uint8_t after;
    uint8_t bits;
  };
  static constexpr uint32_t TRACE_LEN = 4096;  // several packets
  TraceEntry trace[TRACE_LEN] = {};
  volatile uint32_t traceHead = 0;  // calls recorded since arming
  // Recording stops here: shortly after the first bit mismatch, so the trace
  // keeps the moment it happened.
  uint32_t traceStop = 0xFFFFFFFF;
  static constexpr uint32_t TRACE_AFTER_MISMATCH = 512;

  static const char *phaseName(uint8_t phase) {
    static const char *const names[] = {"Idle",       "Address", "Data",     "AckSetup",   "AckRise",
                                        "AckRelease", "ReadData", "ReadAck", "ReadAckFall"};
    return phase < sizeof(names) / sizeof(names[0]) ? names[phase] : "?";
  }

 private:
  enum class Phase : uint8_t {
    Idle, Address, Data, AckSetup, AckRise, AckRelease, ReadData, ReadAck, ReadAckFall
  };
  enum class ByteKind : uint8_t { WriteAddress, WriteData, ReadAddress };

  Phase phase = Phase::Idle;
  ByteKind byteKind = ByteKind::WriteAddress;
  bool previousSclHigh = true;
  bool previousSdaHigh = true;
  uint8_t bits = 0;
  uint8_t incoming = 0;
  bool outgoingBitHigh = true;
  bool replyToRead = false;
  bool controllerAck = false;

  // Current write
  uint8_t writeBuf[MAX_WRITE_LEN] = {};
  uint8_t writeLen = 0;
  bool writePending = false;
  bool writeAddressAcked = false;

  // Out: pointer from a 2-byte write, used by the next read
  uint32_t requestedOffset = NO_POINTER;
  uint32_t readOffset = 0;
  uint16_t readIndex = 0;
  uint16_t readBytes = 0;
  uint32_t readMismatches = 0;

  // In: EEPROM-style pointer that auto-increments across reads
  uint16_t sourceOffset = 0;
  uint16_t pendingPointer = 0;
  bool pointerComplete = true;

  void resetCounters() {
    memset(receivedPackets, 0, sizeof(receivedPackets));
    memset(conflictingBlock, 0, sizeof(conflictingBlock));
    memset(verifiedBlock, 0, sizeof(verifiedBlock));
    memset(servedBytes, 0, sizeof(servedBytes));
    activity = 0;
    edges = 0;
    sclEdges = 0;
    sdaEdges = 0;
    mergedEdges = 0;
    poll = PollStats();
    traceHead = 0;
    starts = 0;
    stops = 0;
    memset(addressCounts, 0, sizeof(addressCounts));
    wrongDirection = false;
    packetCount = 0;
    conflictBlocks = 0;
    verifyCount = 0;
    oddWrites = 0;
    overlengthWrites = 0;
    refusedReads = 0;
    pointerWrites = 0;
    extraWriteBytes = 0;
    pastEndRequests = 0;
    bytesClocked = 0;
    bitMismatches = 0;
    lateRises = 0;
    awaitingRise = false;
    traceStop = 0xFFFFFFFF;
    drivenAckSlots = 0;
    contactLowAckSlots = 0;
    bits = 0;
    incoming = 0;
    writeLen = 0;
    writePending = false;
    writeAddressAcked = false;
    requestedOffset = NO_POINTER;
    sourceOffset = 0;
    pendingPointer = 0;
    pointerComplete = true;
    replyToRead = false;
  }

  void step(bool sclHigh, bool sdaHigh, bool localHigh);

  // The byte being sent to the radio; right after a read address, the first one.
  __attribute__((always_inline)) uint8_t readByte() const {
    return mode == Mode::Out ? image[readOffset + readIndex] : image[sourceOffset];
  }
  static __attribute__((always_inline)) FallAction bitAction(uint8_t value, uint8_t bit) {
    return (value & (0x80U >> bit)) ? FallAction::SendHigh : FallAction::PullLow;
  }

  void release();

  void pullLow();

  void beginWrite();

  // Out mode: a finished write is either a read pointer or a 16-byte packet.
  void finishWrite();

  // Whether to ACK another data byte of the current write.
  bool acceptsWriteByte();

  void storeWriteByte(uint8_t value);

  void beginRead();

  void endRead();

  void setReadBit();

  void onSclRise(bool sdaHigh, bool localHigh);

  void onSclFall();
};

// Interrupt-path definitions live outside the class body: in-class
// IRAM_ATTR members are implicitly inline and break the Xtensa linker
// ("l32r: literal placed after use"). Include this header in one translation unit.

// One observation at a time.
void IRAM_ATTR BusEngine::onBusChange(bool sclHigh, bool sdaHigh, bool localHigh) {
  if (mode == Mode::Off) return;
  step(sclHigh, sdaHigh, localHigh);
}

void IRAM_ATTR BusEngine::step(bool sclHigh, bool sdaHigh, bool localHigh) {
  ++edges;
  const uint8_t phaseBefore = uint8_t(phase);
  const bool sclChanged = sclHigh != previousSclHigh;
  const bool sdaChanged = sdaHigh != previousSdaHigh;
  previousSclHigh = sclHigh;
  previousSdaHigh = sdaHigh;
  if (sclChanged) ++sclEdges;
  if (sdaChanged) ++sdaEdges;
  if (sclChanged && sdaChanged) ++mergedEdges;

  if (sdaHigh) awaitingRise = false;
  if (sclHigh && !sclChanged && sdaChanged && sdaHigh && phase == Phase::ReadData &&
      outgoingBitHigh) {
    // The bridge is sending a 1 here, so no STOP is possible: this is its own
    // line coming up after the clock rise. (Hardware runs of 2026-11-12: taken
    // for a STOP, it ended the radio's verify read in the middle of a block.)
    ++lateRises;
  } else if (sclHigh && !sclChanged && sdaChanged && !drivingLow) {
    // STOP or (repeated) START finishes the preceding write.
    finishWrite();
    // SDA falling while SCL stays high starts a new address.
    phase = sdaHigh ? Phase::Idle : Phase::Address;
    if (sdaHigh) {
      ++stops;
    } else {
      ++starts;
    }
    bits = 0;
    incoming = 0;
  } else if (sclChanged) {
    if (sclHigh) {
      onSclRise(sdaHigh, localHigh);
    } else {
      awaitingRise = false;
      onSclFall();
    }
  }

  if (traceHead >= traceStop) return;
  TraceEntry &entry = trace[traceHead % TRACE_LEN];
  entry.ticks = nowTicks();
  entry.pins = uint8_t((sclHigh ? 1 : 0) | (sdaHigh ? 2 : 0) | (localHigh ? 4 : 0) |
                       (drivingLow ? 8 : 0));
  entry.before = phaseBefore;
  entry.after = uint8_t(phase);
  entry.bits = bits;
  traceHead = traceHead + 1;
}

void IRAM_ATTR BusEngine::release() {
  sdaRelease();
  if (drivingLow) awaitingRise = true;
  drivingLow = false;
}

void IRAM_ATTR BusEngine::pullLow() {
  sdaPullLow();
  lowStartedTicks = nowTicks();
  drivingLow = true;
}

void IRAM_ATTR BusEngine::beginWrite() {
  byteKind = ByteKind::WriteAddress;
  writeLen = 0;
  writeAddressAcked = false;
  pendingPointer = 0;
  if (mode == Mode::Out) {
    requestedOffset = NO_POINTER;
    writePending = true;
  }
}

void IRAM_ATTR BusEngine::finishWrite() {
  if (!writePending) return;
  writePending = false;
  if (!writeAddressAcked || writeLen < 2) return;

  const uint16_t offset = (uint16_t(writeBuf[0]) << 8) | writeBuf[1];
  if (writeLen == 2) {
    requestedOffset = offset;
    return;
  }
  if (writeLen != MAX_WRITE_LEN || offset >= IMAGE_LEN || offset % PACKET_LEN != 0) {
    ++oddWrites;
    return;
  }

  const uint8_t block = offset / BLOCK_LEN;
  const uint16_t packet = 1U << ((offset % BLOCK_LEN) / PACKET_LEN);
  if (receivedPackets[block] & packet) {
    for (uint8_t i = 0; i < PACKET_LEN; ++i) {
      if (image[offset + i] != writeBuf[i + 2] && !conflictingBlock[block]) {
        conflictingBlock[block] = true;
        ++conflictBlocks;
      }
    }
    return;
  }
  for (uint8_t i = 0; i < PACKET_LEN; ++i) image[offset + i] = writeBuf[i + 2];
  receivedPackets[block] |= packet;
  ++packetCount;
}

bool IRAM_ATTR BusEngine::acceptsWriteByte() {
  if (mode == Mode::Out) {
    if (writeLen < MAX_WRITE_LEN) return true;
    ++overlengthWrites;
    return false;
  }
  if (writeLen < 2) return true;
  // In mode is read-only: data after the pointer means the radio is
  // sending (DATA OUT) while a write to it was expected.
  ++extraWriteBytes;
  pointerComplete = false;
  wrongDirection = true;
  return false;
}

void IRAM_ATTR BusEngine::storeWriteByte(uint8_t value) {
  writeBuf[writeLen++] = value;
  if (mode == Mode::In) {
    pendingPointer = (pendingPointer << 8) | value;
    pointerComplete = writeLen == 2;
    if (pointerComplete) {
      sourceOffset = pendingPointer;
      ++pointerWrites;
    }
  }
}

void IRAM_ATTR BusEngine::beginRead() {
  byteKind = ByteKind::ReadAddress;
  readBytes = 0;
  readMismatches = 0;
  readIndex = 0;
  if (mode == Mode::Out) {
    // Reads before any packet mean the radio is importing (DATA IN).
    if (packetCount == 0) wrongDirection = true;
    readOffset = requestedOffset;
    requestedOffset = NO_POINTER;
    replyToRead = false;
    if (readOffset < IMAGE_LEN && readOffset % BLOCK_LEN == 0) {
      const uint8_t block = readOffset / BLOCK_LEN;
      replyToRead = receivedPackets[block] == COMPLETE_BLOCK && !conflictingBlock[block];
    }
  } else {
    replyToRead = pointerComplete && sourceOffset < IMAGE_LEN;
  }
  if (!replyToRead) ++refusedReads;
}

void IRAM_ATTR BusEngine::endRead() {
  if (mode != Mode::Out) return;
  if (readBytes == BLOCK_LEN && readMismatches == 0) {
    const uint8_t block = readOffset / BLOCK_LEN;
    if (!verifiedBlock[block]) {
      verifiedBlock[block] = true;
      ++verifyCount;
    }
  }
}

void IRAM_ATTR BusEngine::setReadBit() {
  outgoingBitHigh = (readByte() & (0x80U >> bits)) != 0;
  if (outgoingBitHigh) {
    release();
  } else {
    pullLow();
  }
}

void IRAM_ATTR BusEngine::onSclRise(bool sdaHigh, bool localHigh) {
  (void)localHigh;
  if (phase == Phase::ReadData) {
    if (sdaHigh != outgoingBitHigh) {
      if (bitMismatches == 0) {
        mismatchOffset = mode == Mode::Out ? readOffset + readIndex : sourceOffset;
        mismatchBit = bits;
        mismatchSentHigh = outgoingBitHigh;
        traceStop = traceHead + TRACE_AFTER_MISMATCH;
      }
      ++bitMismatches;
      ++readMismatches;
    }
    ++bits;
    return;
  }
  if (phase == Phase::ReadAck) {
    ++bytesClocked;
    if (mode == Mode::Out) {
      ++readBytes;
    } else {
      ++servedBytes[sourceOffset / BLOCK_LEN];
      ++sourceOffset;
    }
    controllerAck = !sdaHigh;
    phase = Phase::ReadAckFall;
    return;
  }
  if (phase == Phase::AckRise) {
    if (drivingLow) {
      ++drivenAckSlots;
      if (!sdaHigh) ++contactLowAckSlots;
    }
    if (byteKind == ByteKind::ReadAddress) {
      replyToRead = drivingLow && !sdaHigh;
      phase = drivingLow ? Phase::AckRelease : Phase::Idle;
      return;
    }
    if (byteKind == ByteKind::WriteAddress) {
      writeAddressAcked = !sdaHigh;
    } else {
      storeWriteByte(incoming);
    }
    phase = Phase::AckRelease;
    return;
  }

  if (phase != Phase::Address && phase != Phase::Data) return;
  incoming = (incoming << 1) | (sdaHigh ? 1 : 0);
  if (++bits < 8) return;

  if (phase == Phase::Address) {
    ++addressCounts[incoming >> 1][incoming & 1];
    if (incoming == READ_ADDRESS_BYTE) {
      ++activity;
      beginRead();
    } else if (incoming == WRITE_ADDRESS_BYTE) {
      ++activity;
      beginWrite();
    } else {
      phase = Phase::Idle;  // another device: never ACK
      return;
    }
  } else {
    if (!acceptsWriteByte()) {
      phase = Phase::Idle;  // NACK
      return;
    }
    byteKind = ByteKind::WriteData;
  }
  phase = Phase::AckSetup;
}

void IRAM_ATTR BusEngine::onSclFall() {
  if (phase == Phase::AckSetup) {
    if (byteKind != ByteKind::ReadAddress || replyToRead) pullLow();
    phase = Phase::AckRise;
  } else if (phase == Phase::AckRelease) {
    bits = 0;
    incoming = 0;
    if (byteKind == ByteKind::ReadAddress && replyToRead) {
      setReadBit();
      phase = Phase::ReadData;
    } else {
      if (drivingLow) release();
      phase = byteKind == ByteKind::ReadAddress ? Phase::Idle : Phase::Data;
    }
  } else if (phase == Phase::ReadData) {
    if (bits == 8) {
      // The controller owns SDA for the ninth clock of each read byte.
      release();
      phase = Phase::ReadAck;
    } else {
      setReadBit();
    }
  } else if (phase == Phase::ReadAckFall) {
    if (mode == Mode::Out) {
      if (!controllerAck || readIndex + 1 == BLOCK_LEN) {
        endRead();
        phase = Phase::Idle;
      } else {
        ++readIndex;
        bits = 0;
        setReadBit();
        phase = Phase::ReadData;
      }
    } else if (!controllerAck) {
      phase = Phase::Idle;
    } else if (sourceOffset >= IMAGE_LEN) {
      // No data past the image: leave SDA released.
      ++pastEndRequests;
      phase = Phase::Idle;
    } else {
      bits = 0;
      setReadBit();
      phase = Phase::ReadData;
    }
  }
}

}  // namespace bridge

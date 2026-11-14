// Host simulation of the bridge firmware: a simulated radio (I²C controller)
// clocks bit by bit against the real engine, session and protocol code.
//
// Build and run: test/run-host-sim.sh (from anywhere).

#include <algorithm>
#include <cstdio>
#include <fstream>
#include <functional>
#include <iterator>
#include <random>
#include <string>
#include <vector>

namespace bridge {
bool simTargetLow = false;
bool simScl = true;
bool simControllerLow = false;
uint32_t simMicros = 0;
struct PollHost;
PollHost *pollHost = nullptr;
}  // namespace bridge

#include "../bus_engine.h"
#include "../poller.h"
#include "../protocol.h"
#include "../session.h"

using namespace bridge;
using Bytes = std::vector<uint8_t>;

static int failures = 0;
static int checks = 0;
// BusEngine::fallAction must foretell what onSclFall does with SDA. Checked at
// every SCL fall of the bit-by-bit bus, in every test below.
static int fallActions = 0;
static int fallActionMismatches = 0;
#define CHECK(cond)                                                     \
  do {                                                                  \
    ++checks;                                                           \
    if (!(cond)) {                                                      \
      ++failures;                                                       \
      std::printf("  FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);     \
    }                                                                   \
  } while (0)

// ---- Simulated bus: wired-AND SDA, interrupts on SCL and SDA-contact changes ----
// SCL and the controller's SDA drive live in the globals the engine samples.
struct Bus {
  BusEngine &engine;
  bool &scl = simScl;
  bool &controllerLow = simControllerLow;
  bool lastContact = true;

  explicit Bus(BusEngine &e) : engine(e) {
    simScl = true;
    simControllerLow = false;
  }

  bool contact() const { return !(controllerLow || simTargetLow); }

  void interrupt() {
    // The decoder is told again when its own drive changes the contact.
    for (int i = 0; i < 4; ++i) {
      const bool level = contact();
      engine.onBusChange(scl, level, !simTargetLow);
      lastContact = level;
      if (contact() == level) return;
    }
  }
  void setScl(bool high) {
    const bool falls = scl && !high && engine.mode != Mode::Off;
    const BusEngine::FallAction action = falls ? engine.fallAction() : BusEngine::FallAction::None;
    const bool lowBefore = simTargetLow;
    scl = high;
    simMicros += 36;
    interrupt();
    if (!falls || engine.mode == Mode::Off) return;
    const bool expected = action == BusEngine::FallAction::None ? lowBefore
                                                                : action == BusEngine::FallAction::PullLow;
    if (action != BusEngine::FallAction::None) ++fallActions;
    if (simTargetLow != expected) ++fallActionMismatches;
  }
  void setSda(bool low) {
    const bool before = contact();
    controllerLow = low;
    simMicros += 2;
    if (contact() != before) interrupt();
  }

  void start() {
    setSda(true);
    setScl(false);
  }
  void repeatedStart() {
    setSda(false);
    setScl(true);
    setSda(true);
    setScl(false);
  }
  void stop() {
    setSda(true);
    setScl(true);
    setSda(false);
  }
  void writeBit(bool bit) {
    setSda(!bit);
    setScl(true);
    setScl(false);
  }
  bool readBit() {
    setSda(false);
    setScl(true);
    const bool level = contact();
    setScl(false);
    return level;
  }
  // Returns true when the target ACKs.
  bool writeByte(uint8_t value) {
    for (int bit = 7; bit >= 0; --bit) writeBit((value >> bit) & 1);
    return !readBit();
  }
  uint8_t readByte(bool ack) {
    uint8_t value = 0;
    for (int bit = 0; bit < 8; ++bit) value = uint8_t((value << 1) | (readBit() ? 1 : 0));
    writeBit(!ack);
    return value;
  }
};

// ---- What the radio does ----
struct Radio {
  Bus &bus;

  bool writePacket(uint16_t offset, const uint8_t *data, uint8_t len) {
    bus.start();
    bool acked = bus.writeByte(WRITE_ADDRESS_BYTE);
    acked = acked && bus.writeByte(offset >> 8) && bus.writeByte(offset & 0xFF);
    for (uint8_t i = 0; acked && i < len; ++i) acked = bus.writeByte(data[i]);
    bus.stop();
    return acked;
  }

  // Pointer write, then a read of `len` bytes (ACK all but the last).
  bool readBlock(uint8_t address, uint16_t offset, size_t len, Bytes &out, bool repeated) {
    bus.start();
    bool acked = bus.writeByte(address << 1) && bus.writeByte(offset >> 8) &&
                 bus.writeByte(offset & 0xFF);
    if (repeated) {
      bus.repeatedStart();
    } else {
      bus.stop();
      bus.start();
    }
    acked = acked && bus.writeByte(uint8_t((address << 1) | 1));
    out.clear();
    if (acked) {
      for (size_t i = 0; i < len; ++i) out.push_back(bus.readByte(i + 1 < len));
    }
    bus.stop();
    return acked;
  }

  // DATA OUT: 752 packets, then 47 verify reads.
  bool dataOut(const Bytes &image, const std::vector<uint16_t> &order, Bytes &readback) {
    bool ok = true;
    for (uint16_t packet : order) {
      ok = writePacket(packet * PACKET_LEN, &image[packet * PACKET_LEN], PACKET_LEN) && ok;
    }
    readback.clear();
    for (uint8_t block = 0; block < BLOCK_COUNT; ++block) {
      Bytes data;
      ok = readBlock(TARGET_ADDRESS, block * BLOCK_LEN, BLOCK_LEN, data, true) && ok;
      readback.insert(readback.end(), data.begin(), data.end());
    }
    return ok;
  }

  // DATA IN: two passes of 47 pointer writes and 256-byte reads.
  bool dataIn(Bytes &pass1, Bytes &pass2, bool repeated) {
    bool ok = true;
    for (Bytes *pass : {&pass1, &pass2}) {
      pass->clear();
      for (uint8_t block = 0; block < BLOCK_COUNT; ++block) {
        Bytes data;
        ok = readBlock(TARGET_ADDRESS, block * BLOCK_LEN, BLOCK_LEN, data, repeated) && ok;
        pass->insert(pass->end(), data.begin(), data.end());
      }
    }
    return ok;
  }
};

class NoMask : public Critical {
 public:
  void enter() override {}
  void exit() override {}
};

class Lines : public Output {
 public:
  std::vector<std::string> lines;
  void line(const char *text) override { lines.emplace_back(text); }
  std::string last() const { return lines.empty() ? "" : lines.back(); }
};

// Direct loop port for the host: the "other core" is just a function call.
class DirectPort : public LoopPort {
 public:
  LoopSide &loop;
  PinLevels pins{true, true, true};
  uint32_t nowMs = 0;
  explicit DirectPort(LoopSide &l) : loop(l) {}
  void call(const LoopRequest &request, Output &out) override {
    NoMask mask;
    loop.handle(request, pins, nowMs, out, mask);
  }
};

struct Rig {
  BusEngine engine;
  Session session{engine};
  Staging staging;
  LoopSide loop{engine, session, staging};
  DirectPort port{loop};
  CommandSide commands{staging, port, 0x12345678};
  Bus bus{engine};
  Radio radio{bus};
  Lines out;
  uint32_t nowMs = 1000;

  Rig() { simTargetLow = false; }

  std::string send(const std::string &line) {
    std::vector<char> buffer(line.begin(), line.end());
    buffer.push_back('\0');
    commands.armed = loop.armed();
    port.nowMs = nowMs;
    const size_t before = out.lines.size();
    commands.handleLine(buffer.data(), out);
    return out.lines.size() > before ? out.lines[before] : "";
  }

  // Advances simulated time, polling like loop() does.
  std::vector<std::string> run(uint32_t ms) {
    std::vector<std::string> events;
    for (uint32_t t = 0; t < ms; ++t) {
      ++nowMs;
      simMicros += 1000;
      Lines eventOut;
      loop.poll(nowMs, eventOut);
      events.insert(events.end(), eventOut.lines.begin(), eventOut.lines.end());
    }
    return events;
  }

  // Poll right after bus activity so the session sees it as recent.
  void touch() { run(1); }

  bool upload(const Bytes &image) {
    for (uint8_t block = 0; block < BLOCK_COUNT; ++block) {
      char hex[2 * BLOCK_LEN + 1];
      hexEncode(&image[block * BLOCK_LEN], BLOCK_LEN, hex);
      char line[MAX_LINE];
      snprintf(line, sizeof(line), "PUT %u %s %08X", unsigned(block), hex,
               unsigned(crc32(&image[block * BLOCK_LEN], BLOCK_LEN)));
      if (send(line).rfind("OK PUT", 0) != 0) return false;
    }
    char line[64];
    snprintf(line, sizeof(line), "COMMIT %08X", unsigned(crc32(image.data(), image.size())));
    return send(line).rfind("OK COMMIT", 0) == 0;
  }

  Bytes download() {
    Bytes image;
    for (uint8_t block = 0; block < BLOCK_COUNT; ++block) {
      const std::string reply = send("GET " + std::to_string(block));
      char hex[2 * BLOCK_LEN + 1] = {0};
      unsigned number = 0, crc = 0;
      if (sscanf(reply.c_str(), "OK GET %u %512s %8X", &number, hex, &crc) != 3) return {};
      uint8_t data[BLOCK_LEN];
      if (!hexDecode(hex, BLOCK_LEN, data) || crc32(data, BLOCK_LEN) != crc) return {};
      image.insert(image.end(), data, data + BLOCK_LEN);
    }
    return image;
  }
};

// ---- Time-based waveform for the poller (times in 0.01 µs) ----
constexpr uint8_t GLITCH_SCL = 16;  // crosstalk pulling SCL low for a moment
constexpr uint8_t GLITCH_SDA = 32;
constexpr uint8_t RADIO_SAMPLE = 64;  // the radio reads SDA here (a bit of a read)

struct Edge {
  uint32_t t;
  uint8_t pin;  // PIN_SCL, PIN_SDA (the sender's drive), GLITCH_SCL or GLITCH_SDA
  bool high;    // for glitches: true = glitch over
};

// The radio's timing as measured by the poller on 2026-11-12: START hold
// 9 µs, SCL HIGH about 15 µs, LOW 13 µs or more, SDA changes within about
// 1 µs of an SCL fall, STOP 18 µs after the last rise, 1.63 ms between
// transactions. Optionally with crosstalk: SCL dips when SDA falls while SCL
// is HIGH, and SDA dips when SCL falls while SDA is HIGH.
struct Waveform {
  std::vector<Edge> edges;
  uint32_t t = 100000;
  uint32_t highTime = 1500;
  uint32_t lowTime = 1300;
  uint32_t dataDelay = 100;  // SDA changes this long after an SCL fall
  uint32_t glitch = 0;       // crosstalk glitch width, 0 = none
  uint32_t sampleDelay = 0;  // the radio reads SDA this long after raising SCL
  uint32_t readDip = 0;      // an SCL dip this wide in the HIGH phase of every bit of a read ...
  uint32_t readDipAt = 0;    // ... this long after the clock rise
  bool scl = true, sda = true;
  void put(uint8_t pin, bool high) { edges.push_back({t, pin, high}); }
  void setSda(bool high) {
    if (high == sda) return;
    sda = high;
    put(PIN_SDA, high);
    if (!high && scl && glitch) {  // SDA falling couples a dip into SCL
      edges.push_back({t + 2, GLITCH_SCL, false});
      edges.push_back({t + 2 + glitch, GLITCH_SCL, true});
    }
  }
  void setScl(bool high) {
    if (high == scl) return;
    scl = high;
    put(PIN_SCL, high);
    if (!high && sda && glitch) {  // SCL falling couples a dip into SDA
      edges.push_back({t + 2, GLITCH_SDA, false});
      edges.push_back({t + 2 + glitch, GLITCH_SDA, true});
    }
  }
  void start() {
    setSda(false);
    t += 900;
    setScl(false);
  }
  void bit(bool one, bool sample = false) {
    t += dataDelay;
    setSda(one);
    t += lowTime - dataDelay;
    setScl(true);
    if (sample) edges.push_back({t + sampleDelay, RADIO_SAMPLE, true});
    if (sample && readDip) {
      edges.push_back({t + readDipAt, GLITCH_SCL, false});
      edges.push_back({t + readDipAt + readDip, GLITCH_SCL, true});
    }
    t += highTime;
    setScl(false);
  }
  void byte(uint8_t value) {
    for (int b = 7; b >= 0; --b) bit((value >> b) & 1);
    bit(true);  // release SDA for the ACK
  }
  void stop() {
    t += dataDelay;
    setSda(false);
    t += lowTime - dataDelay;
    setScl(true);
    t += 1800;
    setSda(true);
    t += 163000;  // 1.63 ms to the next transaction
  }
  void packet(uint16_t offset, const uint8_t *data) {
    start();
    byte(WRITE_ADDRESS_BYTE);
    byte(uint8_t(offset >> 8));
    byte(uint8_t(offset));
    for (int i = 0; i < PACKET_LEN; ++i) byte(data[i]);
    stop();
  }
  // The radio's verify read: pointer write, repeated START, then `count`
  // bytes, ACKing all but the last.
  void readBlock(uint16_t offset, size_t count, bool repeated = true) {
    start();
    byte(WRITE_ADDRESS_BYTE);
    byte(uint8_t(offset >> 8));
    byte(uint8_t(offset));
    if (repeated) {
      t += dataDelay;
      setSda(true);
      t += lowTime - dataDelay;
      setScl(true);
      t += 900;
    } else {
      stop();
    }
    start();
    byte(READ_ADDRESS_BYTE);
    for (size_t i = 0; i < count; ++i) {
      for (int b = 0; b < 8; ++b) bit(true, true);
      bit(i + 1 == count);  // ACK (LOW) for all but the last byte
    }
    stop();
  }
  // A write of `count` bytes to another device, such as the radio's EEPROM.
  void otherWrite(uint8_t address, const uint8_t *data, size_t count) {
    start();
    byte(uint8_t(address << 1));
    for (size_t i = 0; i < count; ++i) byte(data[i]);
    stop();
  }
  void idle(uint32_t time) {
    t += time;
    put(PIN_SCL, true);
  }
  std::vector<Edge> sorted() const {
    std::vector<Edge> out = edges;
    std::stable_sort(out.begin(), out.end(), [](const Edge &a, const Edge &b) { return a.t < b.t; });
    return out;
  }
};

// Plays a waveform to the poller. Each sample reads SDA first and SCL
// `skew` later, like the two register reads on the ESP32.
struct TestPoll : PollHost {
  std::vector<Edge> wave;
  Session *session = nullptr;
  uint32_t samplePeriod = 36;  // 0.36 µs, as measured on the board
  uint32_t skew = 12;
  bool sdaFirst = true;
  size_t next = 0;
  uint32_t t = 0;
  bool scl = true;          // as driven by the radio
  bool sdaReleased = true;  // as driven by the sender
  bool sclGlitch = false, sdaGlitch = false;
  uint32_t feeds = 0;
  uint32_t lastFeed = 0;
  uint32_t longestUnfed = 0;
  // A line the bridge lets go of takes this long to read HIGH (at the bridge
  // and at the radio alike).
  uint32_t riseTime = 0;
  uint32_t slowFrom = 0;  // only for lines let go at or after this time
  bool targetWasLow = false, rising = false;
  uint32_t releasedAt = 0;
  // Time taken from the core now and then: every `stealEvery`, a sample comes
  // `stealTime` late (on the ESP32: up to 4.2 µs, about 75 times a second).
  uint32_t stealEvery = 0, stealTime = 0, nextSteal = 0, steals = 0;
  bool stealTwice = false, stealAgain = false;  // a second theft one sample after the first
  // One upkeep step of the poller takes this long.
  uint32_t upkeepCost = 0;
  uint32_t upkeeps = 0, sclFalls = 0, fallsDuringUpkeep = 0;
  std::vector<bool> sampled;  // what the radio read, bit by bit
  bool risenBy(uint32_t when) const { return !rising || when - releasedAt >= riseTime; }
  void advance() {
    while (next < wave.size() && wave[next].t <= t) {
      const Edge &edge = wave[next++];
      if (edge.pin == PIN_SCL && scl && !edge.high) ++sclFalls;
      if (edge.pin == RADIO_SAMPLE) {
        sampled.push_back(sdaReleased && !sdaGlitch && !simTargetLow && risenBy(edge.t));
      }
      if (edge.pin == PIN_SCL) scl = edge.high;
      if (edge.pin == PIN_SDA) sdaReleased = edge.high;
      if (edge.pin == GLITCH_SCL) sclGlitch = !edge.high;
      if (edge.pin == GLITCH_SDA) sdaGlitch = !edge.high;
    }
    simMicros = t / 100;
    // Safety net: never let a broken poll loop spin forever.
    if (session && !wave.empty() && t > wave.back().t + 100000000) session->abort();
  }
  bool sclLevel() const { return scl && !sclGlitch; }
  bool sdaLevel() const { return sdaReleased && !sdaGlitch && !simTargetLow && risenBy(t); }
  uint32_t ticks() override { return t; }
  uint8_t pins() override {
    // The decoder ran since the last sample: note when it let go of SDA.
    if (targetWasLow && !simTargetLow) {
      rising = riseTime != 0 && t >= slowFrom;
      releasedAt = t;
    }
    targetWasLow = simTargetLow;
    if (stealAgain) {
      stealAgain = false;
      t += stealTime;
    } else if (stealEvery && t >= nextSteal) {
      t += stealTime;
      nextSteal = t + stealEvery;
      ++steals;
      stealAgain = stealTwice;
    }
    t += samplePeriod - skew;
    advance();
    const bool first = sdaFirst ? sdaLevel() : sclLevel();
    t += skew;
    advance();
    const bool second = sdaFirst ? sclLevel() : sdaLevel();
    const bool sclNow = sdaFirst ? second : first;
    const bool sdaNow = sdaFirst ? first : second;
    return (sclNow ? PIN_SCL : 0) | (sdaNow ? PIN_SDA : 0) | (!simTargetLow ? PIN_DRIVE : 0);
  }
  uint32_t resyncs = 0;  // times the tick timer was set on leaving
  void resyncTick() override { ++resyncs; }
  bool serialPending() override {  // asked once per upkeep step
    ++upkeeps;
    const uint32_t falls = sclFalls;
    t += upkeepCost;
    advance();
    if (sclFalls != falls) ++fallsDuringUpkeep;
    return false;
  }
  void feedWatchdog() override {
    if (feeds && t - lastFeed > longestUnfed) longestUnfed = t - lastFeed;
    ++feeds;
    lastFeed = t;
  }
  bool finished() const { return next == wave.size(); }
};

// Runs the poller over a waveform until it has all been played.
struct PollRun {
  Rig rig;
  TestPoll host;
  PollState state;
  Event event;  // how the session ended, if it did
  // Armed for DATA OUT, or for DATA IN with `upload` as the image to serve.
  PollRun(const Waveform &wave, const Bytes *upload = nullptr) {
    rig.engine.ticksPerUs = 100;
    if (upload) {
      rig.upload(*upload);
      char arm[32];
      snprintf(arm, sizeof(arm), "ARM IN %08X", unsigned(crc32(upload->data(), upload->size())));
      rig.send(arm);
    } else {
      rig.send("ARM OUT");
    }
    host.wave = wave.sorted();
    host.session = &rig.session;
    pollHost = &host;
  }
  ~PollRun() { pollHost = nullptr; }
  uint32_t returnsDuringWave = 0;  // returns for serial before the waveform ended
  uint32_t calls = 0;              // times the poller was entered
  void run() {
    uint32_t nowMs = rig.nowMs;
    for (int i = 0; i < 200 && rig.loop.armed() && !host.finished(); ++i) {
      const PollResult result = pollArmed(rig.engine, rig.session, state, nowMs);
      ++calls;
      nowMs = result.nowMs;
      if (result.event.kind != EventKind::None) {
        event = result.event;
        break;
      }
      if (!host.finished()) ++returnsDuringWave;
    }
  }
};

static Waveform packetsWave(const Bytes &image, uint16_t packets, uint32_t glitch, uint32_t highTime) {
  Waveform wave;
  wave.glitch = glitch;
  wave.highTime = highTime;
  for (uint16_t p = 0; p < packets; ++p) wave.packet(p * PACKET_LEN, &image[p * PACKET_LEN]);
  wave.idle(300000);  // 3 ms of idle at the end
  return wave;
}

// Packets for whole blocks, then the radio's verify reads of those blocks.
static Waveform readsWave(const Bytes &image, uint8_t firstBlock, uint8_t blocks,
                          uint32_t *readsStart = nullptr) {
  Waveform wave;
  wave.lowTime = 1250;  // the shortest SCL LOW measured on the radio
  const uint16_t firstPacket = firstBlock * (BLOCK_LEN / PACKET_LEN);
  for (uint16_t p = 0; p < blocks * (BLOCK_LEN / PACKET_LEN); ++p) {
    wave.packet((firstPacket + p) * PACKET_LEN, &image[(firstPacket + p) * PACKET_LEN]);
  }
  if (readsStart) *readsStart = wave.t;
  for (uint8_t b = 0; b < blocks; ++b) wave.readBlock((firstBlock + b) * BLOCK_LEN, BLOCK_LEN);
  wave.idle(300000);
  return wave;
}

static Bytes radioBytes(const std::vector<bool> &bits) {
  Bytes out(bits.size() / 8, 0);
  for (size_t i = 0; i < out.size() * 8; ++i) out[i / 8] = uint8_t((out[i / 8] << 1) | (bits[i] ? 1 : 0));
  return out;
}

static bool startsWith(const std::string &text, const std::string &prefix) {
  return text.rfind(prefix, 0) == 0;
}

static bool anyStartsWith(const std::vector<std::string> &lines, const std::string &prefix) {
  return std::any_of(lines.begin(), lines.end(), [&](const std::string &l) { return startsWith(l, prefix); });
}

static Bytes readFile(const char *path) {
  std::ifstream file(path, std::ios::binary);
  return Bytes(std::istreambuf_iterator<char>(file), {});
}

static std::vector<uint16_t> sequentialPackets() {
  std::vector<uint16_t> order(PACKET_COUNT);
  for (uint16_t i = 0; i < PACKET_COUNT; ++i) order[i] = i;
  return order;
}

static void test(const char *name, const std::function<void()> &body) {
  const int before = failures;
  body();
  std::printf("%s %s\n", failures == before ? "ok  " : "FAIL", name);
}

// A made-up image for the simulation: pseudo-random bytes, so every block
// has plenty of 0-to-1 and 1-to-0 bit changes, with a name-like text area.
static Bytes madeUpImage() {
  Bytes image(IMAGE_LEN);
  std::mt19937 random(909);
  for (uint8_t &byte : image) byte = uint8_t(random());
  static const char text[] = "BBC 1     RADIO 2   STATION 3 ";
  for (size_t at = 0x0300; at < 0x0700; ++at) image[at] = uint8_t(text[at % 30]);
  return image;
}

int main(int argc, char **argv) {
  // Without arguments the simulation runs on a made-up image. With two files
  // it runs on real exports: an image, and the same image with the two bytes
  // at 0x0E40 changed.
  if (argc != 1 && argc != 3) {
    std::printf("usage: host_sim [original.bin edited.bin]\n");
    return 2;
  }
  const Bytes original = argc == 3 ? readFile(argv[1]) : madeUpImage();
  Bytes edited = argc == 3 ? readFile(argv[2]) : original;
  if (argc == 1) {
    edited[0x0E40] ^= 0x2A;
    edited[0x0E41] ^= 0x01;
  }
  if (original.size() != IMAGE_LEN || edited.size() != IMAGE_LEN) {
    std::printf("images must be %u bytes\n", unsigned(IMAGE_LEN));
    return 2;
  }
  std::printf("images: %s\n", argc == 3 ? "from files" : "made up");

  test("crc32 and hex helpers", [] {
    const char *digits = "123456789";
    CHECK(crc32(reinterpret_cast<const uint8_t *>(digits), 9) == 0xCBF43926u);
    uint8_t data[3] = {0x00, 0xAB, 0xFF};
    char hex[7];
    hexEncode(data, 3, hex);
    CHECK(std::string(hex) == "00ABFF");
    uint8_t back[3];
    CHECK(hexDecode("00abFF", 3, back) && back[1] == 0xAB && back[2] == 0xFF);
    CHECK(!hexDecode("00AB", 3, back));
    CHECK(!hexDecode("00ABFG", 3, back));
  });

  test("HELLO, unknown commands and READY", [] {
    Rig rig;
    CHECK(rig.send("HELLO") == "OK HELLO bridge proto=1 fw=1.0.0 len=12032 boot=12345678");
    CHECK(rig.send("hello") == "OK HELLO bridge proto=1 fw=1.0.0 len=12032 boot=12345678");
    CHECK(rig.send("FROB 1") == "ERR FROB unknown");
    Lines greeting;
    rig.commands.greet(greeting);
    CHECK(greeting.last() == "READY bridge proto=1 fw=1.0.0 boot=12345678");
    CHECK(startsWith(rig.send("STATUS"), "OK STATUS state=idle staged=0"));
  });

  test("disarmed bridge never ACKs", [] {
    Rig rig;
    rig.bus.start();
    CHECK(!rig.bus.writeByte(WRITE_ADDRESS_BYTE));
    rig.bus.stop();
    CHECK(!simTargetLow);
  });

  test("DATA OUT rebuilds the image and passes verification", [&] {
    Rig rig;
    CHECK(startsWith(rig.send("ARM OUT"), "OK ARM dir=out window_ms=120000"));
    Bytes readback;
    CHECK(rig.radio.dataOut(original, sequentialPackets(), readback));
    CHECK(readback == original);
    const auto events = rig.run(10);
    char done[96];
    snprintf(done, sizeof(done), "EV DONE dir=out packets=752 verify=47 mism=0 crc=%08X",
             unsigned(crc32(original.data(), original.size())));
    CHECK(anyStartsWith(events, done));
    CHECK(!rig.loop.armed());
    CHECK(rig.download() == original);
    CHECK(rig.engine.bitMismatches == 0);
  });

  test("DATA OUT completion event is reported", [&] {
    Rig rig;
    rig.send("ARM OUT");
    Bytes readback;
    rig.radio.dataOut(edited, sequentialPackets(), readback);
    const auto events = rig.run(5);
    char done[96];
    snprintf(done, sizeof(done), "EV DONE dir=out packets=752 verify=47 mism=0 crc=%08X",
             unsigned(crc32(edited.data(), edited.size())));
    CHECK(events.size() == 1 && events[0] == done);
  });

  test("DATA OUT tolerates shuffled and repeated identical packets", [&] {
    Rig rig;
    rig.send("ARM OUT");
    auto order = sequentialPackets();
    std::mt19937 rng(7);
    std::shuffle(order.begin(), order.end(), rng);
    order.push_back(5);
    order.push_back(700);
    Bytes readback;
    CHECK(rig.radio.dataOut(original, order, readback));
    CHECK(readback == original);
    CHECK(anyStartsWith(rig.run(5), "EV DONE dir=out packets=752 verify=47"));
  });

  test("DATA OUT with a missing packet fails as incomplete", [&] {
    Rig rig;
    rig.send("ARM OUT");
    auto order = sequentialPackets();
    order.erase(order.begin() + 100);  // block 6
    Bytes readback;
    CHECK(!rig.radio.dataOut(original, order, readback));  // block 6 verify read is refused
    rig.touch();
    const auto events = rig.run(INCOMPLETE_QUIET_MS + 10);
    CHECK(anyStartsWith(events, "EV FAIL dir=out reason=incomplete packets=751 verify=46"));
    CHECK(!rig.loop.armed());
  });

  test("DATA OUT with conflicting duplicates fails as conflict", [&] {
    Rig rig;
    rig.send("ARM OUT");
    Bytes other = original;
    other[0x0E40] ^= 0xFF;
    for (uint16_t p = 0; p < PACKET_COUNT; ++p) {
      rig.radio.writePacket(p * PACKET_LEN, &original[p * PACKET_LEN], PACKET_LEN);
    }
    rig.radio.writePacket(0x0E40, &other[0x0E40], PACKET_LEN);
    Bytes data;
    CHECK(!rig.radio.readBlock(TARGET_ADDRESS, 0x0E00, BLOCK_LEN, data, true));
    CHECK(rig.radio.readBlock(TARGET_ADDRESS, 0x0D00, BLOCK_LEN, data, true));
    rig.touch();
    CHECK(anyStartsWith(rig.run(INCOMPLETE_QUIET_MS + 10), "EV FAIL dir=out reason=conflict"));
  });

  test("DATA IN serves the uploaded image twice", [&] {
    for (bool repeated : {true, false}) {
      Rig rig;
      CHECK(rig.upload(edited));
      char arm[32];
      snprintf(arm, sizeof(arm), "ARM IN %08X", unsigned(crc32(edited.data(), edited.size())));
      CHECK(startsWith(rig.send(arm), "OK ARM dir=in window_ms=120000"));
      Bytes pass1, pass2;
      CHECK(rig.radio.dataIn(pass1, pass2, repeated));
      CHECK(pass1 == edited);
      CHECK(pass2 == edited);
      rig.touch();
      const auto events = rig.run(IN_DONE_QUIET_MS + 10);
      CHECK(anyStartsWith(events, "EV DONE dir=in passes=2 bytes=24064 mism=0 past_end=0"));
      CHECK(!rig.loop.armed());
    }
  });

  test("DATA IN refuses data writes and reports the wrong direction", [&] {
    Rig rig;
    CHECK(rig.upload(original));
    char arm[32];
    snprintf(arm, sizeof(arm), "ARM IN %08X", unsigned(crc32(original.data(), original.size())));
    rig.send(arm);
    CHECK(!rig.radio.writePacket(0x0000, &original[0], PACKET_LEN));
    const auto events = rig.run(2);
    CHECK(anyStartsWith(events, "EV FAIL dir=in reason=wrong_direction"));
    // Served image was not modified.
    CHECK(Bytes(rig.engine.image, rig.engine.image + IMAGE_LEN) == original);
  });

  test("armed for OUT but the radio imports: wrong direction", [&] {
    Rig rig;
    rig.send("ARM OUT");
    Bytes data;
    CHECK(!rig.radio.readBlock(TARGET_ADDRESS, 0, BLOCK_LEN, data, true));
    CHECK(anyStartsWith(rig.run(2), "EV FAIL dir=out reason=wrong_direction"));
  });

  test("reads past the end of the image are not served", [&] {
    Rig rig;
    rig.upload(original);
    char arm[32];
    snprintf(arm, sizeof(arm), "ARM IN %08X", unsigned(crc32(original.data(), original.size())));
    rig.send(arm);
    Bytes data;
    CHECK(rig.radio.readBlock(TARGET_ADDRESS, 0x2E00, BLOCK_LEN + 4, data, true));
    CHECK(Bytes(data.begin(), data.begin() + BLOCK_LEN) == Bytes(original.end() - BLOCK_LEN, original.end()));
    CHECK(data[BLOCK_LEN] == 0xFF);  // released SDA reads as 1s
    CHECK(rig.engine.pastEndRequests == 1);
  });

  test("other devices on the bus are never ACKed", [&] {
    Rig rig;
    rig.send("ARM OUT");
    for (uint8_t address : {0x50, 0x11, 0x5B}) {
      rig.bus.start();
      CHECK(!rig.bus.writeByte(uint8_t(address << 1)));
      rig.bus.stop();
      rig.bus.start();
      CHECK(!rig.bus.writeByte(uint8_t((address << 1) | 1)));
      rig.bus.stop();
    }
    CHECK(rig.engine.activity == 0);
    CHECK(rig.loop.armed());
  });

  test("arm window expires without radio traffic", [&] {
    Rig rig;
    rig.send("ARM OUT");
    CHECK(rig.run(ARM_WINDOW_MS - 10).empty());
    CHECK(rig.loop.armed());
    CHECK(anyStartsWith(rig.run(20), "EV FAIL dir=out reason=no_radio"));
    CHECK(!rig.loop.armed());
  });

  test("SDA is never held low longer than the timeout", [&] {
    Rig rig;
    rig.send("ARM OUT");
    rig.bus.start();
    for (int bit = 7; bit >= 0; --bit) rig.bus.writeBit((WRITE_ADDRESS_BYTE >> bit) & 1);
    CHECK(simTargetLow);  // ACK slot driven, radio stopped clocking
    simMicros += SDA_HOLD_TIMEOUT_US + 1;
    Lines out;
    rig.loop.poll(rig.nowMs, out);
    CHECK(startsWith(out.last(), "EV FAIL dir=out reason=stalled"));
    CHECK(!simTargetLow);
  });

  test("a fresh ACK pulse is never mistaken for a stall", [&] {
    Rig rig;
    rig.send("ARM OUT");
    rig.bus.start();
    for (int bit = 7; bit >= 0; --bit) rig.bus.writeBit((WRITE_ADDRESS_BYTE >> bit) & 1);
    CHECK(simTargetLow);
    // Hardware run 2026-11-12: the loop read the clock, then the interrupt
    // started a new pulse, and the wrapped difference looked like a stall.
    Lines out;
    for (int i = 0; i < 1000; ++i) rig.loop.poll(rig.nowMs, out);
    CHECK(out.lines.empty());
    CHECK(rig.loop.armed());
    simMicros += SDA_HOLD_TIMEOUT_US - 1;
    rig.loop.poll(rig.nowMs, out);
    CHECK(out.lines.empty());
    simMicros += 1;
    rig.loop.poll(rig.nowMs, out);
    CHECK(startsWith(out.last(), "EV FAIL dir=out reason=stalled"));
  });

  test("arming needs an idle bus", [&] {
    Rig rig;
    rig.port.pins = {false, true, true};
    CHECK(rig.send("ARM OUT") == "ERR ARM bus_busy");
    CHECK(!rig.loop.armed());
    // SCL and SDA idle but the drive pin reads LOW: GPIO21 is not wired to SDA.
    rig.port.pins = {true, true, false};
    CHECK(rig.send("ARM OUT") == "ERR ARM drive_low");
    CHECK(!rig.loop.armed());
  });

  test("upload validation", [&] {
    Rig rig;
    CHECK(rig.send("ARM IN 00000000") == "ERR ARM not_staged");
    CHECK(rig.send("COMMIT 00000000") == "ERR COMMIT missing");
    CHECK(rig.send("PUT 47 00 00000000") == "ERR PUT args");
    CHECK(rig.send("PUT 1 0011 00000000") == "ERR PUT hex");
    std::string zeros(512, '0');
    CHECK(rig.send("PUT 0 " + zeros + " 00000000") == "ERR PUT crc");
    CHECK(rig.upload(original));
    CHECK(rig.send("COMMIT 00000000") == "ERR COMMIT crc");
    CHECK(rig.upload(original));
    CHECK(rig.send("ARM IN 00000000") == "ERR ARM crc");
    char status[96];
    snprintf(status, sizeof(status), "OK STATUS state=idle staged=1 staged_crc=%08X",
             unsigned(crc32(original.data(), original.size())));
    CHECK(startsWith(rig.send("STATUS"), status));
  });

  test("commands while armed are refused, ABORT disarms", [&] {
    Rig rig;
    rig.upload(original);
    rig.send("ARM OUT");
    CHECK(rig.send("ARM OUT") == "ERR ARM armed");
    CHECK(rig.send("GET 0") == "ERR GET armed");
    CHECK(rig.send("PUT 0 00 00000000") == "ERR PUT armed");
    CHECK(rig.send("COMMIT 00000000") == "ERR COMMIT armed");
    CHECK(startsWith(rig.send("STATUS"), "OK STATUS state=out"));
    const size_t before = rig.out.lines.size();
    CHECK(rig.send("ABORT") == "OK ABORT");
    CHECK(rig.out.lines.size() == before + 2);
    CHECK(startsWith(rig.out.last(), "EV FAIL dir=out reason=aborted"));
    CHECK(!rig.loop.armed());
    CHECK(rig.send("ABORT") == "OK ABORT");
  });

  test("progress events while transferring, when enabled", [&] {
    Rig rig;
    rig.session.reportProgress(true);
    rig.send("ARM OUT");
    std::vector<std::string> events;
    for (uint16_t p = 0; p < PACKET_COUNT; ++p) {
      rig.radio.writePacket(p * PACKET_LEN, &original[p * PACKET_LEN], PACKET_LEN);
      if (p % 100 == 0) {
        auto more = rig.run(PROGRESS_INTERVAL_MS);
        events.insert(events.end(), more.begin(), more.end());
      }
    }
    CHECK(anyStartsWith(events, "EV PROGRESS dir=out packets="));
    CHECK(rig.loop.armed());
    CHECK(rig.engine.starts == PACKET_COUNT && rig.engine.stops == PACKET_COUNT);
    // 752 writes of 19 bytes, 9 clocks each, two SCL edges per clock, plus
    // the SCL fall after START and the SCL rise before STOP.
    CHECK(rig.engine.sclEdges == PACKET_COUNT * (19 * 9 * 2 + 2));
    CHECK(rig.engine.sdaEdges > 0 && rig.engine.sdaEdges < rig.engine.sclEdges);
    CHECK(rig.engine.addressCounts[TARGET_ADDRESS][0] == PACKET_COUNT);
    Lines diag;
    rig.commands.armed = false;  // DIAG is answered while armed too
    char cmd[] = "DIAG";
    rig.commands.handleLine(cmd, diag);
    CHECK(std::any_of(diag.lines.begin(), diag.lines.end(),
                      [](const std::string &l) { return l == "# address 0x51: writes 752, reads 0"; }));

  });

  test("the serial line stays silent while armed by default", [&] {
    Rig rig;
    rig.send("ARM OUT");
    std::vector<std::string> events;
    for (uint16_t p = 0; p < PACKET_COUNT; ++p) {
      rig.radio.writePacket(p * PACKET_LEN, &original[p * PACKET_LEN], PACKET_LEN);
      if (p % 50 == 0) {
        auto more = rig.run(PROGRESS_INTERVAL_MS);
        events.insert(events.end(), more.begin(), more.end());
      }
    }
    CHECK(events.empty());
    CHECK(rig.loop.armed());
  });

  test("TRACE lists the last interrupt calls, ending at a failure", [&] {
    Rig rig;
    rig.send("ARM OUT");
    CHECK(rig.send("TRACE") == "ERR TRACE armed");
    rig.radio.writePacket(0, &original[0], PACKET_LEN);
    rig.bus.start();
    for (int bit = 7; bit >= 0; --bit) rig.bus.writeBit((WRITE_ADDRESS_BYTE >> bit) & 1);
    simMicros += SDA_HOLD_TIMEOUT_US + 1;  // the radio stops clocking during our ACK
    Lines out;
    rig.loop.poll(rig.nowMs, out);
    CHECK(startsWith(out.last(), "EV FAIL dir=out reason=stalled"));
    const uint32_t recorded = rig.engine.traceHead;
    rig.bus.stop();  // after disarming nothing more is recorded
    CHECK(rig.engine.traceHead == recorded);
    const size_t before = rig.out.lines.size();
    CHECK(startsWith(rig.send("TRACE"), "# trace: last "));
    CHECK(rig.out.last() == "OK TRACE");
    const size_t lines = rig.out.lines.size() - before;
    CHECK(lines == std::min<uint32_t>(recorded, BusEngine::TRACE_LEN) + 2);
    // The last call is the fall after the address byte, which drives the ACK
    // (the last address bit is 0, so the contact was already LOW).
    CHECK(rig.out.lines[rig.out.lines.size() - 2] == "# +36 S0 D0 G1 L1 AckSetup>AckRise b8");
  });

  test("polling decodes the radio's timing", [&] {
    // SCL HIGH for 3 and 15 µs, sampled every 0.15, 0.36 and 0.6 µs.
    for (uint32_t highTime : {300u, 1500u})
    for (uint32_t period : {15u, 36u, 60u}) {
      PollRun run(packetsWave(original, 20, 0, highTime));
      run.host.samplePeriod = period;
      run.run();
      BusEngine &engine = run.rig.engine;
      CHECK(run.host.finished());
      CHECK(engine.packetCount == 20);
      CHECK(engine.oddWrites == 0);
      CHECK(engine.starts == 20 && engine.stops == 20);
      CHECK(engine.drivenAckSlots == 20 * 19u);
      CHECK(std::equal(original.begin(), original.begin() + 20 * PACKET_LEN, engine.image));
      // The measured SCL HIGH width is within two samples of the real one.
      CHECK(engine.poll.minSclHighTicks + 2 * period >= highTime);
      CHECK(engine.poll.minSclHighTicks <= highTime + 2 * period);
    }
  });

  test("crosstalk glitches are filtered out, not decoded as clock pulses", [&] {
    // Hardware run 2026-11-12: with unfiltered polling every address decoded
    // one bit late (0x51 write seen as 0x28 read), because SCL dipped when
    // SDA fell at START. Here SCL dips for 0.1–2.5 µs at every SDA fall while
    // SCL is HIGH, and SDA dips at every SCL fall while SDA is HIGH.
    for (uint32_t glitch : {10u, 40u, 80u, 150u, 250u}) {
      PollRun run(packetsWave(original, 20, glitch, 1500));
      run.run();
      BusEngine &engine = run.rig.engine;
      CHECK(engine.packetCount == 20);
      CHECK(engine.oddWrites == 0);
      CHECK(engine.starts == 20 && engine.stops == 20);
      CHECK(engine.addressCounts[TARGET_ADDRESS][0] == 20);
      CHECK(engine.addressCounts[0x28][1] == 0);
      CHECK(std::equal(original.begin(), original.begin() + 20 * PACKET_LEN, engine.image));
      uint32_t sclDips = 0, sdaDips = 0;
      for (uint8_t bin = 0; bin < WIDTH_BINS; ++bin) {
        sclDips += engine.poll.sclDips[bin];
        sdaDips += engine.poll.sdaDips[bin];
      }
      // Dips of two samples or more are always seen: one per START. Up to
      // 1.5 µs they are rejected. A longer one passes as a short SCL LOW,
      // which is over before the START's SDA fall is accepted: harmless.
      if (glitch == 80) CHECK(sclDips == 20);
      if (glitch >= 80) CHECK(sclDips + engine.poll.shortLows == 20);
      if (glitch == 250) CHECK(engine.poll.shortLows == 20);
      // SDA dips shorter than the sender's own change 1 µs after the fall
      // show up separately; longer ones merge with that change.
      if (glitch == 40 || glitch == 80) CHECK(sdaDips > 100);
    }
  });

  test("SDA changing right at an SCL fall is never taken for a START or STOP", [&] {
    // Hardware run 2026-11-12: thousands of false STARTs, mostly inside the
    // radio's own EEPROM reads, where SDA changes within a fraction of a µs
    // of the clock fall. Here every data change follows its SCL fall by
    // 0.01–1 µs, for packets to the bridge and for traffic to another device.
    for (uint32_t delay : {1u, 5u, 12u, 20u, 36u, 100u}) {
      Waveform wave;
      wave.dataDelay = delay;
      for (uint16_t p = 0; p < 12; ++p) {
        wave.otherWrite(0x50, &original[0x0300 + p * 40], 40);  // bytes with many 1→0 changes
        wave.packet(p * PACKET_LEN, &original[p * PACKET_LEN]);
      }
      wave.idle(300000);
      PollRun run(wave);
      run.run();
      BusEngine &engine = run.rig.engine;
      CHECK(engine.starts == 24);
      CHECK(engine.stops == 24);
      CHECK(engine.addressCounts[0x50][0] == 12);
      CHECK(engine.addressCounts[TARGET_ADDRESS][0] == 12);
      CHECK(engine.packetCount == 12);
      CHECK(engine.oddWrites == 0);
      CHECK(std::equal(original.begin(), original.begin() + 12 * PACKET_LEN, engine.image));
    }
  });

  test("the interrupt watchdog is fed while interrupts stay masked", [&] {
    // A whole transfer's worth of bus time with no break: 40 packets.
    PollRun run(packetsWave(original, 40, 0, 1500));
    run.run();
    CHECK(run.rig.engine.packetCount == 40);
    CHECK(run.host.feeds > 5);
    CHECK(run.host.longestUnfed <= 6000000);  // 60 ms; the watchdog allows 300 ms
    CHECK(run.returnsDuringWave == 0);  // nothing interrupts a running transfer
  });

  test("the poller leaves for serial input only when the bus is idle", [&] {
    struct Chatty : TestPoll {
      bool serialPending() override { return true; }
    };
    Rig rig;
    rig.engine.ticksPerUs = 100;
    rig.send("ARM OUT");
    Chatty host;
    Waveform wave;
    for (uint16_t p = 0; p < 3; ++p) {
      wave.packet(p * PACKET_LEN, &original[p * PACKET_LEN]);
      wave.t += 900000;  // 9 ms pauses: long enough to leave for serial several times
    }
    wave.idle(300000);
    host.wave = wave.sorted();
    host.session = &rig.session;
    pollHost = &host;
    PollState state;
    uint32_t nowMs = rig.nowMs;
    int returns = 0;
    while (rig.loop.armed() && !host.finished() && returns < 1000) {
      const PollResult result = pollArmed(rig.engine, rig.session, state, nowMs);
      nowMs = result.nowMs;
      ++returns;
      // Returned with the bus idle: both lines HIGH and the bridge not driving.
      CHECK(host.scl && host.sdaReleased && !simTargetLow);
      host.t += 30000;  // 0.3 ms spent in loop() before watching resumes
      host.advance();
    }
    pollHost = nullptr;
    CHECK(rig.engine.packetCount == 3);
    CHECK(returns >= 9);
    CHECK(rig.engine.poll.serialReturns >= 9);
  });

  test("the fallback serial check stops once the radio addresses the bridge", [&] {
    // Two seconds of idle bus, a packet, then 5 s of idle: the loop returns
    // for a serial check during the first wait and never again after.
    Waveform wave;
    wave.idle(250000000);  // 2.5 s
    wave.packet(0, &original[0]);
    wave.idle(200000000);  // 2 s more; the session itself ends after 3 s
    PollRun run(wave);
    run.run();
    CHECK(run.rig.engine.packetCount == 1);
    CHECK(run.rig.engine.poll.serialReturns == 1);
  });

  test("watching that resumes mid-transaction waits for the next START", [&] {
    // The loop was away while a transaction to another device began. The
    // rest of it must be skipped, not decoded from the middle.
    Rig rig;
    rig.engine.ticksPerUs = 100;
    rig.send("ARM OUT");
    TestPoll host;
    Waveform wave;
    wave.otherWrite(0x11, &original[0x0300], 6);
    wave.packet(0, &original[0]);
    wave.idle(300000);
    host.wave = wave.sorted();
    host.session = &rig.session;
    host.t = 100000 + 900 + 5 * 2800;  // a few bits into the first transaction
    host.advance();
    pollHost = &host;
    PollState state;
    uint32_t nowMs = rig.nowMs;
    for (int i = 0; i < 50 && rig.loop.armed() && !host.finished(); ++i) {
      nowMs = pollArmed(rig.engine, rig.session, state, nowMs).nowMs;
    }
    pollHost = nullptr;
    CHECK(rig.engine.starts == 1);  // only the packet's
    CHECK(rig.engine.addressCounts[TARGET_ADDRESS][0] == 1);
    CHECK(rig.engine.packetCount == 1);
    CHECK(std::equal(original.begin(), original.begin() + PACKET_LEN, rig.engine.image));
  });

  test("DIAG prints a report", [&] {
    Rig rig;
    const size_t before = rig.out.lines.size();
    CHECK(rig.send("DIAG") == "# state=idle SCL=HIGH SDA=HIGH drive=HIGH (released)");
    CHECK(rig.out.last() == "OK DIAG");
    CHECK(rig.out.lines.size() - before == 19);
  });

  // Hardware runs of 2026-11-12, 19:58 and 19:59: all 752 packets arrived, but
  // the radio's verification stopped after 19 and 5 blocks with one wrong bit.
  // The image blocks used here hold the page names (many 0-to-1 bit changes).
  const uint8_t NAME_BLOCK = 3, NAME_BLOCKS = 4;
  const Bytes names(edited.begin() + NAME_BLOCK * BLOCK_LEN,
                    edited.begin() + (NAME_BLOCK + NAME_BLOCKS) * BLOCK_LEN);

  test("verify reads reach the radio right even when upkeep is slow", [&] {
    // SDA needs 6 µs to rise once let go, SCL is LOW for 12.5 µs, and an
    // upkeep step takes 5 µs: an upkeep just before an SCL fall would make a
    // 1-bit come up after the clock. Upkeep must keep away from the falls.
    PollRun run(readsWave(edited, NAME_BLOCK, NAME_BLOCKS));
    run.host.riseTime = 600;
    run.host.upkeepCost = 500;
    run.run();
    BusEngine &engine = run.rig.engine;
    CHECK(run.host.finished());
    CHECK(run.host.upkeeps > 200);
    CHECK(run.host.fallsDuringUpkeep == 0);
    CHECK(radioBytes(run.host.sampled) == names);
    CHECK(engine.verifyCount == NAME_BLOCKS);
    CHECK(engine.bitMismatches == 0);
    CHECK(engine.lateRises == 0 && engine.poll.sdaLate == 0 && engine.poll.sdaWithClock == 0);
    // Every answer is given at first sight of the fall, within 1 µs of the
    // last look before it, and agrees with the decoder. With a 6 µs rise the
    // line is up 5 µs or more before the clock.
    uint32_t slow = 0, answers = 0;
    for (uint8_t bin = 0; bin < WIDTH_BINS; ++bin) {
      answers += engine.poll.answerTime[bin];
      if (bin >= 1) slow += engine.poll.answerTime[bin];
    }
    CHECK(answers > 9000 && slow == 0);
    CHECK(engine.poll.earlyAnswers == answers && engine.poll.plainAnswers == 0);
    CHECK(engine.poll.earlyWrong == 0 && engine.poll.earlyUndone == 0);
    CHECK(engine.poll.minSetupTicks >= 500 && engine.poll.minSetupTicks < 700);
    CHECK(engine.poll.sdaRise[6] + engine.poll.sdaRise[5] > 500);  // the 6 µs rise, measured
    CHECK(engine.poll.minSclLowTicks >= 1200 && engine.poll.minSclLowTicks <= 1300);
    CHECK(engine.poll.shortLows == 0);
  });

  test("a 1-bit still settling when the clock rises is taken with the clock", [&] {
    // The line comes up 10.5 µs after the bridge lets go: less than the 3 µs
    // SDA filter before the clock rise. Reported 3 µs late, it would look
    // like SDA changing while SCL is HIGH: a wrong bit, then a STOP.
    PollRun run(readsWave(edited, NAME_BLOCK, NAME_BLOCKS));
    run.host.riseTime = 1050;
    run.run();
    BusEngine &engine = run.rig.engine;
    CHECK(run.host.finished());
    CHECK(radioBytes(run.host.sampled) == names);
    CHECK(engine.poll.sdaWithClock > 500);
    CHECK(engine.verifyCount == NAME_BLOCKS);
    CHECK(engine.bitMismatches == 0);
    CHECK(engine.lateRises == 0);
    CHECK(engine.bytesClocked == NAME_BLOCKS * BLOCK_LEN);
    CHECK(engine.stops == NAME_BLOCKS * (BLOCK_LEN / PACKET_LEN) + NAME_BLOCKS);
  });

  test("a 1-bit that comes up after the clock rise does not end the read", [&] {
    // 13 µs to rise: later than the clock. The bit is wrong and is counted,
    // but the bridge's own line coming up is not a STOP: it keeps serving.
    // (The packets arrive first, with a line that rises at once.)
    uint32_t readsStart = 0;
    PollRun run(readsWave(edited, NAME_BLOCK, NAME_BLOCKS, &readsStart));
    run.host.riseTime = 1300;
    run.host.slowFrom = readsStart;
    run.run();
    CHECK(run.rig.engine.packetCount == NAME_BLOCKS * (BLOCK_LEN / PACKET_LEN));
    BusEngine &engine = run.rig.engine;
    CHECK(run.host.finished());
    CHECK(engine.bytesClocked == NAME_BLOCKS * BLOCK_LEN);
    CHECK(engine.lateRises > 500);
    CHECK(engine.bitMismatches == engine.lateRises);
    CHECK(engine.poll.sdaLate >= engine.lateRises);
    CHECK(engine.verifyCount == 0);
    // The first one is described, and the trace stops shortly after it.
    CHECK(engine.mismatchOffset >= NAME_BLOCK * BLOCK_LEN);
    CHECK(engine.mismatchOffset < NAME_BLOCK * BLOCK_LEN + 16);
    CHECK(engine.mismatchSentHigh);
    CHECK(engine.traceHead == engine.traceStop);
    run.rig.send("ABORT");
    run.rig.send("DIAG");
    CHECK(anyStartsWith(run.rig.out.lines, "# first bit mismatch: image offset 0x03"));
    CHECK(std::any_of(run.rig.out.lines.begin(), run.rig.out.lines.end(), [](const std::string &l) {
      return l.find("bridge sent HIGH (line read LOW)") != std::string::npos;
    }));
  });

  // Writing to the radio (DATA IN) uses the same answer path for every byte.
  const auto dataInWave = [&](bool repeated) {
    Waveform wave;
    wave.lowTime = 1250;
    for (int pass = 0; pass < 2; ++pass) {
      for (uint8_t b = 0; b < BLOCK_COUNT; ++b) wave.readBlock(b * BLOCK_LEN, BLOCK_LEN, repeated);
    }
    wave.idle((IN_DONE_QUIET_MS + 200) * 100000);
    return wave;
  };
  Bytes twice = edited;
  twice.insert(twice.end(), edited.begin(), edited.end());

  test("DATA IN through the poller: the radio reads the image twice, bit for bit", [&] {
    for (bool repeated : {true, false}) {
      PollRun run(dataInWave(repeated), &edited);
      run.host.riseTime = 100;    // as measured: SDA is up within 1 µs
      run.host.upkeepCost = 180;  // as measured
      run.run();
      BusEngine &engine = run.rig.engine;
      CHECK(radioBytes(run.host.sampled) == twice);
      CHECK(run.event.kind == EventKind::Done);
      CHECK(run.event.passes == 2 && run.event.mismatches == 0 && run.event.pastEnd == 0);
      CHECK(engine.bytesClocked == 2 * IMAGE_LEN);
      CHECK(engine.bitMismatches == 0 && engine.lateRises == 0);
      CHECK(engine.poll.sdaLate == 0 && engine.poll.sdaWithClock == 0);
      CHECK(engine.poll.earlyWrong == 0 && engine.poll.earlyUndone == 0 && engine.poll.plainAnswers == 0);
      CHECK(engine.poll.earlyAnswers > 2 * IMAGE_LEN * 8);
      CHECK(engine.poll.minSetupTicks >= 1000);  // up 10 µs or more before the clock
      CHECK(!run.rig.loop.armed());
      // Hardware run of 2026-11-13: the board reset 300 ms after each long
      // DATA IN, because the tick timer was left behind. Every time the
      // poller leaves, it must set the tick timer first.
      CHECK(run.calls >= 1 && run.host.resyncs == run.calls);
    }
  });

  test("time taken from the core does not make a sent bit late", [&] {
    // Hardware runs of 2026-11-12 (20:22, 20:23): about 75 times a second the
    // core loses up to 4.2 µs, and a 1-bit was once up only 0.9 µs before the
    // clock. Here it loses 4.2 µs 2000 times a second, once or twice in a
    // row. With answers at first sight the line is still up well before the
    // clock: the first look after a theft sees the fall and answers at once.
    const uint32_t steal = 420;
    for (bool twice_ : {false, true}) {
      PollRun run(dataInWave(true), &edited);
      run.host.riseTime = 100;
      run.host.upkeepCost = 180;
      run.host.stealEvery = 50000 - 137;  // drifts across the bit period
      run.host.stealTime = steal;
      run.host.stealTwice = twice_;
      run.run();
      BusEngine &engine = run.rig.engine;
      CHECK(run.host.steals > 10000);
      CHECK(radioBytes(run.host.sampled) == twice);
      CHECK(run.event.kind == EventKind::Done && run.event.passes == 2 && run.event.mismatches == 0);
      CHECK(engine.bitMismatches == 0 && engine.lateRises == 0 && engine.poll.sdaLate == 0);
      CHECK(engine.poll.earlyWrong == 0);
      // Worst case: the theft hides the fall, the answer follows at once, the
      // line needs 1 µs. (A second theft only delays the bridge seeing it.)
      if (!twice_) CHECK(engine.poll.minSetupTicks >= 1250 - steal - 100 - 2 * 36 - 50);
      CHECK(engine.poll.maxAnswerTicks <= steal + 2 * 36);
    }
  });

  test("an answer given to a clock glitch is taken back", [&] {
    // An SCL dip of 0.8 µs late in the HIGH phase of every bit the bridge
    // sends: it looks like the start of a fall. The bridge sets the next bit,
    // sees the clock come back, and restores the current one. (No such dips
    // have been seen on the radio since the pull-ups.)
    uint32_t readsStart = 0;
    Waveform wave = readsWave(edited, NAME_BLOCK, 1, &readsStart);
    Waveform dipped = wave;
    dipped.edges.clear();
    dipped.t = 100000;
    dipped.scl = dipped.sda = true;
    for (uint16_t p = 0; p < BLOCK_LEN / PACKET_LEN; ++p) {
      const uint16_t offset = NAME_BLOCK * BLOCK_LEN + p * PACKET_LEN;
      dipped.packet(offset, &edited[offset]);
    }
    dipped.readDip = 80;
    dipped.readDipAt = 1100;
    dipped.readBlock(NAME_BLOCK * BLOCK_LEN, BLOCK_LEN);
    dipped.idle(300000);
    PollRun run(dipped);
    run.run();
    BusEngine &engine = run.rig.engine;
    CHECK(run.host.finished());
    CHECK(engine.poll.earlyUndone == BLOCK_LEN * 8);  // every data bit; the ACK slots have no dip
    CHECK(engine.poll.earlyWrong == 0);
    CHECK(radioBytes(run.host.sampled) == Bytes(names.begin(), names.begin() + BLOCK_LEN));
    CHECK(engine.verifyCount == 1 && engine.bitMismatches == 0);
    CHECK(engine.starts == BLOCK_LEN / PACKET_LEN + 2 && engine.stops == BLOCK_LEN / PACKET_LEN + 1);

    // The same dip early in the HIGH phase, when the radio may still be
    // reading SDA, is not answered at all.
    dipped.edges.clear();
    dipped.t = 100000;
    dipped.scl = dipped.sda = true;
    dipped.readDip = 0;
    for (uint16_t p = 0; p < BLOCK_LEN / PACKET_LEN; ++p) {
      const uint16_t offset = NAME_BLOCK * BLOCK_LEN + p * PACKET_LEN;
      dipped.packet(offset, &edited[offset]);
    }
    dipped.readDip = 80;
    dipped.readDipAt = 300;
    dipped.readBlock(NAME_BLOCK * BLOCK_LEN, BLOCK_LEN);
    dipped.idle(300000);
    PollRun early(dipped);
    early.run();
    CHECK(early.rig.engine.poll.earlyUndone == 0);
    CHECK(early.rig.engine.verifyCount == 1 && early.rig.engine.bitMismatches == 0);
    CHECK(radioBytes(early.host.sampled) == Bytes(names.begin(), names.begin() + BLOCK_LEN));
  });

  test("the decoder's forecast of each answer matched what it then did", [&] {
    CHECK(fallActions > 500000);
    CHECK(fallActionMismatches == 0);
  });

  std::printf("\n%d checks, %d failed\n", checks, failures);
  return failures == 0 ? 0 : 1;
}

/*
 * Watches the bus by polling while the bridge is armed.
 *
 * While armed, the loop on core 1 samples the pins continuously (about every
 * 0.4 µs) with interrupts masked the whole time, and hands each change to
 * the decoder. Nothing else runs on core 1, so no bus edge can be hidden.
 *
 * What the hardware runs of 2026-11-12 showed, and how this deals with it:
 *
 * 1. Interrupt-driven versions lost edges whenever an interrupt was late.
 *
 * 2. Glitches. SCL and SDA each show brief false levels of up to about 3 µs,
 *    for example an SCL dip when SDA falls at a START. A new level therefore
 *    counts only after it has lasted long enough:
 *      - SDA changes: 3 µs. Real SDA levels last 8 µs or more.
 *      - SCL falling: 1.5 µs. Real LOW phases last 12.5 µs or more. This
 *        filter delays every answer the bridge gives (ACK, next data bit),
 *        so it is kept short; with the pull-ups on, no SCL dips are seen.
 *      - SCL rising: seen in two samples in a row.
 *
 * 3. Order of near-simultaneous changes. The radio and its I²C devices change
 *    SDA within a µs of an SCL fall. Read in the wrong order, that looks like
 *    SDA changing while SCL is still HIGH: a false START or STOP. So SDA is
 *    sampled before SCL, and when both changes are accepted together, the
 *    clock edge that makes sense comes first (see applyChange).
 *
 * 4. Interrupt windows. An earlier version let pending interrupts run for a
 *    moment while the bus was idle. A window lasts 15–20 µs when a tick is
 *    pending, and up to 155 µs now and then. The radio starts each packet a
 *    fixed 1.63 ms after the previous one, which lined up with a window often
 *    enough to hide the START of about 33 of 752 packets. There are no
 *    windows now. The interrupt watchdog expects a tick on this core every
 *    300 ms; the loop stands in for that tick (see pollFeedWatchdog).
 *
 * 5. Answer time. When the bridge sends data (the radio's verify reads, and
 *    DATA IN), it must put the next bit on SDA before the radio raises SCL
 *    again, 12.5 µs after the fall at the earliest; a released line also
 *    needs a few µs to rise. A 3 µs fall filter plus an upkeep step of up to
 *    5 µs just before a fall left too little: about one bit in tens of
 *    thousands came up late. Upkeep now runs only right after an SCL rise
 *    (the next fall is about 15 µs away) or while the bus is still.
 *
 * 6. The bridge's own SDA changes. A line the bridge lets go of rises by
 *    itself, and the 3 µs SDA filter then reports it later still. If the
 *    clock rises in between, the decoder would see SDA change while SCL is
 *    HIGH: a wrong bit, then a STOP that ends the read. In the hardware runs
 *    this ended the radio's verify read after 5 and 19 blocks. So when the
 *    clock rises while the bridge's own change is still settling, that
 *    change is taken together with the clock rise, as the radio sees it.
 *
 * 7. Answering at first sight. Even so, the hardware runs of 2026-11-12
 *    (20:22, 20:23) showed a thin margin: something takes up to 4.2 µs from
 *    this core about 75 times a second, interrupts masked or not, and on top
 *    of a 3.5 µs answer a 1-bit was once up only 0.9 µs before the clock.
 *    What the bridge will do at the next SCL fall is known as soon as the
 *    clock has risen (BusEngine::fallAction). So SDA is now set the moment a
 *    fall is first seen, without waiting for the glitch filter. The decoder
 *    still gets the fall only after the filter, and does the same again. If
 *    the fall turns out to be a glitch, SDA is put back. This is done only
 *    once SCL has been HIGH for 8 µs: the radio reads SDA right after it
 *    raises the clock, and real HIGH phases last 14 µs or more.
 *
 * 8. Leaving. FreeRTOS takes this core's tick from a CPU timer compare
 *    (CCOMPARE0 against the cycle counter, which wraps every 17.9 s). When
 *    interrupts come back on, its handler adds one tick at a time until the
 *    compare value is ahead of the counter, using a signed difference. After
 *    a masked period whose length, modulo 17.9 s, is over 8.9 s, that
 *    difference already looks "ahead": the handler stops, and the next tick
 *    comes up to 8.9 s later. The interrupt watchdog then resets the board
 *    300 ms after the transfer. (Hardware run of 2026-11-13: both DATA IN
 *    sessions that lasted that long ended in such a reset; the reads of the
 *    days before stayed just under the limit.) So on leaving, the compare
 *    value is set one tick ahead of the counter (see pollLeave).
 *
 * Serial input (ABORT) is noticed by looking at the UART's receive FIFO.
 * The loop then returns to Arduino's loop(), with interrupts enabled, once
 * the bus has been idle for 2 ms. The radio's packets follow each other
 * 1.63 ms apart, so this cannot happen inside a running stream of packets.
 * As a fallback the loop also returns every two seconds until the radio has
 * first addressed the bridge. When watching resumes, the bus is taken as
 * found; a transaction that began meanwhile is skipped.
 */
#pragma once

#include "bus_engine.h"
#include "session.h"

#ifdef ARDUINO
#include "freertos/FreeRTOS.h"
#include "soc/uart_struct.h"
#include "xtensa/hal.h"
#endif

namespace bridge {

constexpr uint32_t SCL_RISE_FILTER_NS = 50;  // i.e. still HIGH at the next sample
constexpr uint32_t SCL_FALL_FILTER_NS = 1500;
constexpr uint32_t SDA_FILTER_NS = 3000;

constexpr uint32_t UPKEEP_INTERVAL_US = 1000;  // clock, watchdog and serial checks
constexpr uint32_t CALM_US = 50;               // bus still this long: nothing is under way
constexpr uint32_t UPKEEP_OVERDUE_US = 20000;  // do it anyway after this long
constexpr uint32_t LONG_GAP_NS = 1500;
constexpr uint32_t SHORT_LOW_US = 6;           // an SCL LOW shorter than this is no clock phase
constexpr uint32_t EARLY_MIN_HIGH_US = 8;      // answer at first sight only after this much SCL HIGH
constexpr uint32_t WATCHDOG_FEED_MS = 50;      // the interrupt watchdog allows 300 ms
constexpr uint32_t SESSION_INTERVAL_MS = 5;
constexpr uint32_t SERIAL_IDLE_US = 2000;      // bus idle this long before leaving for serial
constexpr uint32_t SERIAL_FALLBACK_MS = 2000;  // periodic check until the transfer starts

// ---- Platform ----
#ifdef ARDUINO
static inline __attribute__((always_inline)) uint32_t pollTicks() {
  return esp_cpu_get_cycle_count();
}
// SDA is read before SCL: a change that follows an SCL fall within the time
// between the two reads is then never seen ahead of that fall.
static inline __attribute__((always_inline)) uint8_t pollPins() {
  const uint32_t high = GPIO.in1.val;
  const uint32_t low = GPIO.in;
  return ((low >> SCL_PIN) & 1) | (((high >> (SENSE_PIN - 32)) & 1) << 1) |
         (((low >> DRIVE_PIN) & 1) << 2);
}
static inline __attribute__((always_inline)) void pollMask() { portDISABLE_INTERRUPTS(); }
static inline __attribute__((always_inline)) void pollUnmask() { portENABLE_INTERRUPTS(); }
// Sets this core's tick timer to fire one tick from now (see 8 above).
static inline void pollResyncTick(uint32_t ticksPerMs) {
  xthal_set_ccompare(0, xthal_get_ccount() + ticksPerMs);
}
static inline __attribute__((always_inline)) bool pollSerialPending() {
  return UART0.status.rxfifo_cnt != 0;
}
}  // namespace bridge
// Set by this core's tick interrupt in ESP-IDF (esp_system, int_wdt.c). Core 0
// feeds the interrupt watchdog only while it sees this flag being set.
extern "C" volatile bool int_wdt_cpu1_ticked;
namespace bridge {
// The interrupt watchdog expects a tick on each core every 300 ms. With
// interrupts masked here, stand in for this core's tick by setting the flag
// core 0 looks for before it feeds the watchdog. (Feeding the watchdog timer
// directly from this core did not prevent a reset; tested on the board.)
static inline __attribute__((always_inline)) void pollFeedWatchdog() {
  int_wdt_cpu1_ticked = true;
}
#else
struct PollHost {
  virtual uint32_t ticks() = 0;
  virtual uint8_t pins() = 0;
  virtual bool serialPending() { return false; }
  virtual void feedWatchdog() {}
  virtual void resyncTick() {}
  virtual ~PollHost() = default;
};
extern PollHost *pollHost;
static inline uint32_t pollTicks() { return pollHost->ticks(); }
static inline uint8_t pollPins() { return pollHost->pins(); }
static inline void pollMask() {}
static inline void pollUnmask() {}
static inline void pollResyncTick(uint32_t) { pollHost->resyncTick(); }
static inline bool pollSerialPending() { return pollHost->serialPending(); }
static inline void pollFeedWatchdog() { pollHost->feedWatchdog(); }
#endif

// Ends a period with interrupts masked: the tick timer first, then interrupts.
static inline void pollLeave(uint32_t ticksPerMs) {
  pollResyncTick(ticksPerMs);
  pollUnmask();
}

// Kept between calls of pollArmed.
struct PollState {
  uint32_t lastSerialMs = 0;
};

struct PollResult {
  Event event;  // DONE or FAIL when the session ended
  uint32_t nowMs = 0;
};

static inline __attribute__((always_inline)) void countWidth(uint32_t *bins, uint32_t ticks,
                                                             uint32_t ticksPerBin) {
  const uint32_t bin = ticks / ticksPerBin;
  ++bins[bin < WIDTH_BINS ? bin : WIDTH_BINS - 1];
}

// Hands an accepted change to the decoder. When SCL and SDA are accepted in
// the same sample, the clock edge is ordered the way data moves on the bus:
//   - SCL falling: the clock fall first, then the SDA change. Senders change
//     SDA right after the fall. (A START is not affected: SDA falls 9 µs
//     before SCL there, so it is accepted on its own first.)
//   - SCL rising: the SDA change first, then the clock rise. Data is set up
//     before it is clocked.
static inline __attribute__((always_inline)) void applyChange(BusEngine &engine, uint8_t from,
                                                              uint8_t to) {
  const bool local = to & PIN_DRIVE;
  const uint8_t changed = (from ^ to) & (PIN_SCL | PIN_SDA);
  if (changed == (PIN_SCL | PIN_SDA)) {
    const bool sclFalls = from & PIN_SCL;
    const uint8_t middle =
        sclFalls ? ((to & PIN_SCL) | (from & PIN_SDA)) : ((from & PIN_SCL) | (to & PIN_SDA));
    engine.onBusChange(middle & PIN_SCL, middle & PIN_SDA, local);
  }
  engine.onBusChange(to & PIN_SCL, to & PIN_SDA, local);
}

#ifdef ARDUINO
#define POLL_HOT __attribute__((optimize("O2")))
#else
#define POLL_HOT
#endif

// Polls until the session ends or the loop should look at the serial port.
// Not a template and not inline, so it can live in IRAM (see bus_engine.h).
IRAM_ATTR POLL_HOT PollResult pollArmed(BusEngine &engine, Session &session, PollState &state,
                                        uint32_t nowMs) {
  PollStats &stats = engine.poll;
  const uint32_t perUs = engine.ticksPerUs ? engine.ticksPerUs : 1;
  const uint32_t perMs = perUs * 1000;
  const uint32_t sclRiseFilter = SCL_RISE_FILTER_NS * perUs / 1000;
  const uint32_t sclFallFilter = SCL_FALL_FILTER_NS * perUs / 1000;
  const uint32_t sdaFilter = SDA_FILTER_NS * perUs / 1000;
  const uint32_t glitchBin = perUs / 10 ? perUs / 10 : 1;  // 0.1 µs
  const uint32_t highBin = perUs / 2 ? perUs / 2 : 1;      // 0.5 µs
  const uint32_t longGap = LONG_GAP_NS * perUs / 1000;
  const uint32_t shortLow = SHORT_LOW_US * perUs;
  const uint32_t earlyMinHigh = EARLY_MIN_HIGH_US * perUs;

  pollMask();
  // Take the bus as it is: whatever happened while nobody watched is skipped.
  uint8_t stable = pollPins();
  engine.resync(stable & PIN_SCL, stable & PIN_SDA);
  uint8_t pending = 0;  // pins whose raw level differs from `stable`
  uint32_t sclSince = 0, sdaSince = 0;
  uint32_t now = pollTicks();
  uint32_t lastSample = now;
  uint32_t samples = 0;  // since the last upkeep
  uint32_t maxGap = stats.maxGapTicks;
  uint32_t lastChange = now;
  uint32_t lastUpkeep = now;
  uint32_t msMark = now;
  uint32_t riseSeen = now;
  bool highTimed = false;
  uint32_t fallSeen = now;
  bool lowTimed = false;
  uint32_t lastRead = now;  // the previous pin read, whatever was done since
  uint32_t sclBlind = 0;    // time not looking, just before SCL was first seen changed
  // Timing of a line the bridge let go of: 0 none, 1 waiting for SDA HIGH, 2 seen HIGH.
  uint8_t riseState = 0;
  uint32_t releasedAt = now, sdaHighAt = now;
  // An answer given at first sight of an SCL fall that is still in the filter.
  bool early = false;
  bool earlyLow = false;  // what SDA was set to
  uint32_t lastSessionMs = nowMs;
  uint32_t lastFeedMs = nowMs;
  bool serialPending = false;
  bool final = false;
  Event event;
  pollFeedWatchdog();

  for (;;) {
    const uint8_t pins = pollPins();
    now = pollTicks();
    ++samples;
    const uint32_t gap = now - lastSample;
    if (gap > maxGap) maxGap = gap;
    if (gap > longGap) ++stats.longGaps;
    lastSample = now;
    const uint32_t sinceRead = now - lastRead;
    lastRead = now;

    const uint8_t diff = (pins ^ stable) & (PIN_SCL | PIN_SDA);
    uint8_t accept = 0;
    if (diff & PIN_SCL) {
      if (!(pending & PIN_SCL)) {
        pending |= PIN_SCL;
        sclSince = now;
        sclBlind = sinceRead;
        if ((stable & PIN_SCL) && highTimed && now - riseSeen >= earlyMinHigh) {
          const BusEngine::FallAction action = engine.fallAction();
          if (action != BusEngine::FallAction::None) {
            earlyLow = action == BusEngine::FallAction::PullLow;
            if (earlyLow) {
              sdaPullLow();
            } else {
              sdaRelease();
            }
            early = true;
            const uint32_t at = pollTicks();
            const uint32_t took = at - now + sinceRead;  // from the last look before the fall
            countWidth(stats.answerTime, took, perUs);
            if (took > stats.maxAnswerTicks) stats.maxAnswerTicks = took;
            ++stats.earlyAnswers;
            // A 1 after a 0: time how long the line takes to come up.
            if (action == BusEngine::FallAction::SendHigh && engine.drivingLow &&
                !(stable & PIN_SDA)) {
              riseState = 1;
              releasedAt = at;
            }
          }
        }
      } else if (now - sclSince >= ((stable & PIN_SCL) ? sclFallFilter : sclRiseFilter)) {
        accept |= PIN_SCL;
      }
    } else if (pending & PIN_SCL) {
      pending &= ~PIN_SCL;  // it went back: a glitch
      countWidth((stable & PIN_SCL) ? stats.sclDips : stats.sclSpikes, now - sclSince, glitchBin);
      if (early) {  // the clock did not fall after all: put SDA back
        if (engine.drivingLow) {
          sdaPullLow();
        } else {
          sdaRelease();
        }
        early = false;
        riseState = 0;
        ++stats.earlyUndone;
      }
    }
    if (diff & PIN_SDA) {
      if (!(pending & PIN_SDA)) {
        pending |= PIN_SDA;
        sdaSince = now;
        if (riseState == 1 && (pins & PIN_SDA)) {
          countWidth(stats.sdaRise, now - releasedAt, perUs);
          sdaHighAt = now;
          riseState = 2;
        }
      } else if (now - sdaSince >= sdaFilter) {
        accept |= PIN_SDA;
      }
    } else if (pending & PIN_SDA) {
      pending &= ~PIN_SDA;
      countWidth((stable & PIN_SDA) ? stats.sdaDips : stats.sdaSpikes, now - sdaSince, glitchBin);
    }

    // The clock rises while the bridge's own SDA change is still settling:
    // take that change with the clock rise (see 6 above).
    if ((accept & PIN_SCL) && !(stable & PIN_SCL) && (pending & PIN_SDA) && !(accept & PIN_SDA) &&
        engine.ownSdaChange(pins & PIN_SDA)) {
      accept |= PIN_SDA;
      ++stats.sdaWithClock;
    }

    if (accept) {
      const uint8_t next = ((stable ^ accept) & (PIN_SCL | PIN_SDA)) | (pins & PIN_DRIVE);
      const bool sclRose = (accept & PIN_SCL) && (next & PIN_SCL);
      const bool sclFell = (accept & PIN_SCL) && !sclRose;
      if (sclRose) {
        riseSeen = sclSince;  // when the rise was first seen
        highTimed = true;
        if (lowTimed) {
          const uint32_t low = sclSince - fallSeen;  // first seen LOW to first seen HIGH
          if (low < shortLow) {
            ++stats.shortLows;
          } else if (low < stats.minSclLowTicks) {
            stats.minSclLowTicks = low;
          }
          lowTimed = false;
        }
        if (riseState == 2) {
          const int32_t setup = int32_t(sclSince - sdaHighAt);
          if (setup <= 0) {
            ++stats.sdaLate;
          } else {
            countWidth(stats.sdaLead, uint32_t(setup), perUs);
            if (uint32_t(setup) < stats.minSetupTicks) stats.minSetupTicks = uint32_t(setup);
          }
        } else if (riseState == 1) {
          ++stats.sdaLate;
        }
        riseState = 0;
      } else if (sclFell) {
        if (highTimed) {
          const uint32_t width = sclSince - riseSeen;  // first seen HIGH to first seen LOW
          countWidth(stats.sclHigh, width, highBin);
          if (width < stats.minSclHighTicks) stats.minSclHighTicks = width;
          highTimed = false;
        }
        fallSeen = sclSince;
        lowTimed = true;
      }
      // A fall the bridge answers that was not answered at first sight.
      const bool plain = sclFell && !early && engine.fallAction() != BusEngine::FallAction::None;
      applyChange(engine, stable, next);
      const uint32_t answered = pollTicks();
      if (answered - now > stats.maxStepTicks) stats.maxStepTicks = answered - now;
      stable = next;
      pending &= ~accept;
      if (sclFell && early) {
        early = false;
        // The decoder has now done the same. If it decided otherwise, it rules.
        if (bool(engine.drivingLow) != earlyLow) {
          if (engine.drivingLow) {
            sdaPullLow();
          } else {
            sdaRelease();
          }
          riseState = 0;
          ++stats.earlyWrong;
        }
      } else if (plain) {
        const uint32_t took = answered - sclSince + sclBlind;
        countWidth(stats.answerTime, took, perUs);
        if (took > stats.maxAnswerTicks) stats.maxAnswerTicks = took;
        ++stats.plainAnswers;
        riseState = (engine.sendingHigh() && !(stable & PIN_SDA)) ? 1 : 0;
        releasedAt = answered;
      }
      lastChange = now;
      lastSample = answered;  // the decoder's own time is not a sampling gap
      // Right after an SCL rise nothing is urgent: the fall is about 15 µs away.
      if (!sclRose || pending || answered - lastUpkeep < UPKEEP_INTERVAL_US * perUs) continue;
      now = answered;
    } else {
      if (pending) continue;
      const uint32_t sinceUpkeep = now - lastUpkeep;
      if (sinceUpkeep < UPKEEP_INTERVAL_US * perUs) continue;
      // Otherwise only while the bus is still, or when long overdue.
      if (now - lastChange < CALM_US * perUs && sinceUpkeep < UPKEEP_OVERDUE_US * perUs) continue;
    }

    // Upkeep, about once a millisecond. It takes up to about 5 µs, so it runs
    // only when the bridge has nothing to answer soon.
    lastUpkeep = now;
    stats.samples += samples;
    samples = 0;
    stats.maxGapTicks = maxGap;
    if (now - msMark >= perMs) {
      const uint32_t elapsed = (now - msMark) / perMs;
      nowMs += elapsed;
      msMark += elapsed * perMs;
      stats.sampledTicks += uint64_t(elapsed) * perMs;
    }
    if (engine.mode == Mode::Off) break;
    ++stats.upkeeps;
    if (nowMs - lastFeedMs >= WATCHDOG_FEED_MS) {
      lastFeedMs = nowMs;
      pollFeedWatchdog();
      ++stats.watchdogFeeds;
    }
    if (pollSerialPending()) serialPending = true;
    if (nowMs - lastSessionMs >= SESSION_INTERVAL_MS) {
      lastSessionMs = nowMs;
      event = session.tick(nowMs);
      if (event.kind == EventKind::Done || event.kind == EventKind::Fail) {
        final = true;
        break;
      }
      event = Event{};
    }
    const bool idle = (stable & (PIN_SCL | PIN_SDA)) == (PIN_SCL | PIN_SDA) &&
                      now - lastChange >= SERIAL_IDLE_US * perUs;
    const bool waiting = engine.activity == 0;  // the radio has not addressed the bridge yet
    if (idle && (serialPending || (waiting && nowMs - state.lastSerialMs >= SERIAL_FALLBACK_MS))) {
      ++stats.serialReturns;
      break;
    }
    const uint32_t done = pollTicks();
    if (done - now > stats.maxUpkeepTicks) stats.maxUpkeepTicks = done - now;
    lastSample = done;  // upkeep is reported separately from sampling gaps
  }
  stats.samples += samples;
  stats.maxGapTicks = maxGap;
  pollLeave(perMs);
  state.lastSerialMs = nowMs;
  PollResult result;
  if (final) result.event = event;
  result.nowMs = nowMs;
  return result;
}

}  // namespace bridge

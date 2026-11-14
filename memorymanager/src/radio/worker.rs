//! A background thread that owns the bridge connection and runs transfers,
//! so the TUI never blocks on the serial port.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use super::link::{Link, LinkError};
use super::log::Logger;
use super::protocol::{self, BLOCK_COUNT, BridgeEvent, Direction, Hello, Stats};

/// Extra time beyond the bridge's arm window before giving up locally. The
/// bridge is silent while the radio transfers and reports once at the end,
/// so this covers a transfer started at the very end of the window. The
/// bridge's own timeouts normally end a run long before this.
const SAFETY_MARGIN: Duration = Duration::from_secs(180);
const EVENT_POLL: Duration = Duration::from_millis(100);

pub struct Connection<P> {
    pub link: Link<P>,
    pub hello: Hello,
    pub port_name: String,
}

/// Opens a connection to the bridge: a serial port in the app, a fake in tests.
pub trait Connector: Send + 'static {
    type Port: Read + Write + Send + 'static;
    fn connect(&mut self) -> Result<Connection<Self::Port>, LinkError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RadioRequest {
    /// The radio's DATA OUT: read its memory.
    Read,
    /// The radio's DATA IN: write this image to it.
    Write(Vec<u8>),
    /// Cancel the running transfer.
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    Connecting,
    Uploading { done: usize, total: usize },
    /// Armed; the user now starts the transfer on the radio.
    WaitingForRadio { direction: Direction, window: Duration },
    Transferring { direction: Direction, stats: Stats },
    Downloading { done: usize, total: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RadioEvent {
    Connected { port: String, firmware: String },
    Stage(Stage),
    ReadFinished { image: Vec<u8>, stats: Stats },
    WriteFinished { stats: Stats },
    Failed { message: String },
    Cancelled,
}

pub struct Worker {
    jobs: Sender<Job>,
    events: Receiver<RadioEvent>,
    cancel: Arc<AtomicBool>,
}

enum Job {
    Read,
    Write(Vec<u8>),
}

impl Worker {
    /// Starts the worker thread. `log` receives a transcript of each transfer.
    pub fn spawn<C: Connector>(connector: C, log: Logger) -> Self {
        let (jobs, job_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        thread::Builder::new()
            .name("radio-bridge".into())
            .spawn(move || run(connector, job_rx, event_tx, thread_cancel, log))
            .expect("spawn the radio bridge thread");
        Self { jobs, events, cancel }
    }

    pub fn request(&self, request: RadioRequest) {
        let job = match request {
            RadioRequest::Cancel => return self.cancel.store(true, Ordering::SeqCst),
            RadioRequest::Read => Job::Read,
            RadioRequest::Write(image) => Job::Write(image),
        };
        self.cancel.store(false, Ordering::SeqCst);
        let _ = self.jobs.send(job);
    }

    pub fn try_recv(&self) -> Option<RadioEvent> {
        self.events.try_recv().ok()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Option<RadioEvent> {
        self.events.recv_timeout(timeout).ok()
    }
}

enum Stop {
    Cancelled,
    Failed(String),
}

impl From<LinkError> for Stop {
    fn from(err: LinkError) -> Self {
        Stop::Failed(capitalize(&err.to_string()))
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect::<String>() + ".",
        None => String::new(),
    }
}

fn run<C: Connector>(
    mut connector: C,
    jobs: Receiver<Job>,
    events: Sender<RadioEvent>,
    cancel: Arc<AtomicBool>,
    log: Logger,
) {
    let mut connection: Option<Connection<C::Port>> = None;
    for job in jobs {
        log.note(match &job {
            Job::Read => "=== Read from radio (DATA OUT)",
            Job::Write(_) => "=== Write to radio (DATA IN)",
        });
        let outcome = execute(&mut connector, &mut connection, job, &events, &cancel, &log);
        // The bridge keeps its counters until the next arm: record them for every run.
        if let Some(connection) = connection.as_mut()
            && let Err(err) = connection.link.diag()
        {
            log.note(&format!("DIAG failed: {err}"));
        }
        // The bridge's last bus events show how a transfer went wrong: after a
        // failure, and after a run the radio itself did not accept in full.
        let unclean = match &outcome {
            Ok(RadioEvent::ReadFinished { stats, .. }) => {
                stats.verify < BLOCK_COUNT as u32 || stats.mismatches > 0
            }
            Ok(RadioEvent::WriteFinished { stats }) => stats.mismatches > 0,
            Err(Stop::Failed(_)) => true,
            _ => false,
        };
        if unclean
            && let Some(connection) = connection.as_mut()
            && let Err(err) = connection.link.trace()
        {
            log.note(&format!("TRACE failed: {err}"));
        }
        let event = match outcome {
            Ok(event) => {
                log.note("=== Finished");
                event
            }
            Err(Stop::Cancelled) => {
                log.note("=== Cancelled");
                RadioEvent::Cancelled
            }
            Err(Stop::Failed(message)) => {
                log.note(&format!("=== Failed: {message}"));
                // Start fresh next time: the port or the bridge may be in a bad state.
                connection = None;
                let message = match log.path() {
                    Some(path) => format!("{message} Details: {}", path.display()),
                    None => message,
                };
                RadioEvent::Failed { message }
            }
        };
        if events.send(event).is_err() {
            return;
        }
    }
}

fn execute<C: Connector>(
    connector: &mut C,
    connection: &mut Option<Connection<C::Port>>,
    job: Job,
    events: &Sender<RadioEvent>,
    cancel: &AtomicBool,
    log: &Logger,
) -> Result<RadioEvent, Stop> {
    let emit = |stage| {
        let _ = events.send(RadioEvent::Stage(stage));
    };
    if connection.is_none() {
        emit(Stage::Connecting);
        let mut fresh = connector.connect()?;
        fresh.link.set_logger(log.clone());
        log.note(&format!("Connected to {} (bridge firmware {})", fresh.port_name, fresh.hello.firmware));
        let _ = events.send(RadioEvent::Connected {
            port: fresh.port_name.clone(),
            firmware: fresh.hello.firmware.clone(),
        });
        *connection = Some(fresh);
    }
    let link = &mut connection.as_mut().expect("connected").link;

    match job {
        Job::Read => {
            check_cancel(cancel)?;
            let window = link.arm(Direction::Out, 0)?;
            emit(Stage::WaitingForRadio { direction: Direction::Out, window });
            let stats = wait_for_done(link, Direction::Out, window, &emit, cancel)?;
            let image = link.download(|done| emit(Stage::Downloading { done, total: BLOCK_COUNT }))?;
            if stats.crc.is_some_and(|crc| crc != protocol::crc32(&image)) {
                return Err(Stop::Failed("The downloaded image does not match the bridge's checksum.".into()));
            }
            Ok(RadioEvent::ReadFinished { image, stats })
        }
        Job::Write(image) => {
            emit(Stage::Uploading { done: 0, total: BLOCK_COUNT });
            let crc = link.upload(&image, |done| emit(Stage::Uploading { done, total: BLOCK_COUNT }))?;
            check_cancel(cancel)?;
            let window = link.arm(Direction::In, crc)?;
            emit(Stage::WaitingForRadio { direction: Direction::In, window });
            let stats = wait_for_done(link, Direction::In, window, &emit, cancel)?;
            Ok(RadioEvent::WriteFinished { stats })
        }
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), Stop> {
    if cancel.swap(false, Ordering::SeqCst) { Err(Stop::Cancelled) } else { Ok(()) }
}

fn wait_for_done<P: Read + Write>(
    link: &mut Link<P>,
    direction: Direction,
    window: Duration,
    emit: &impl Fn(Stage),
    cancel: &AtomicBool,
) -> Result<Stats, Stop> {
    let mut deadline = Instant::now() + window + SAFETY_MARGIN;
    loop {
        if cancel.swap(false, Ordering::SeqCst) {
            link.abort()?;
            return Err(Stop::Cancelled);
        }
        if Instant::now() >= deadline {
            let _ = link.abort();
            return Err(Stop::Failed("The bridge stopped reporting progress.".into()));
        }
        match link.next_event(EVENT_POLL)? {
            Some(BridgeEvent::Progress(dir, stats)) if dir == direction => {
                // The bridge times out a stalled radio itself; keep waiting while it reports.
                deadline = Instant::now() + window + SAFETY_MARGIN;
                emit(Stage::Transferring { direction, stats });
            }
            Some(BridgeEvent::Done(dir, stats)) if dir == direction => return Ok(stats),
            Some(BridgeEvent::Fail(dir, reason, _)) => {
                return Err(if reason == "aborted" {
                    Stop::Cancelled
                } else {
                    Stop::Failed(protocol::describe_failure(dir, &reason))
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fake::{FakeBridge, FakeConfig};
    use super::super::link::Timeouts;
    use super::*;
    use crate::image::IMAGE_LEN;

    struct FakeConnector {
        bridge: FakeBridge,
        connects: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Connector for FakeConnector {
        type Port = FakeBridge;
        fn connect(&mut self) -> Result<Connection<FakeBridge>, LinkError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            let timeouts = Timeouts {
                reply: Duration::from_millis(300),
                hello: Duration::from_millis(100),
                hello_attempts: 3,
            };
            let mut link = Link::new(self.bridge.clone(), timeouts);
            let hello = link.handshake()?;
            Ok(Connection { link, hello, port_name: "/dev/fake".into() })
        }
    }

    fn start(config: FakeConfig) -> (Worker, FakeBridge, Arc<std::sync::atomic::AtomicUsize>) {
        let bridge = FakeBridge::new(config);
        let connects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker = Worker::spawn(
            FakeConnector { bridge: bridge.clone(), connects: Arc::clone(&connects) },
            Logger::disabled(),
        );
        (worker, bridge, connects)
    }

    /// Collects events until a final one (finished, failed or cancelled).
    fn finish(worker: &Worker) -> Vec<RadioEvent> {
        let mut events = Vec::new();
        while let Some(event) = worker.recv_timeout(Duration::from_secs(5)) {
            let last = matches!(
                event,
                RadioEvent::ReadFinished { .. }
                    | RadioEvent::WriteFinished { .. }
                    | RadioEvent::Failed { .. }
                    | RadioEvent::Cancelled
            );
            events.push(event);
            if last {
                return events;
            }
        }
        panic!("no final event; got {events:?}");
    }

    fn pattern(seed: u8) -> Vec<u8> {
        (0..IMAGE_LEN).map(|i| (i as u8) ^ seed).collect()
    }

    #[test]
    fn read_reports_each_stage_and_returns_the_radio_image() {
        let (worker, _bridge, _) = start(FakeConfig { radio_image: pattern(0x5A), ..FakeConfig::default() });
        worker.request(RadioRequest::Read);
        let events = finish(&worker);
        assert_eq!(events[0], RadioEvent::Stage(Stage::Connecting));
        assert_eq!(events[1], RadioEvent::Connected { port: "/dev/fake".into(), firmware: "1.0.0".into() });
        assert!(events.contains(&RadioEvent::Stage(Stage::WaitingForRadio {
            direction: Direction::Out,
            window: Duration::from_secs(120),
        })));
        assert!(events.iter().any(|e| matches!(e, RadioEvent::Stage(Stage::Transferring { .. }))));
        assert!(events.contains(&RadioEvent::Stage(Stage::Downloading { done: 47, total: 47 })));
        match events.last().unwrap() {
            RadioEvent::ReadFinished { image, stats } => {
                assert_eq!(image, &pattern(0x5A));
                assert_eq!((stats.packets, stats.verify), (752, 47));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn write_uploads_then_the_radio_reads_it() {
        let (worker, bridge, connects) = start(FakeConfig::default());
        worker.request(RadioRequest::Write(pattern(3)));
        let events = finish(&worker);
        assert!(events.contains(&RadioEvent::Stage(Stage::Uploading { done: 47, total: 47 })));
        assert!(matches!(events.last(), Some(RadioEvent::WriteFinished { stats }) if stats.passes == 2));
        assert_eq!(bridge.received_by_radio(), Some(pattern(3)));

        // The connection is reused for the next job.
        worker.request(RadioRequest::Read);
        assert!(matches!(finish(&worker).last(), Some(RadioEvent::ReadFinished { .. })));
        assert_eq!(connects.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn radio_failures_are_explained_and_the_link_is_reset() {
        let (worker, _bridge, connects) =
            start(FakeConfig { radio_fails: Some("wrong_direction".into()), ..FakeConfig::default() });
        worker.request(RadioRequest::Read);
        match finish(&worker).last() {
            Some(RadioEvent::Failed { message }) => assert!(message.contains("FM + LIGHT"), "{message}"),
            other => panic!("{other:?}"),
        }
        worker.request(RadioRequest::Read);
        finish(&worker);
        assert_eq!(connects.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cancel_aborts_while_waiting_for_the_radio() {
        let (worker, bridge, _) = start(FakeConfig { radio_never_starts: true, ..FakeConfig::default() });
        worker.request(RadioRequest::Read);
        loop {
            match worker.recv_timeout(Duration::from_secs(5)) {
                Some(RadioEvent::Stage(Stage::WaitingForRadio { .. })) => break,
                Some(_) => {}
                None => panic!("never armed"),
            }
        }
        worker.request(RadioRequest::Cancel);
        assert_eq!(finish(&worker).last(), Some(&RadioEvent::Cancelled));
        assert!(bridge.commands().contains(&"ABORT".to_owned()));
    }

    #[test]
    fn the_log_records_traffic_and_diagnostics_after_a_failure() {
        let path = std::env::temp_dir().join(format!("mm-worker-log-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let bridge = FakeBridge::new(FakeConfig { radio_fails: Some("incomplete".into()), ..FakeConfig::default() });
        let connects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker = Worker::spawn(
            FakeConnector { bridge: bridge.clone(), connects },
            Logger::open(&path).unwrap(),
        );
        worker.request(RadioRequest::Read);
        match finish(&worker).last() {
            Some(RadioEvent::Failed { message }) => {
                assert!(message.ends_with(&format!("Details: {}", path.display())), "{message}")
            }
            other => panic!("{other:?}"),
        }
        let log = std::fs::read_to_string(&path).unwrap();
        for expected in [
            "* === Read from radio (DATA OUT)",
            "* Connected to /dev/fake (bridge firmware 1.0.0)",
            "> ARM OUT",
            "< EV FAIL dir=out reason=incomplete",
            "> DIAG",
            "< # state=idle",
            "> TRACE",
            "< # +36 S0 D0 G1 L0 Address>Address b0",
            "* === Failed: The radio stopped before the transfer was complete.",
        ] {
            assert!(log.contains(expected), "missing {expected:?} in:\n{log}");
        }
        assert!(log.find("> DIAG").unwrap() > log.find("< EV FAIL").unwrap());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_log_keeps_the_bus_trace_when_the_radio_rejects_a_read() {
        // Hardware runs of 2026-11-12: the image arrived complete, but the
        // radio's own verification stopped early and it showed FAILED.
        let path = std::env::temp_dir().join(format!("mm-worker-unclean-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let bridge = FakeBridge::new(FakeConfig { verified_blocks: 19, ..FakeConfig::default() });
        let connects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker = Worker::spawn(
            FakeConnector { bridge: bridge.clone(), connects },
            Logger::open(&path).unwrap(),
        );
        worker.request(RadioRequest::Read);
        match finish(&worker).last() {
            Some(RadioEvent::ReadFinished { stats, .. }) => {
                assert_eq!((stats.verify, stats.mismatches), (19, 1));
            }
            other => panic!("{other:?}"),
        }
        let log = std::fs::read_to_string(&path).unwrap();
        assert!(log.contains("< EV DONE dir=out packets=752 verify=19 mism=1"), "{log}");
        assert!(log.find("> TRACE").unwrap() > log.find("> DIAG").unwrap(), "{log}");
        let _ = std::fs::remove_file(&path);

        // A clean read needs no trace.
        let path = std::env::temp_dir().join(format!("mm-worker-clean-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let bridge = FakeBridge::new(FakeConfig::default());
        let connects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker = Worker::spawn(FakeConnector { bridge, connects }, Logger::open(&path).unwrap());
        worker.request(RadioRequest::Read);
        assert!(matches!(finish(&worker).last(), Some(RadioEvent::ReadFinished { .. })));
        let log = std::fs::read_to_string(&path).unwrap();
        assert!(log.contains("> DIAG") && !log.contains("> TRACE"), "{log}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn connection_problems_are_reported() {
        let (worker, _, _) = start(FakeConfig { proto: 2, ..FakeConfig::default() });
        worker.request(RadioRequest::Read);
        match finish(&worker).last() {
            Some(RadioEvent::Failed { message }) => {
                assert!(message.contains("flash the bridge firmware"), "{message}")
            }
            other => panic!("{other:?}"),
        }
        let (worker, _, _) = start(FakeConfig { bus_busy: true, ..FakeConfig::default() });
        worker.request(RadioRequest::Read);
        assert!(matches!(finish(&worker).last(), Some(RadioEvent::Failed { message }) if message.contains("bus is busy")));
    }
}

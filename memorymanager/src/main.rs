//! Terminal UI for editing ATS-909X2 presets in DATA OUT images.

mod app;
mod browser;
mod ui;

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use memory_manager::image::EepromImage;
use memory_manager::radio::log::Logger;
use memory_manager::radio::port::SerialConnector;
use memory_manager::radio::worker::Worker;
use ratatui::DefaultTerminal;

use app::{App, Document};

const USAGE: &str = "\
Usage: memory-manager [--port SERIAL_PORT] [--demo] [IMAGE.bin]

Editor for the presets of a Sangean ATS-909X2: FM, MW, LW, SW and AIR.
IMAGE.bin is a 12,032-byte DATA OUT export (EEPROM 0x0000–0x2EFF).
Without an image the editor starts empty; press R to read the radio or o to open an image.
--demo opens a made-up image to try the editor with; it cannot be written to a radio.

R reads the radio and W writes it through an ESP32 running the bridge firmware.
The bridge's USB serial port is found automatically; --port names it instead,
for example --port /dev/cu.usbserial-0001.";

/// How often the screen refreshes while waiting for keys, for live transfer progress.
const TICK: Duration = Duration::from_millis(100);

struct Options {
    image: Option<PathBuf>,
    port: Option<String>,
    /// Open the built-in sample image.
    demo: bool,
}

fn parse_args(args: Vec<OsString>) -> Result<Option<Options>, ()> {
    let mut options = Options { image: None, port: None, demo: false };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-h" | "--help") => {
                println!("{USAGE}");
                return Ok(None);
            }
            Some("-V" | "--version") => {
                println!("memory-manager {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            Some("--demo") => options.demo = true,
            Some("--port") => {
                let port = args.next().and_then(|p| p.into_string().ok()).ok_or(())?;
                options.port = Some(port);
            }
            Some(flag) if flag.starts_with('-') => return Err(()),
            _ if options.image.is_none() => options.image = Some(PathBuf::from(arg)),
            _ => return Err(()),
        }
    }
    Ok(Some(options))
}

fn main() -> ExitCode {
    let options = match parse_args(std::env::args_os().skip(1).collect()) {
        Ok(Some(options)) => options,
        Ok(None) => return ExitCode::SUCCESS,
        Err(()) => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let doc = match options.image {
        Some(path) => match EepromImage::load(&path) {
            Ok(image) => Some(Document { path: Some(app::resolved(path)), image }),
            Err(err) => {
                eprintln!("memory-manager: cannot open {}: {err}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None if options.demo => Some(Document { path: None, image: memory_manager::sample::demo() }),
        None => None,
    };

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, App::new(doc), options.port);
    ratatui::restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("memory-manager: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(terminal: &mut DefaultTerminal, mut app: App, port: Option<String>) -> io::Result<()> {
    // Started on the first transfer, so the serial port is only opened when needed.
    let mut worker: Option<Worker> = None;
    let log = Logger::open_default();
    app.log_path = log.path().map(PathBuf::from);
    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, &app))?;
        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.handle_key(key);
        }
        for request in app.take_radio_requests() {
            worker
                .get_or_insert_with(|| {
                    Worker::spawn(SerialConnector::new(port.clone(), log.clone()), log.clone())
                })
                .request(request);
        }
        if let Some(worker) = &worker {
            while let Some(event) = worker.try_recv() {
                app.on_radio_event(event);
            }
        }
    }
    Ok(())
}

//! TUI state and key handling. Holds no terminal resources, so it can be
//! driven directly from tests.
//!
//! File commands follow the usual File menu: New starts an untitled image,
//! Save writes an image that already has a file and asks for a name
//! otherwise, Save As always asks, and replacing an existing file needs
//! confirmation.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use memory_manager::image::{
    Band, EepromImage, ImageDiff, NAME_LEN, NameRef, Page, PresetRef, Region, SLOTS, SW_MAX_KHZ,
    SW_MIN_KHZ, Slot, is_name_char, name_text, parse_frequency,
};
use memory_manager::radio::protocol::{BLOCK_COUNT, Direction, Stats};
use memory_manager::radio::worker::{RadioEvent, RadioRequest, Stage};

use crate::browser::{Browser, EntryKind};

/// The open image and its file.
pub struct Document {
    /// `None` for a new image that has never been saved.
    pub path: Option<PathBuf>,
    pub image: EepromImage,
}

impl Document {
    pub fn display_name(&self) -> String {
        self.path
            .as_deref()
            .and_then(Path::file_name)
            .map_or_else(|| "Untitled".to_owned(), |name| name.to_string_lossy().into_owned())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Pages,
    Slots,
}

pub enum Mode {
    Normal,
    Prompt(Prompt),
    Confirm(Confirm),
    /// The Open dialog.
    Browse(Browser),
    /// The Save / Save As dialog.
    Save(SaveDialog),
    /// A transfer to or from the radio through the ESP32 bridge.
    Radio(Transfer),
    Help,
}

/// What a radio transfer is for.
pub enum TransferKind {
    /// `R`: read the radio, then save the image.
    Read,
    /// `b` before a write: read the radio into a backup file next to the image.
    Backup,
    /// `W`: write the current file to the radio.
    Write { file: String, image: Vec<u8> },
    /// `v` after a write: read the radio back and compare.
    Verify { expected: Vec<u8> },
}

pub enum Outcome {
    Written { warning: Option<String> },
    Verified { diff: ImageDiff },
    Failed(String),
}

/// Progress of a radio transfer, updated from the worker's events.
pub struct Transfer {
    pub kind: TransferKind,
    pub connection: Option<String>,
    pub upload: Option<(usize, usize)>,
    /// Direction, when the bridge was armed, and how long it waits for the radio.
    pub waiting: Option<(Direction, Instant, Duration)>,
    pub progress: Option<(Direction, Stats)>,
    pub download: Option<(usize, usize)>,
    pub outcome: Option<Outcome>,
    pub cancelling: bool,
}

impl Transfer {
    fn new(kind: TransferKind) -> Self {
        Self {
            kind,
            connection: None,
            upload: None,
            waiting: None,
            progress: None,
            download: None,
            outcome: None,
            cancelling: false,
        }
    }

    pub fn active(&self) -> bool {
        self.outcome.is_none()
    }
}

/// The most recent image read from the radio in this session.
pub struct RadioBackup {
    pub image: EepromImage,
    /// Local time of the read, `HH:MM`.
    pub taken: String,
}

pub struct Prompt {
    pub kind: PromptKind,
    pub input: LineInput,
    pub error: Option<String>,
    initial: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    Frequency { at: PresetRef },
    /// The name of an SW page, or of a preset on another band.
    Name { name: NameRef },
    Open,
}

pub enum Confirm {
    /// Unsaved changes would be lost by `next`.
    Discard { next: AfterDiscard },
    /// The chosen file exists. Cancelling returns to the dialog.
    Overwrite { path: PathBuf, dialog: Box<SaveDialog> },
    /// Writing to the radio needs the image saved first.
    SaveBeforeRadio,
    /// Last check before DATA IN: what changes compared with `reference`.
    WriteRadio { file: String, reference: String, diff: ImageDiff, has_backup: bool },
}

/// What to do once unsaved changes are saved or discarded.
pub enum AfterDiscard {
    Quit,
    New,
    Open(Box<Document>),
    ReadRadio,
    WriteRadio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogFocus {
    Name,
    List,
}

pub struct SaveDialog {
    pub browser: Browser,
    pub name: LineInput,
    pub focus: DialogFocus,
    pub error: Option<String>,
    /// Save As, as opposed to Save of an image that has no file yet.
    pub save_as: bool,
    /// The quit, New or Open to continue with after saving.
    then: Option<AfterDiscard>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Error,
}

pub struct Status {
    pub text: String,
    pub kind: StatusKind,
}

pub struct App {
    pub doc: Option<Document>,
    pub region: Region,
    pub band: Band,
    /// The page last shown on each band, so switching bands comes back to it.
    band_pages: [Page; Band::ALL.len()],
    pub page: Page,
    pub slot: Slot,
    pub focus: Focus,
    pub mode: Mode,
    pub status: Option<Status>,
    pub should_quit: bool,
    /// Folder of the last file opened or saved; file dialogs start here when
    /// the current image has no file.
    last_dir: Option<PathBuf>,
    /// The latest image read from the radio in this session.
    pub radio_backup: Option<RadioBackup>,
    /// The current image came from the radio and has not been saved yet.
    pub radio_unsaved: bool,
    /// The current image came from a read the radio did not verify, so parts
    /// of it may not be the radio's memory. It is never written to a radio.
    pub unverified: bool,
    /// File name offered when an untitled image is first saved.
    untitled_name: Option<String>,
    /// Requests for the radio worker, collected by `main`.
    radio_requests: Vec<RadioRequest>,
    /// Where the radio transfer transcript is written, shown on the transfer panel.
    pub log_path: Option<PathBuf>,
}

impl App {
    pub fn new(doc: Option<Document>) -> Self {
        let last_dir = doc
            .as_ref()
            .and_then(|doc| doc.path.as_deref())
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        Self {
            doc,
            region: Region::BankA,
            band: Band::Sw,
            band_pages: [Page::new(1).expect("page 1 exists"); Band::ALL.len()],
            page: Page::new(1).expect("page 1 exists"),
            slot: Slot::new(1).expect("slot 1 exists"),
            focus: Focus::Pages,
            mode: Mode::Normal,
            status: None,
            should_quit: false,
            last_dir,
            radio_backup: None,
            radio_unsaved: false,
            unverified: false,
            untitled_name: None,
            radio_requests: Vec::new(),
            log_path: None,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.doc.as_ref().is_some_and(|doc| doc.image.is_modified()) || self.radio_unsaved
    }

    /// Hands pending requests to the radio worker.
    pub fn take_radio_requests(&mut self) -> Vec<RadioRequest> {
        std::mem::take(&mut self.radio_requests)
    }

    /// Applies an event from the radio worker.
    pub fn on_radio_event(&mut self, event: RadioEvent) {
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        let Mode::Radio(mut transfer) = mode else {
            self.mode = mode;
            return;
        };
        self.mode = match event {
            RadioEvent::Connected { port, firmware } => {
                transfer.connection = Some(format!("{port} (bridge firmware {firmware})"));
                Mode::Radio(transfer)
            }
            RadioEvent::Stage(stage) => {
                match stage {
                    Stage::Connecting => {}
                    Stage::Uploading { done, total } => transfer.upload = Some((done, total)),
                    Stage::WaitingForRadio { direction, window } => {
                        transfer.waiting = Some((direction, Instant::now(), window));
                    }
                    Stage::Transferring { direction, stats } => transfer.progress = Some((direction, stats)),
                    Stage::Downloading { done, total } => transfer.download = Some((done, total)),
                }
                Mode::Radio(transfer)
            }
            RadioEvent::ReadFinished { image, stats } => self.finish_read(transfer, &image, &stats),
            RadioEvent::WriteFinished { stats } => {
                let warning = (stats.mismatches > 0).then(|| {
                    format!(
                        "{} bits read back differently at the radio's contact. The radio may show FAILED; \
                         read back to check.",
                        stats.mismatches
                    )
                });
                transfer.outcome = Some(Outcome::Written { warning });
                Mode::Radio(transfer)
            }
            RadioEvent::Failed { message } => {
                transfer.outcome = Some(Outcome::Failed(message));
                Mode::Radio(transfer)
            }
            RadioEvent::Cancelled => {
                self.info("Transfer cancelled. The radio was not changed by this transfer.");
                Mode::Normal
            }
        };
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        self.mode = match mode {
            Mode::Normal => {
                self.status = None;
                self.on_normal_key(key)
            }
            Mode::Prompt(prompt) => self.on_prompt_key(prompt, key),
            Mode::Confirm(confirm) => self.on_confirm_key(confirm, key),
            Mode::Browse(browser) => self.on_browse_key(browser, key),
            Mode::Save(dialog) => self.on_save_key(dialog, key),
            Mode::Radio(transfer) => self.on_radio_key(transfer, key),
            Mode::Help => Mode::Normal,
        };
    }

    fn on_normal_key(&mut self, key: KeyEvent) -> Mode {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => return self.request_quit(),
            KeyCode::Char('c') if ctrl => return self.request_quit(),
            KeyCode::Char('?') | KeyCode::F(1) => return Mode::Help,
            KeyCode::Char('n') => return self.request_new(),
            KeyCode::Char('o') => return self.open_browser(),
            KeyCode::Char('s') => return self.save(None),
            KeyCode::Char('S') => return self.save_dialog(true, None),
            KeyCode::Char('R') => return self.request_read_radio(),
            KeyCode::Char('W') => return self.request_write_radio(),
            _ => {}
        }
        if self.doc.is_none() {
            self.info("No image open. Press n for a new image or o to open one.");
            return Mode::Normal;
        }
        match key.code {
            KeyCode::Tab => self.step_region(1),
            KeyCode::BackTab => self.step_region(-1),
            KeyCode::Char('b') => self.step_band(1),
            KeyCode::Char('B') => self.step_band(-1),
            KeyCode::Char('L') => self.toggle_lock(),
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Pages,
            KeyCode::Right | KeyCode::Char('l') => self.focus = Focus::Slots,
            KeyCode::Up | KeyCode::Char('k') => self.step_focused(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step_focused(1),
            KeyCode::PageUp => self.step_page(if self.focus == Focus::Pages { -10 } else { -1 }),
            KeyCode::PageDown => self.step_page(if self.focus == Focus::Pages { 10 } else { 1 }),
            KeyCode::Home | KeyCode::Char('g') => self.step_focused(-100),
            KeyCode::End | KeyCode::Char('G') => self.step_focused(100),
            KeyCode::Enter | KeyCode::Char('e') => {
                return match self.focus {
                    Focus::Pages if self.band == Band::Sw => self.rename_prompt(),
                    // Only SW pages have names: Enter opens the page instead.
                    Focus::Pages => {
                        self.focus = Focus::Slots;
                        Mode::Normal
                    }
                    Focus::Slots => self.frequency_prompt(""),
                };
            }
            KeyCode::Char('r') => return self.rename_prompt(),
            KeyCode::Char(c) if c.is_ascii_digit() && self.focus == Focus::Slots => {
                return self.frequency_prompt(&c.to_string());
            }
            KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('x') => self.clear_slot(),
            KeyCode::Char('u') => self.revert_focused(),
            _ => {}
        }
        Mode::Normal
    }

    fn on_prompt_key(&mut self, mut prompt: Prompt, key: KeyEvent) -> Mode {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Mode::Normal,
            KeyCode::Char('c') if ctrl => return Mode::Normal,
            KeyCode::Enter => return self.submit_prompt(prompt),
            KeyCode::Char(c) if !ctrl => {
                prompt.error = accept_char(prompt.kind, &prompt.input, c).err();
                if prompt.error.is_none() {
                    prompt.input.insert(c);
                }
                return Mode::Prompt(prompt);
            }
            _ => prompt.input.edit(key),
        }
        prompt.error = None;
        Mode::Prompt(prompt)
    }

    fn on_confirm_key(&mut self, confirm: Confirm, key: KeyEvent) -> Mode {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let cancel = matches!(key.code, KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('c'))
            || (ctrl && key.code == KeyCode::Char('c'));
        match confirm {
            Confirm::Discard { next } => match key.code {
                _ if cancel => {
                    self.info("Cancelled.");
                    Mode::Normal
                }
                KeyCode::Char('s') => self.save(Some(next)),
                KeyCode::Char('d') | KeyCode::Char('y') => self.proceed(next),
                _ => Mode::Confirm(Confirm::Discard { next }),
            },
            Confirm::SaveBeforeRadio => match key.code {
                _ if cancel => {
                    self.info("Write to radio cancelled.");
                    Mode::Normal
                }
                KeyCode::Char('s') => self.save(Some(AfterDiscard::WriteRadio)),
                _ => Mode::Confirm(Confirm::SaveBeforeRadio),
            },
            Confirm::WriteRadio { file, reference, diff, has_backup } => match key.code {
                _ if cancel => {
                    self.info("Write to radio cancelled.");
                    Mode::Normal
                }
                KeyCode::Char('y') => self.start_write(),
                KeyCode::Char('b') if !has_backup => self.start_transfer(TransferKind::Backup),
                _ => Mode::Confirm(Confirm::WriteRadio { file, reference, diff, has_backup }),
            },
            Confirm::Overwrite { path, dialog } => match key.code {
                _ if cancel => Mode::Save(*dialog),
                KeyCode::Char('y') => self.finish_save(*dialog, path),
                _ => Mode::Confirm(Confirm::Overwrite { path, dialog }),
            },
        }
    }

    fn on_browse_key(&mut self, mut browser: Browser, key: KeyEvent) -> Mode {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Mode::Normal,
            KeyCode::Char('c') if ctrl => return Mode::Normal,
            KeyCode::Char('/') => return self.open_prompt(&browser.dir),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let Some((kind, path)) = browser.selected_entry().map(|e| (e.kind, e.path.clone())) else {
                    return Mode::Browse(browser);
                };
                match kind {
                    EntryKind::Parent => browser.parent(),
                    EntryKind::Dir => browser.enter(path),
                    EntryKind::File { .. } => match self.open_file(path) {
                        Ok(mode) => return mode,
                        Err(message) => browser.error = Some(message),
                    },
                }
            }
            _ => navigate(&mut browser, key),
        }
        Mode::Browse(browser)
    }

    /// Save dialog keys. Arrow keys move the highlight and hand control to
    /// the folder list, so Enter then opens the highlighted folder (or picks
    /// the highlighted file); typing goes back to the file name, and Enter
    /// then saves. Tab switches explicitly.
    fn on_save_key(&mut self, mut dialog: SaveDialog, key: KeyEvent) -> Mode {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        dialog.error = None;
        let in_list = dialog.focus == DialogFocus::List;
        match key.code {
            KeyCode::Esc => return self.cancel_save(),
            KeyCode::Char('c') if ctrl => return self.cancel_save(),
            KeyCode::Tab | KeyCode::BackTab => {
                dialog.focus = if in_list { DialogFocus::Name } else { DialogFocus::List };
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                navigate(&mut dialog.browser, key);
                dialog.focus = DialogFocus::List;
            }
            KeyCode::Enter if !in_list => return self.submit_save_name(dialog),
            KeyCode::Enter => {
                let Some(entry) = dialog.browser.selected_entry().cloned() else {
                    return Mode::Save(dialog);
                };
                match entry.kind {
                    EntryKind::Parent => dialog.browser.parent(),
                    EntryKind::Dir => dialog.browser.enter(entry.path),
                    // Choosing an existing file means saving over it, which is confirmed first.
                    EntryKind::File { .. } => {
                        dialog.name = LineInput::new(&entry.name);
                        return self.submit_save_name(dialog);
                    }
                }
            }
            KeyCode::Char('u') if ctrl => {
                dialog.focus = DialogFocus::Name;
                dialog.name.edit(key);
            }
            KeyCode::Char(c) if !ctrl => {
                if c.is_control() {
                    dialog.error = Some("Control characters are not allowed.".into());
                } else {
                    dialog.focus = DialogFocus::Name;
                    dialog.name.insert(c);
                }
            }
            KeyCode::Right if in_list => {
                if let Some(entry) = dialog.browser.selected_entry().cloned() {
                    match entry.kind {
                        EntryKind::Parent => dialog.browser.parent(),
                        EntryKind::Dir => dialog.browser.enter(entry.path),
                        EntryKind::File { .. } => {}
                    }
                }
            }
            KeyCode::Left | KeyCode::Backspace if in_list => dialog.browser.parent(),
            KeyCode::Home | KeyCode::End if in_list => navigate(&mut dialog.browser, key),
            _ if !in_list => dialog.name.edit(key),
            _ => {}
        }
        Mode::Save(dialog)
    }

    fn submit_prompt(&mut self, mut prompt: Prompt) -> Mode {
        let text = prompt.input.text();
        let result = match prompt.kind {
            PromptKind::Frequency { at } => self.apply_frequency(at, &text),
            PromptKind::Name { name } => {
                if text == prompt.initial {
                    self.info("Name unchanged; existing bytes kept.");
                    Ok(Mode::Normal)
                } else {
                    self.apply_name(name, &text)
                }
            }
            PromptKind::Open => self.open_path(&text),
        };
        match result {
            Ok(mode) => mode,
            Err(message) => {
                prompt.error = Some(message);
                Mode::Prompt(prompt)
            }
        }
    }

    /// Resolves the typed name against the dialog's folder and saves, asking
    /// first if the file exists.
    fn submit_save_name(&mut self, mut dialog: SaveDialog) -> Mode {
        let text = dialog.name.text();
        let Some(typed) = expand_path(&text) else {
            dialog.error = Some("Enter a file name.".into());
            dialog.focus = DialogFocus::Name;
            return Mode::Save(dialog);
        };
        let mut path = dialog.browser.dir.join(typed);
        if path.is_dir() {
            dialog.browser.enter(path);
            dialog.name.clear();
            return Mode::Save(dialog);
        }
        if path.extension().is_none() {
            path.set_extension("bin");
        }
        if let Some(folder) = path.parent().filter(|folder| !folder.is_dir()) {
            dialog.error = Some(format!("Folder {} does not exist.", folder.display()));
            dialog.focus = DialogFocus::Name;
            return Mode::Save(dialog);
        }
        if path.exists() {
            return Mode::Confirm(Confirm::Overwrite { path, dialog: Box::new(dialog) });
        }
        self.finish_save(dialog, path)
    }

    fn finish_save(&mut self, mut dialog: SaveDialog, path: PathBuf) -> Mode {
        match self.write_doc(path) {
            Ok(()) => match dialog.then.take() {
                Some(next) => self.proceed(next),
                None => Mode::Normal,
            },
            Err(message) => {
                dialog.error = Some(message);
                Mode::Save(dialog)
            }
        }
    }

    fn cancel_save(&mut self) -> Mode {
        self.info("Save cancelled; changes are still unsaved.");
        Mode::Normal
    }

    fn apply_frequency(&mut self, at: PresetRef, text: &str) -> Result<Mode, String> {
        let khz = parse_frequency(at.band, text).map_err(|e| capitalize(&e.to_string()))?;
        let doc = self.doc.as_mut().ok_or("No image open.")?;
        let before = doc.image.frequency(at);
        doc.image.set_frequency(at, khz).map_err(|e| capitalize(&e.to_string()))?;
        self.info(format!(
            "{}: {} → {}",
            preset_label(at),
            frequency_text(at.band, before),
            at.band.frequency_text(khz)
        ));
        Ok(Mode::Normal)
    }

    fn apply_name(&mut self, name: NameRef, text: &str) -> Result<Mode, String> {
        let doc = self.doc.as_mut().ok_or("No image open.")?;
        doc.image.set_name(name, text).map_err(|e| capitalize(&e.to_string()))?;
        self.info(match name {
            NameRef::SwPage(_, page) => format!("Page {} renamed to \"{text}\".", page.number()),
            NameRef::Preset(at) => format!("{} named \"{text}\".", preset_label(at)),
        });
        Ok(Mode::Normal)
    }

    fn open_path(&mut self, text: &str) -> Result<Mode, String> {
        let path = expand_path(text).ok_or("Enter the path of a .bin image.")?;
        self.open_file(path)
    }

    fn open_file(&mut self, path: PathBuf) -> Result<Mode, String> {
        let image = EepromImage::load(&path).map_err(|e| format!("Cannot open: {e}"))?;
        let doc = Document { path: Some(resolved(path)), image };
        if self.is_dirty() {
            return Ok(Mode::Confirm(Confirm::Discard {
                next: AfterDiscard::Open(Box::new(doc)),
            }));
        }
        self.install(doc);
        Ok(Mode::Normal)
    }

    /// New clears a copy of an image of the user's own radio, so it needs one:
    /// the open image, or the backup read in this session.
    fn request_new(&mut self) -> Mode {
        if self.doc.is_none() && self.radio_backup.is_none() {
            self.info("New starts from your radio's own image: read the radio (R) or open an image first.");
            return Mode::Normal;
        }
        if self.is_dirty() {
            return Mode::Confirm(Confirm::Discard { next: AfterDiscard::New });
        }
        self.proceed(AfterDiscard::New)
    }

    fn request_quit(&mut self) -> Mode {
        if self.is_dirty() {
            Mode::Confirm(Confirm::Discard { next: AfterDiscard::Quit })
        } else {
            self.should_quit = true;
            Mode::Normal
        }
    }

    fn proceed(&mut self, next: AfterDiscard) -> Mode {
        match next {
            AfterDiscard::Quit => self.should_quit = true,
            AfterDiscard::New => {
                let (base, source) = match (&self.doc, &self.radio_backup) {
                    (Some(doc), _) => (&doc.image, doc.display_name()),
                    (None, Some(backup)) => (&backup.image, format!("the radio backup read at {}", backup.taken)),
                    (None, None) => return Mode::Normal,
                };
                let image = base.cleared();
                self.install(Document { path: None, image });
                self.info(format!(
                    "New image: all presets and names cleared, on every band. Everything else is as in {source}."
                ));
            }
            AfterDiscard::Open(doc) => self.install(*doc),
            AfterDiscard::ReadRadio => return self.start_transfer(TransferKind::Read),
            AfterDiscard::WriteRadio => return self.prepare_write(),
        }
        Mode::Normal
    }

    fn install(&mut self, doc: Document) {
        self.radio_unsaved = false;
        self.unverified = false;
        self.untitled_name = None;
        if let Some(path) = &doc.path {
            self.last_dir = path.parent().map(Path::to_path_buf);
            self.info(format!("Opened {}.", path.display()));
        }
        if !doc.image.has_product_string() {
            self.error("This image lacks the radio's product string, so it cannot be written to a radio.");
        }
        self.doc = Some(doc);
    }

    /// Save: writes straight to the image's file, or asks for a name if it has none.
    fn save(&mut self, then: Option<AfterDiscard>) -> Mode {
        let Some(doc) = &self.doc else {
            self.info("No image open.");
            return Mode::Normal;
        };
        let Some(path) = doc.path.clone() else {
            return self.save_dialog(false, then);
        };
        match self.write_doc(path) {
            Ok(()) => match then {
                Some(next) => self.proceed(next),
                None => Mode::Normal,
            },
            Err(message) => {
                self.error(format!("{message} Changes are still unsaved."));
                Mode::Normal
            }
        }
    }

    fn write_doc(&mut self, path: PathBuf) -> Result<(), String> {
        let doc = self.doc.as_mut().ok_or("No image open.")?;
        doc.image
            .save(&path)
            .map_err(|err| format!("Save failed: {} ({err}).", path.display()))?;
        let path = resolved(path);
        doc.path = Some(path.clone());
        self.radio_unsaved = false;
        self.untitled_name = None;
        self.last_dir = path.parent().map(Path::to_path_buf);
        self.info(format!("Saved {}.", path.display()));
        Ok(())
    }

    /// The folder file dialogs start in, and the current file to preselect.
    fn dialog_start(&self) -> (PathBuf, Option<PathBuf>) {
        let current = self.doc.as_ref().and_then(|doc| doc.path.clone());
        let dir = current
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| self.last_dir.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        (dir, current)
    }

    fn open_browser(&mut self) -> Mode {
        let (dir, current) = self.dialog_start();
        Mode::Browse(Browser::new(dir, current.as_deref()))
    }

    fn save_dialog(&mut self, save_as: bool, then: Option<AfterDiscard>) -> Mode {
        let Some(doc) = &self.doc else {
            self.info("No image open.");
            return Mode::Normal;
        };
        let name = match &doc.path {
            Some(_) => doc.display_name(),
            None => self.untitled_name.clone().unwrap_or_default(),
        };
        self.save_dialog_named(save_as, then, &name)
    }

    fn save_dialog_named(&mut self, save_as: bool, then: Option<AfterDiscard>, name: &str) -> Mode {
        let (dir, current) = self.dialog_start();
        Mode::Save(SaveDialog {
            browser: Browser::new(dir, current.as_deref()),
            name: LineInput::new(name),
            focus: DialogFocus::Name,
            error: None,
            save_as,
            then,
        })
    }

    fn request_read_radio(&mut self) -> Mode {
        if self.is_dirty() {
            return Mode::Confirm(Confirm::Discard { next: AfterDiscard::ReadRadio });
        }
        self.start_transfer(TransferKind::Read)
    }

    fn request_write_radio(&mut self) -> Mode {
        let Some(doc) = &self.doc else {
            self.info("No image open. Open or read an image first.");
            return Mode::Normal;
        };
        if self.unverified {
            self.error(format!("Not written: the radio did not verify the read this image came from. {REREAD}"));
            return Mode::Normal;
        }
        if !doc.image.has_product_string() {
            self.error(
                "Not written: this image lacks the radio's product string (SANGEANATS-909X2 at 0x0F50). \
                 It is not an image of a radio.",
            );
            return Mode::Normal;
        }
        if doc.path.is_none() || self.is_dirty() {
            return Mode::Confirm(Confirm::SaveBeforeRadio);
        }
        self.prepare_write()
    }

    /// The confirmation before DATA IN. With a radio backup from this session
    /// it lists what the file changes; without one there is nothing to
    /// compare with.
    fn prepare_write(&mut self) -> Mode {
        let Some(doc) = &self.doc else { return Mode::Normal };
        let (diff, reference) = match &self.radio_backup {
            Some(backup) => (doc.image.compare(&backup.image), format!("the radio backup read at {}", backup.taken)),
            None => (ImageDiff::default(), String::new()),
        };
        Mode::Confirm(Confirm::WriteRadio {
            file: doc.display_name(),
            reference,
            diff,
            has_backup: self.radio_backup.is_some(),
        })
    }

    fn start_write(&mut self) -> Mode {
        let Some(doc) = &self.doc else { return Mode::Normal };
        let kind = TransferKind::Write { file: doc.display_name(), image: doc.image.as_bytes().to_vec() };
        self.start_transfer(kind)
    }

    fn start_transfer(&mut self, kind: TransferKind) -> Mode {
        let request = match &kind {
            TransferKind::Write { image, .. } => RadioRequest::Write(image.clone()),
            TransferKind::Read | TransferKind::Backup | TransferKind::Verify { .. } => RadioRequest::Read,
        };
        self.radio_requests.push(request);
        Mode::Radio(Transfer::new(kind))
    }

    fn on_radio_key(&mut self, mut transfer: Transfer, key: KeyEvent) -> Mode {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if transfer.active() {
            match key.code {
                KeyCode::Esc => {
                    if !transfer.cancelling {
                        transfer.cancelling = true;
                        self.radio_requests.push(RadioRequest::Cancel);
                    }
                }
                KeyCode::Char('c') if ctrl => {
                    if !transfer.cancelling {
                        transfer.cancelling = true;
                        self.radio_requests.push(RadioRequest::Cancel);
                    }
                }
                KeyCode::Char('q') => self.info("A transfer is running. Press Esc to cancel it first."),
                _ => {}
            }
            return Mode::Radio(transfer);
        }
        match (key.code, &transfer.kind, &transfer.outcome) {
            (KeyCode::Char('v'), TransferKind::Write { image, .. }, Some(Outcome::Written { .. })) => {
                let expected = image.clone();
                self.start_transfer(TransferKind::Verify { expected })
            }
            (KeyCode::Char('v'), TransferKind::Verify { expected }, Some(Outcome::Failed(_))) => {
                let expected = expected.clone();
                self.start_transfer(TransferKind::Verify { expected })
            }
            (KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q'), _, _) => Mode::Normal,
            _ => Mode::Radio(transfer),
        }
    }

    fn finish_read(&mut self, mut transfer: Transfer, bytes: &[u8], stats: &Stats) -> Mode {
        let image = match EepromImage::from_bytes(bytes) {
            Ok(image) => image,
            Err(err) => {
                transfer.outcome = Some(Outcome::Failed(format!("The bridge returned an unusable image: {err}.")));
                return Mode::Radio(transfer);
            }
        };
        let stamp = Stamp::now();
        // Only a read the radio verified in full is known to be its memory.
        let verified = stats.verify == BLOCK_COUNT as u32 && stats.mismatches == 0;
        if verified {
            self.radio_backup = Some(RadioBackup { image: image.clone(), taken: stamp.time.clone() });
        }
        let warning = read_warning(stats);
        match transfer.kind {
            TransferKind::Read => {
                self.install(Document { path: None, image });
                self.radio_unsaved = true;
                self.unverified = !verified;
                let suffix = if verified { "" } else { "-unverified" };
                self.untitled_name = Some(format!("radio-{}{suffix}.bin", stamp.file));
                match warning {
                    Some(warning) => self.error(warning),
                    None => self.info("Read from the radio. Choose where to save it."),
                }
                self.save_dialog(false, None)
            }
            TransferKind::Backup if !verified => {
                let warning = warning.unwrap_or_default();
                transfer.outcome = Some(Outcome::Failed(format!("No backup saved and nothing written. {warning}")));
                Mode::Radio(transfer)
            }
            TransferKind::Backup => {
                let (dir, _) = self.dialog_start();
                let path = unique_path(&dir, &format!("radio-backup-{}", stamp.file));
                let mut backup = image;
                if let Err(err) = backup.save(&path) {
                    transfer.outcome =
                        Some(Outcome::Failed(format!("Could not save the backup to {}: {err}.", path.display())));
                    return Mode::Radio(transfer);
                }
                let saved = format!("Radio backup saved to {}.", path.display());
                match warning {
                    Some(warning) => self.error(format!("{saved} {warning}")),
                    None => self.info(saved),
                }
                self.prepare_write()
            }
            TransferKind::Verify { .. } if !verified => {
                transfer.outcome = Some(Outcome::Failed(
                    "The radio did not verify the read-back, so it cannot show whether the write succeeded. \
                     Tune FM to an empty frequency, with no station, then press v to read back again."
                        .into(),
                ));
                Mode::Radio(transfer)
            }
            TransferKind::Verify { ref expected } => {
                let expected = EepromImage::from_bytes(expected).expect("written image is valid");
                transfer.outcome = Some(Outcome::Verified { diff: image.compare(&expected) });
                Mode::Radio(transfer)
            }
            TransferKind::Write { .. } => Mode::Radio(transfer),
        }
    }

    fn open_prompt(&mut self, dir: &Path) -> Mode {
        let initial = dir.join("").display().to_string();
        self.prompt(PromptKind::Open, &initial)
    }

    /// Renames the SW page, or on another band the selected preset.
    fn rename_prompt(&mut self) -> Mode {
        let at = self.at();
        let name = match at.name() {
            None => NameRef::SwPage(self.region, self.page),
            Some(_) if self.focus == Focus::Pages => {
                self.focus = Focus::Slots;
                self.info(format!("On {}, each preset has its own name: pick one and press r.", at.band.label()));
                return Mode::Normal;
            }
            Some(name) => name,
        };
        let Some(doc) = &self.doc else { return Mode::Normal };
        if matches!(name, NameRef::Preset(_)) && doc.image.frequency(at).is_none() {
            self.info("Store a frequency first; an empty position has no name.");
            return Mode::Normal;
        }
        let current = name_text(&doc.image.name(name));
        let initial = current.trim_end_matches(' ').to_owned();
        self.prompt(PromptKind::Name { name }, &initial)
    }

    fn frequency_prompt(&mut self, initial: &str) -> Mode {
        self.focus = Focus::Slots;
        let at = self.at();
        // Typed digits start a fresh value; Enter starts from the current one.
        let start = if initial.is_empty() {
            self.selected_khz().map(|khz| at.band.number_text(khz)).unwrap_or_default()
        } else {
            initial.to_owned()
        };
        self.prompt(PromptKind::Frequency { at }, &start)
    }

    fn prompt(&self, kind: PromptKind, initial: &str) -> Mode {
        Mode::Prompt(Prompt {
            kind,
            input: LineInput::new(initial),
            error: None,
            initial: initial.to_owned(),
        })
    }

    fn clear_slot(&mut self) {
        if self.focus != Focus::Slots {
            self.info("Select a slot (→) to clear it.");
            return;
        }
        let at = self.at();
        let Some(doc) = self.doc.as_mut() else { return };
        let before = doc.image.frequency(at);
        doc.image.clear_preset(at);
        if before.is_some() {
            self.info(format!("{} cleared (was {}).", preset_label(at), frequency_text(at.band, before)));
        }
    }

    /// Locks or unlocks the selected preset (the radio's Memory Lock).
    fn toggle_lock(&mut self) {
        if self.focus != Focus::Slots {
            self.info("Select a slot (→) to lock or unlock it.");
            return;
        }
        let at = self.at();
        let Some(doc) = self.doc.as_mut() else { return };
        let locked = !doc.image.is_locked(at);
        match doc.image.set_locked(at, locked) {
            Ok(()) => self.info(format!("{} {}.", preset_label(at), if locked { "locked" } else { "unlocked" })),
            Err(err) => self.error(capitalize(&err.to_string())),
        }
    }

    fn revert_focused(&mut self) {
        let (at, page, slot, focus) = (self.at(), self.page, self.slot, self.focus);
        let page_name = NameRef::SwPage(self.region, page);
        let Some(doc) = self.doc.as_mut() else { return };
        let preset_name = at.name().filter(|&name| doc.image.is_name_modified(name));
        let message = match focus {
            Focus::Slots if doc.image.is_preset_modified(at) || preset_name.is_some() => {
                doc.image.revert_preset(at);
                if let Some(name) = preset_name {
                    doc.image.revert_name(name);
                }
                format!("Slot {} reverted to {}.", slot.number(), frequency_text(at.band, doc.image.frequency(at)))
            }
            Focus::Pages if at.band == Band::Sw && doc.image.is_name_modified(page_name) => {
                doc.image.revert_name(page_name);
                format!("Page {} name reverted.", page.number())
            }
            _ => "Nothing to revert here.".to_owned(),
        };
        self.info(message);
    }

    /// The selected preset.
    pub fn at(&self) -> PresetRef {
        PresetRef::new(self.region, self.band, self.page, self.slot).expect("the page is kept within the band")
    }

    fn selected_khz(&self) -> Option<u32> {
        self.doc.as_ref()?.image.frequency(self.at())
    }

    fn step_region(&mut self, delta: isize) {
        let index = (self.region.index() as isize + delta).rem_euclid(Region::ALL.len() as isize);
        self.region = Region::ALL[index as usize];
    }

    fn step_band(&mut self, delta: isize) {
        self.band_pages[self.band.index()] = self.page;
        let index = (self.band.index() as isize + delta).rem_euclid(Band::ALL.len() as isize);
        self.band = Band::ALL[index as usize];
        self.page = self.band_pages[self.band.index()];
    }

    fn step_focused(&mut self, delta: i32) {
        match self.focus {
            Focus::Pages => self.step_page(delta),
            Focus::Slots => {
                let n = (i32::from(self.slot.number()) + delta).clamp(1, i32::from(SLOTS));
                self.slot = Slot::new(n as u8).expect("clamped to 1..=9");
            }
        }
    }

    fn step_page(&mut self, delta: i32) {
        let n = (i32::from(self.page.number()) + delta).clamp(1, i32::from(self.band.pages()));
        self.page = Page::new(n as u8).expect("clamped to the band's pages");
    }

    fn info(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), kind: StatusKind::Info });
    }

    fn error(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), kind: StatusKind::Error });
    }
}

/// List navigation shared by the Open and Save dialogs.
fn navigate(browser: &mut Browser, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => browser.move_by(-1),
        KeyCode::Down | KeyCode::Char('j') => browser.move_by(1),
        KeyCode::PageUp => browser.move_by(-10),
        KeyCode::PageDown => browser.move_by(10),
        KeyCode::Home | KeyCode::Char('g') => browser.move_by(isize::MIN / 2),
        KeyCode::End | KeyCode::Char('G') => browser.move_by(isize::MAX / 2),
        KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => browser.parent(),
        KeyCode::Char('~') => browser.home(),
        KeyCode::Char('a') => browser.toggle_all(),
        KeyCode::Char('.') => browser.toggle_hidden(),
        _ => {}
    }
}

/// Live filter for typed characters; the image editor validates again on submit.
fn accept_char(kind: PromptKind, input: &LineInput, c: char) -> Result<(), String> {
    match kind {
        PromptKind::Frequency { at } if at.band == Band::Sw => {
            if !c.is_ascii_digit() {
                Err(format!("Digits only: whole kHz from {SW_MIN_KHZ} to {SW_MAX_KHZ}."))
            } else if input.len() >= 5 {
                Err(format!("At most 5 digits (max {SW_MAX_KHZ} kHz)."))
            } else {
                Ok(())
            }
        }
        PromptKind::Frequency { at } => {
            let band = at.band;
            let longest = band.number_text(band.max_khz()).len();
            let point = c == '.' && band.in_mhz() && !input.chars().contains(&'.');
            if !c.is_ascii_digit() && !point {
                Err(if band.in_mhz() {
                    format!("Digits and one decimal point: {} from {}.", band.unit(), band.range_text())
                } else {
                    format!("Digits only: whole kHz from {}.", band.range_text())
                })
            } else if input.len() >= longest {
                Err(format!("At most {longest} characters ({}).", band.range_text()))
            } else if let Some(point_at) = input.chars().iter().position(|&ch| ch == '.')
                && c != '.'
                && input.len() - point_at > band.decimals()
            {
                Err(format!("{} takes {} decimals.", band.label(), band.decimals()))
            } else {
                Ok(())
            }
        }
        PromptKind::Name { .. } => {
            if !is_name_char(c) {
                Err(format!("{c:?} is not allowed; use ASCII letters, digits and spaces."))
            } else if input.len() >= NAME_LEN {
                Err(format!("Names are at most {NAME_LEN} characters."))
            } else {
                Ok(())
            }
        }
        PromptKind::Open => {
            if c.is_control() { Err("Control characters are not allowed.".into()) } else { Ok(()) }
        }
    }
}

/// Warns when the radio's own DATA OUT verification did not complete cleanly.
fn read_warning(stats: &Stats) -> Option<String> {
    let mut parts = Vec::new();
    if stats.verify < BLOCK_COUNT as u32 {
        parts.push(format!(
            "The radio did not verify this read ({} of {BLOCK_COUNT} blocks) and probably showed FAILED. \
             Parts of the image may be RDS radio text instead of memory.",
            stats.verify
        ));
    }
    if stats.mismatches > 0 {
        parts.push(format!("{} bits read back differently at the radio's contact.", stats.mismatches));
    }
    (!parts.is_empty()).then(|| format!("{} Do not write it to the radio. {REREAD}", parts.join(" ")))
}

/// What to do when a read was not verified. While DATA OUT runs the radio
/// is on FM, and RDS radio text from the station can get into the export.
const REREAD: &str = "Tune FM to an empty frequency, with no station, and read again.";

/// Local date and time for file names (`2026-11-12-1530`) and labels (`15:30`).
struct Stamp {
    file: String,
    time: String,
}

impl Stamp {
    fn now() -> Self {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let time = secs as libc::time_t;
        // SAFETY: `tm` is plain data, and localtime_r only writes to it.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let ok = unsafe { !libc::localtime_r(&time, &mut tm).is_null() };
        if !ok {
            return Self { file: format!("{secs}"), time: "now".to_owned() };
        }
        Self {
            file: format!(
                "{:04}-{:02}-{:02}-{:02}{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min
            ),
            time: format!("{:02}:{:02}", tm.tm_hour, tm.tm_min),
        }
    }
}

/// `dir/stem.bin`, or `dir/stem-2.bin` and so on if it exists.
fn unique_path(dir: &Path, stem: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.bin"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem}-{n}.bin")))
        .find(|path| !path.exists())
        .expect("some name is free")
}

/// A preset's frequency for messages: `9765 kHz`, `91.30 MHz` or `empty`.
pub fn frequency_text(band: Band, khz: Option<u32>) -> String {
    khz.map_or_else(|| "empty".to_owned(), |khz| band.frequency_text(khz))
}

/// How messages name a preset: `Page 49 slot 1` on SW, `FM page 1 slot 3` elsewhere.
pub fn preset_label(at: PresetRef) -> String {
    let (page, slot) = (at.page().number(), at.slot.number());
    match at.band {
        Band::Sw => format!("Page {page} slot {slot}"),
        band => format!("{} page {page} slot {slot}", band.label()),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect::<String>() + ".",
        None => String::new(),
    }
}

/// Trims the input and expands a leading `~`. Returns `None` for blank input.
fn expand_path(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (text.strip_prefix('~'), home) {
        (Some(""), Some(home)) => Some(home),
        (Some(rest), Some(home)) if rest.starts_with('/') => Some(home.join(&rest[1..])),
        _ => Some(PathBuf::from(text)),
    }
}

/// An absolute path without `.`/`..` or symlinks where the file exists.
pub fn resolved(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path)
        .or_else(|_| std::path::absolute(&path))
        .unwrap_or(path)
}

/// A single-line text field with a cursor, indexed by character.
#[derive(Clone, Debug, Default)]
pub struct LineInput {
    chars: Vec<char>,
    cursor: usize,
}

impl LineInput {
    pub fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        Self { cursor: chars.len(), chars }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn chars(&self) -> &[char] {
        &self.chars
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    /// Cursor movement and deletion keys; Ctrl+U clears the field.
    pub fn edit(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Home => self.home(),
            KeyCode::End => self.end(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => self.clear(),
            _ => {}
        }
    }

    fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.chars.len();
    }

    fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use memory_manager::sample;
    use std::fs;

    /// A folder holding the sample images the tests open, as files:
    /// `original-data-out.bin` and `edited-9999-data-out.bin` (the same with
    /// MemoBank A page 49 slot 1 set to 9999 kHz), next to a `README.md` that
    /// is not an image and an empty folder. Built once per test run; tests
    /// copy from it and never write to it.
    pub fn fixture(name: &str) -> PathBuf {
        static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        let dir = DIR.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("memory-manager-fixtures-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("original-data-out.bin"), sample::radio_like()).unwrap();
            fs::write(dir.join("edited-9999-data-out.bin"), sample::edited_9999()).unwrap();
            fs::write(dir.join("README.md"), b"notes").unwrap();
            fs::create_dir_all(dir.join("images")).unwrap();
            resolved(dir)
        });
        let path = dir.join(name);
        assert!(path.is_file(), "no such fixture: {name}");
        path
    }

    pub fn app_with_original() -> App {
        let path = fixture("original-data-out.bin");
        let image = EepromImage::load(&path).unwrap();
        App::new(Some(Document { path: Some(path), image }))
    }

    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> Self {
            // A counter keeps names unique: tests run in parallel, and the
            // clock alone can give two of them the same timestamp.
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("memory-manager-app-{tag}-{}-{nanos}-{unique}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The sample with presets stored on FM, MW, LW and AIR (FM page 1 slot
    /// 3 locked) and SW page 47 slot 1 at 14200 kHz USB, as an untitled image.
    pub fn app_with_markers() -> App {
        let image = EepromImage::from_bytes(&sample::with_presets_on_every_band()).unwrap();
        App::new(Some(Document { path: None, image }))
    }

    fn status(app: &App) -> &str {
        app.status.as_ref().map_or("", |status| status.text.as_str())
    }

    /// From the start (SW): FM page 1, with the given slot selected.
    fn go_to_fm_slot(app: &mut App, slot: u8) {
        type_text(app, "bb");
        assert_eq!(app.band, Band::Fm);
        press(app, KeyCode::Right);
        for _ in 1..slot {
            press(app, KeyCode::Down);
        }
    }

    fn fm(page: u8, slot: u8) -> PresetRef {
        PresetRef::new(Region::BankA, Band::Fm, Page::new(page).unwrap(), Slot::new(slot).unwrap()).unwrap()
    }

    #[test]
    fn bands_cycle_and_each_remembers_its_page() {
        let mut app = app_with_markers();
        assert_eq!(app.band, Band::Sw);
        let seen: Vec<Band> = (0..5)
            .map(|_| {
                press(&mut app, KeyCode::Char('b'));
                app.band
            })
            .collect();
        assert_eq!(seen, [Band::Air, Band::Fm, Band::Mw, Band::Lw, Band::Sw]);
        press(&mut app, KeyCode::Char('B'));
        assert_eq!(app.band, Band::Lw);
        press(&mut app, KeyCode::Char('b'));

        // SW page 31, then AIR, whose five pages stop the selection at 5.
        for _ in 0..3 {
            press(&mut app, KeyCode::PageDown);
        }
        assert_eq!(app.page.number(), 31);
        press(&mut app, KeyCode::Char('b'));
        assert_eq!((app.band, app.page.number()), (Band::Air, 1));
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.page.number(), 5);
        press(&mut app, KeyCode::End);
        assert_eq!(app.page.number(), 5);
        press(&mut app, KeyCode::Char('B'));
        assert_eq!((app.band, app.page.number()), (Band::Sw, 31));
        press(&mut app, KeyCode::Char('b'));
        assert_eq!((app.band, app.page.number()), (Band::Air, 5));
        assert_eq!(app.at().address(), 0x0E60 + 2 * 36);
    }

    #[test]
    fn an_fm_frequency_is_typed_in_mhz_and_keeps_its_lock() {
        let mut app = app_with_markers();
        go_to_fm_slot(&mut app, 3);
        assert_eq!(image(&app).frequency(fm(1, 3)), Some(91_300));

        // Enter starts from the current value, written the way it is typed.
        press(&mut app, KeyCode::Enter);
        assert_eq!(prompt(&app).input.text(), "91.30");
        press(&mut app, KeyCode::Esc);

        // A digit starts a fresh value. Letters and a second point are refused.
        type_text(&mut app, "99");
        press(&mut app, KeyCode::Char('x'));
        assert!(prompt(&app).error.as_deref().unwrap().contains("MHz from 64.00–108.00 MHz"));
        type_text(&mut app, ".9");
        press(&mut app, KeyCode::Char('.'));
        assert!(prompt(&app).error.is_some());
        type_text(&mut app, "0");
        assert_eq!(prompt(&app).input.text(), "99.90");
        type_text(&mut app, "5");
        assert_eq!(prompt(&app).error.as_deref(), Some("FM takes 2 decimals."));
        assert_eq!(prompt(&app).input.text(), "99.90");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(status(&app), "FM page 1 slot 3: 91.30 MHz → 99.90 MHz");
        assert_eq!(image(&app).frequency(fm(1, 3)), Some(99_900));
        assert!(image(&app).is_locked(fm(1, 3)), "the lock bit survives the edit");
        assert_eq!(&image(&app).as_bytes()[0x0764..0x0766], &(0x8000u16 | 9990).to_le_bytes());

        // Off the 50 kHz step, or out of band: the prompt stays and nothing is written.
        type_text(&mut app, "91.33");
        press(&mut app, KeyCode::Enter);
        assert_eq!(prompt(&app).error.as_deref(), Some("FM frequencies go in steps of 50 kHz."));
        ctrl_u(&mut app);
        type_text(&mut app, "108.5");
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            prompt(&app).error.as_deref(),
            Some("108.5 MHz is outside the FM range 64.00–108.00 MHz.")
        );
        press(&mut app, KeyCode::Esc);
        assert_eq!(image(&app).frequency(fm(1, 3)), Some(99_900));

        // u puts the saved bytes back.
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(status(&app), "Slot 3 reverted to 91.30 MHz.");
        assert!(!image(&app).is_modified());
    }

    #[test]
    fn other_bands_take_their_own_units() {
        let mut app = app_with_markers();
        press(&mut app, KeyCode::Char('b')); // AIR
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Down); // slot 2: 137.000 MHz
        type_text(&mut app, "127.5");
        press(&mut app, KeyCode::Enter);
        assert_eq!(status(&app), "AIR page 1 slot 2: 137.000 MHz → 127.500 MHz");
        assert_eq!(&image(&app).as_bytes()[0x0E62..0x0E64], &17_500u16.to_le_bytes());

        type_text(&mut app, "bb"); // FM, then MW; the slot selection carries over
        assert_eq!(app.band, Band::Mw);
        press(&mut app, KeyCode::Home);
        type_text(&mut app, "1");
        press(&mut app, KeyCode::Char('.'));
        assert_eq!(prompt(&app).error.as_deref(), Some("Digits only: whole kHz from 520–1710 kHz."));
        type_text(&mut app, "179");
        press(&mut app, KeyCode::Enter);
        assert_eq!(status(&app), "MW page 1 slot 1: 522 kHz → 1179 kHz");
        assert_eq!(&image(&app).as_bytes()[0x07C0..0x07C2], &1179u16.to_le_bytes());
        assert_eq!(image(&app).modified_byte_count(), 4);
    }

    #[test]
    fn capital_l_locks_and_unlocks_a_preset() {
        let mut app = app_with_markers();
        press(&mut app, KeyCode::Char('L'));
        assert_eq!(status(&app), "Select a slot (→) to lock or unlock it.");

        go_to_fm_slot(&mut app, 3);
        press(&mut app, KeyCode::Char('L'));
        assert_eq!(status(&app), "FM page 1 slot 3 unlocked.");
        assert_eq!(&image(&app).as_bytes()[0x0764..0x0766], &9130u16.to_le_bytes());
        press(&mut app, KeyCode::Char('L'));
        assert_eq!(status(&app), "FM page 1 slot 3 locked.");
        assert!(!image(&app).is_modified());

        press(&mut app, KeyCode::Down); // slot 4 is empty
        press(&mut app, KeyCode::Char('L'));
        assert_eq!(status(&app), "An empty position cannot be locked.");
        assert_eq!(app.status.as_ref().unwrap().kind, StatusKind::Error);
        assert!(!image(&app).is_modified());

        // On SW the lock is the same bit.
        let mut app = app_with_original();
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Char('L'));
        assert_eq!(status(&app), "Page 1 slot 1 locked.");
        assert_eq!(image(&app).modified_byte_count(), 1);
        assert_eq!(image(&app).as_bytes()[0x0841] & 0x80, 0x80);
    }

    #[test]
    fn presets_outside_sw_carry_their_own_names() {
        let mut app = app_with_markers();
        type_text(&mut app, "bb");
        // FM pages have no names: Enter opens the page, r asks for a preset.
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.focus, Focus::Slots);
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Char('r'));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.focus, Focus::Slots);
        assert_eq!(status(&app), "On FM, each preset has its own name: pick one and press r.");

        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('r'));
        let name = NameRef::Preset(fm(1, 3));
        assert_eq!(prompt(&app).kind, PromptKind::Name { name });
        assert_eq!(prompt(&app).input.text(), "", "the radio left the name as zeros");
        type_text(&mut app, "TEST 1");
        press(&mut app, KeyCode::Enter);
        assert_eq!(status(&app), "FM page 1 slot 3 named \"TEST 1\".");
        assert_eq!(&image(&app).as_bytes()[0x0014..0x001E], b"TEST 1    ");
        assert_eq!(image(&app).modified_byte_count(), 10);

        // Unchanged names keep their bytes; u restores the saved ones.
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(prompt(&app).input.text(), "TEST 1");
        press(&mut app, KeyCode::Enter);
        assert_eq!(status(&app), "Name unchanged; existing bytes kept.");
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(image(&app).name(name), [0; NAME_LEN]);
        assert!(!image(&app).is_modified());

        // An empty position has no name to give.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('r'));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(status(&app), "Store a frequency first; an empty position has no name.");

        // On SW, r still renames the page from either pane.
        type_text(&mut app, "BB");
        assert_eq!(app.band, Band::Sw);
        press(&mut app, KeyCode::Char('r'));
        assert!(matches!(prompt(&app).kind, PromptKind::Name { name: NameRef::SwPage(Region::BankA, _) }));
    }

    #[test]
    fn clearing_a_preset_outside_sw_blanks_its_name() {
        let mut app = app_with_markers();
        press(&mut app, KeyCode::Char('b')); // AIR
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(status(&app), "AIR page 1 slot 3 cleared (was 121.500 MHz).");
        assert_eq!(image(&app).frequency(app.at()), None);
        assert_eq!(&image(&app).as_bytes()[0x0594..0x059E], b"          ");
        assert_eq!(image(&app).modified_byte_count(), 2 + NAME_LEN);

        // On SW the mode byte goes with the preset.
        let mut app = app_with_markers();
        for _ in 0..46 {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(app.page.number(), 47);
        press(&mut app, KeyCode::Right);
        assert_eq!(image(&app).mode(app.at()), Some(0xD8));
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(status(&app), "Page 47 slot 1 cleared (was 14200 kHz).");
        assert_eq!(image(&app).mode(app.at()), Some(0));
        assert_eq!(image(&app).modified_byte_count(), 3);
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn prompt(app: &App) -> &Prompt {
        match &app.mode {
            Mode::Prompt(prompt) => prompt,
            _ => panic!("expected a prompt"),
        }
    }

    fn image(app: &App) -> &EepromImage {
        &app.doc.as_ref().unwrap().image
    }

    fn doc_path(app: &App) -> &Path {
        app.doc.as_ref().unwrap().path.as_deref().expect("image has a file")
    }

    fn save_dialog(app: &App) -> &SaveDialog {
        match &app.mode {
            Mode::Save(dialog) => dialog,
            _ => panic!("expected the save dialog"),
        }
    }

    fn ctrl_u(app: &mut App) {
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    }

    /// A temp folder holding `work.bin` (a copy of the original export), opened in the app.
    fn app_with_work_copy(tag: &str) -> (TempDir, App) {
        let dir = TempDir::new(tag);
        let work = dir.0.join("work.bin");
        fs::copy(fixture("original-data-out.bin"), &work).unwrap();
        let image = EepromImage::load(&work).unwrap();
        let app = App::new(Some(Document { path: Some(resolved(work)), image }));
        (dir, app)
    }

    fn edit_page_49_slot_1_to_9999(app: &mut App) {
        press(app, KeyCode::End);
        press(app, KeyCode::Right);
        type_text(app, "9999");
        press(app, KeyCode::Enter);
        press(app, KeyCode::Left);
        press(app, KeyCode::Home);
    }

    #[test]
    fn keyboard_edit_reproduces_the_edited_fixture() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::End); // page 49
        press(&mut app, KeyCode::Right); // slots, slot 1
        type_text(&mut app, "9999");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        let expected = fs::read(fixture("edited-9999-data-out.bin")).unwrap();
        assert_eq!(image(&app).as_bytes(), &expected[..]);
        assert!(app.is_dirty());
    }

    #[test]
    fn enter_on_a_slot_starts_from_its_current_value() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);
        assert_eq!(prompt(&app).input.text(), "9410");
    }

    #[test]
    fn invalid_frequency_keeps_the_prompt_and_the_image() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Right);
        type_text(&mut app, "1000");
        press(&mut app, KeyCode::Enter);
        assert!(prompt(&app).error.as_deref().unwrap().contains("outside the SW range"));
        type_text(&mut app, "x");
        assert!(prompt(&app).error.as_deref().unwrap().starts_with("Digits only"));
        assert_eq!(prompt(&app).input.text(), "1000");
        type_text(&mut app, "00");
        assert!(prompt(&app).error.as_deref().unwrap().starts_with("At most 5 digits"));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(!app.is_dirty());
    }

    #[test]
    fn empty_frequency_is_refused() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        press(&mut app, KeyCode::Enter);
        assert!(prompt(&app).error.is_some());
        assert!(!app.is_dirty());
    }

    #[test]
    fn rename_prompt_filters_input_and_preserves_unchanged_names() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Enter); // rename page 1, stored as BBC@1@@@@@
        assert_eq!(prompt(&app).input.text(), "BBC 1");
        press(&mut app, KeyCode::Enter); // submitted unchanged
        assert!(matches!(app.mode, Mode::Normal));
        assert!(!app.is_dirty());

        // A rename writes 0x20 blanks: the 0x40 blanks and the digit change.
        press(&mut app, KeyCode::Char('r'));
        press(&mut app, KeyCode::Backspace);
        type_text(&mut app, "3");
        press(&mut app, KeyCode::Enter);
        assert_eq!(&image(&app).page_name(Region::BankA, Page::new(1).unwrap()), b"BBC 3     ");
        assert_eq!(image(&app).modified_byte_count(), 7);
        press(&mut app, KeyCode::Char('u'));
        assert!(!app.is_dirty());

        press(&mut app, KeyCode::Char('r'));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_text(&mut app, "BBC.");
        assert!(prompt(&app).error.is_some());
        type_text(&mut app, " ONE ONE ONE");
        assert_eq!(prompt(&app).input.text(), "BBC ONE ON");
        assert!(prompt(&app).error.as_deref().unwrap().contains("at most 10"));
        press(&mut app, KeyCode::Enter);
        assert_eq!(&image(&app).page_name(Region::BankA, Page::new(1).unwrap()), b"BBC ONE ON");
    }

    #[test]
    fn edited_name_with_unsupported_bytes_is_refused_on_submit() {
        let mut app = app_with_original();
        for _ in 0..4 {
            press(&mut app, KeyCode::Down); // page 5, RAD.ROPA
        }
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(prompt(&app).input.text(), "RAD.ROPA");
        press(&mut app, KeyCode::Backspace); // "RAD.ROP" still contains '.'
        press(&mut app, KeyCode::Enter);
        assert!(prompt(&app).error.as_deref().unwrap().contains("'.'"));
        assert!(!app.is_dirty());
    }

    #[test]
    fn clear_and_revert_slot() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Char('x'));
        assert!(!app.is_dirty(), "clear needs the slot pane");
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Char('x'));
        assert!(app.is_dirty());
        assert_eq!(image(&app).frequency_khz(app.region, app.page, app.slot), None);
        press(&mut app, KeyCode::Char('u'));
        assert!(!app.is_dirty());
    }

    #[test]
    fn navigation_wraps_regions_and_clamps_pages_and_slots() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.region, Region::BankC);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.region, Region::BankA);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.page.number(), 1);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.page.number(), 11);
        for _ in 0..10 {
            press(&mut app, KeyCode::PageDown);
        }
        assert_eq!(app.page.number(), 49);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::End);
        assert_eq!(app.slot.number(), 9);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.slot.number(), 9);
        press(&mut app, KeyCode::PageUp);
        assert_eq!((app.page.number(), app.slot.number()), (48, 9));
    }

    #[test]
    fn quitting_clean_exits_immediately() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Char('q'));
        assert!(app.should_quit);
    }

    #[test]
    fn quitting_dirty_asks_first() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('q'));
        assert!(matches!(app.mode, Mode::Confirm(Confirm::Discard { next: AfterDiscard::Quit })));
        press(&mut app, KeyCode::Char('z')); // unrelated keys keep asking
        assert!(matches!(app.mode, Mode::Confirm(_)));
        press(&mut app, KeyCode::Esc);
        assert!(!app.should_quit);
        assert!(app.is_dirty());

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        press(&mut app, KeyCode::Char('d'));
        assert!(app.should_quit);
    }

    #[test]
    fn quit_with_save_first_writes_the_file() {
        let dir = TempDir::new("quitsave");
        let path = dir.0.join("work.bin");
        fs::copy(fixture("original-data-out.bin"), &path).unwrap();
        let image = EepromImage::load(&path).unwrap();
        let mut app = App::new(Some(Document { path: Some(path.clone()), image }));
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Right);
        type_text(&mut app, "9999");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('s'));
        assert!(app.should_quit);
        assert_eq!(fs::read(&path).unwrap(), fs::read(fixture("edited-9999-data-out.bin")).unwrap());
    }

    #[test]
    fn opening_another_file_while_dirty_asks_first() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Char('/'));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let edited = fixture("edited-9999-data-out.bin");
        type_text(&mut app, &edited.display().to_string());
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Confirm(Confirm::Discard { next: AfterDiscard::Open(_) })));
        press(&mut app, KeyCode::Char('n'));
        assert!(app.is_dirty());
        assert!(doc_path(&app).ends_with("original-data-out.bin"));

        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Char('/'));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_text(&mut app, &edited.display().to_string());
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('d'));
        assert!(!app.is_dirty());
        assert!(doc_path(&app).ends_with("edited-9999-data-out.bin"));
        assert_eq!(image(&app).frequency_khz(Region::BankA, Page::new(49).unwrap(), Slot::new(1).unwrap()), Some(9999));
    }

    #[test]
    fn opening_an_invalid_file_reports_and_keeps_the_current_image() {
        let dir = TempDir::new("badopen");
        let bad = dir.0.join("short.bin");
        fs::write(&bad, [0u8; 100]).unwrap();
        let mut app = app_with_original();
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Char('/'));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_text(&mut app, &bad.display().to_string());
        press(&mut app, KeyCode::Enter);
        let error = prompt(&app).error.clone().unwrap();
        assert!(error.contains("expected exactly 12032 bytes"), "{error}");
        assert!(error.contains("found 100"), "{error}");
        press(&mut app, KeyCode::Esc);
        assert!(doc_path(&app).ends_with("original-data-out.bin"));
    }

    #[test]
    fn save_on_a_file_that_exists_writes_without_asking() {
        let (dir, mut app) = app_with_work_copy("save");
        edit_page_49_slot_1_to_9999(&mut app);
        press(&mut app, KeyCode::Char('s'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(!app.is_dirty());
        let edited = fs::read(fixture("edited-9999-data-out.bin")).unwrap();
        assert_eq!(fs::read(dir.0.join("work.bin")).unwrap(), edited);
        assert!(app.status.as_ref().unwrap().text.starts_with("Saved"));
    }

    #[test]
    fn save_as_shows_the_folder_and_name_and_writes_a_new_file() {
        let (dir, mut app) = app_with_work_copy("saveas");
        edit_page_49_slot_1_to_9999(&mut app);
        press(&mut app, KeyCode::Char('S'));
        let dialog = save_dialog(&app);
        assert!(dialog.save_as);
        assert_eq!(dialog.browser.dir, resolved(dir.0.clone()));
        assert_eq!(dialog.name.text(), "work.bin");
        assert_eq!(dialog.focus, DialogFocus::Name);

        ctrl_u(&mut app);
        type_text(&mut app, "copy"); // .bin is added
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(!app.is_dirty());
        assert!(doc_path(&app).ends_with("copy.bin"));
        let edited = fs::read(fixture("edited-9999-data-out.bin")).unwrap();
        assert_eq!(fs::read(dir.0.join("copy.bin")).unwrap(), edited);
        // The file the image came from is untouched.
        assert_eq!(fs::read(dir.0.join("work.bin")).unwrap(), fs::read(fixture("original-data-out.bin")).unwrap());
        // Save now goes straight to the new file.
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('s'));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(fs::read(dir.0.join("copy.bin")).unwrap()[0x0840..0x0842], [0, 0]);
    }

    #[test]
    fn save_as_onto_an_existing_file_warns_first() {
        let (dir, mut app) = app_with_work_copy("overwrite");
        let other = dir.0.join("other.bin");
        fs::copy(fixture("original-data-out.bin"), &other).unwrap();
        edit_page_49_slot_1_to_9999(&mut app);

        // Typing the name of an existing file asks before replacing it; cancelling returns to the dialog.
        press(&mut app, KeyCode::Char('S'));
        ctrl_u(&mut app);
        type_text(&mut app, "other.bin");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(&app.mode, Mode::Confirm(Confirm::Overwrite { path, .. }) if path.ends_with("other.bin")));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(save_dialog(&app).name.text(), "other.bin");
        assert_eq!(fs::read(&other).unwrap(), fs::read(fixture("original-data-out.bin")).unwrap());

        // Choosing an existing file in the list asks too; y replaces it.
        press(&mut app, KeyCode::Tab);
        assert_eq!(save_dialog(&app).focus, DialogFocus::List);
        assert_eq!(save_dialog(&app).browser.selected_entry().unwrap().name, "work.bin");
        press(&mut app, KeyCode::Up);
        assert_eq!(save_dialog(&app).browser.selected_entry().unwrap().name, "other.bin");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Confirm(Confirm::Overwrite { .. })));
        press(&mut app, KeyCode::Char('y'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(doc_path(&app).ends_with("other.bin"));
        assert_eq!(fs::read(&other).unwrap(), fs::read(fixture("edited-9999-data-out.bin")).unwrap());

        // Save As onto the image's own file also asks.
        press(&mut app, KeyCode::Char('S'));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Confirm(Confirm::Overwrite { .. })));
    }

    #[test]
    fn save_dialog_moves_through_folders_and_checks_the_name() {
        let (dir, mut app) = app_with_work_copy("dialog");
        fs::create_dir(dir.0.join("sub")).unwrap();
        press(&mut app, KeyCode::Char('S'));

        ctrl_u(&mut app);
        press(&mut app, KeyCode::Enter);
        assert_eq!(save_dialog(&app).error.as_deref(), Some("Enter a file name."));

        type_text(&mut app, "missing/x.bin");
        press(&mut app, KeyCode::Enter);
        assert!(save_dialog(&app).error.as_deref().unwrap().contains("does not exist"));

        // Typing a folder name enters it.
        ctrl_u(&mut app);
        type_text(&mut app, "sub");
        press(&mut app, KeyCode::Enter);
        assert_eq!(save_dialog(&app).browser.dir, resolved(dir.0.clone()).join("sub"));
        assert_eq!(save_dialog(&app).name.text(), "");

        // In the list, Enter on ".." goes up; list keys do not type into the name.
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Enter);
        assert_eq!(save_dialog(&app).browser.dir, resolved(dir.0.clone()));
        assert_eq!(save_dialog(&app).name.text(), "");

        press(&mut app, KeyCode::BackTab);
        type_text(&mut app, "sub/inside");
        press(&mut app, KeyCode::Enter);
        assert!(doc_path(&app).ends_with("sub/inside.bin"));
    }

    #[test]
    fn save_dialog_arrows_and_enter_move_between_folders() {
        // Reported 2026-11-12: arrows moved the highlight, but Enter saved and
        // ←/Backspace edited the name instead of changing folder.
        let (dir, mut app) = app_with_work_copy("savenav");
        let home = resolved(dir.0.clone());
        let sub = home.join("sub");
        fs::create_dir(&sub).unwrap();
        press(&mut app, KeyCode::Char('S'));
        assert_eq!(save_dialog(&app).name.text(), "work.bin");
        assert_eq!(save_dialog(&app).focus, DialogFocus::Name);

        // ↑ highlights the folder and hands control to the list; Enter opens it.
        press(&mut app, KeyCode::Up);
        assert_eq!(save_dialog(&app).focus, DialogFocus::List);
        assert_eq!(save_dialog(&app).browser.selected_entry().unwrap().name, "sub");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Save(_)), "Enter on a folder must not save");
        assert_eq!(save_dialog(&app).browser.dir, sub);
        assert_eq!(save_dialog(&app).name.text(), "work.bin", "the typed name is kept");
        assert_eq!(fs::read_dir(&sub).unwrap().count(), 0);

        // ← goes up with the folder highlighted; → goes back in; Backspace goes up.
        press(&mut app, KeyCode::Left);
        assert_eq!(save_dialog(&app).browser.dir, home);
        assert_eq!(save_dialog(&app).browser.selected_entry().unwrap().name, "sub");
        press(&mut app, KeyCode::Right);
        assert_eq!(save_dialog(&app).browser.dir, sub);
        press(&mut app, KeyCode::Backspace);
        assert_eq!(save_dialog(&app).browser.dir, home);
        assert_eq!(save_dialog(&app).name.text(), "work.bin");

        // ".." with Enter goes up; Enter on the folder we came from goes back.
        press(&mut app, KeyCode::Up);
        assert_eq!(save_dialog(&app).browser.selected_entry().unwrap().name, "..");
        press(&mut app, KeyCode::Enter);
        assert_eq!(save_dialog(&app).browser.dir, home.parent().unwrap());
        press(&mut app, KeyCode::Enter);
        assert_eq!(save_dialog(&app).browser.dir, home);

        // → on a file does nothing; typing returns to the name and Enter saves here.
        press(&mut app, KeyCode::Down);
        assert_eq!(save_dialog(&app).browser.selected_entry().unwrap().name, "work.bin");
        press(&mut app, KeyCode::Right);
        assert!(matches!(app.mode, Mode::Save(_)));
        ctrl_u(&mut app);
        assert_eq!(save_dialog(&app).focus, DialogFocus::Name);
        type_text(&mut app, "copy");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(home.join("copy.bin").exists());
    }

    #[test]
    fn save_dialog_enter_on_a_highlighted_file_asks_before_replacing() {
        let (_dir, mut app) = app_with_work_copy("savepick");
        press(&mut app, KeyCode::Char('S'));
        ctrl_u(&mut app);
        type_text(&mut app, "new-name");
        press(&mut app, KeyCode::Down); // highlight stays on work.bin, the only file
        press(&mut app, KeyCode::Enter);
        assert!(matches!(&app.mode, Mode::Confirm(Confirm::Overwrite { path, .. }) if path.ends_with("work.bin")));
    }

    #[test]
    fn escape_cancels_save_as_and_keeps_changes() {
        let (dir, mut app) = app_with_work_copy("cancel");
        edit_page_49_slot_1_to_9999(&mut app);
        press(&mut app, KeyCode::Char('S'));
        ctrl_u(&mut app);
        type_text(&mut app, "never");
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.is_dirty());
        assert!(!dir.0.join("never.bin").exists());
    }

    #[test]
    fn new_image_is_untitled_and_save_asks_for_a_name() {
        let (dir, mut app) = app_with_work_copy("new");
        // New clears a copy of the open image: its presets and names go,
        // every other byte (settings, product string) stays.
        let cleared = image(&app).cleared();
        press(&mut app, KeyCode::Char('n'));
        let doc = app.doc.as_ref().unwrap();
        assert_eq!(doc.path, None);
        assert_eq!(doc.display_name(), "Untitled");
        assert_eq!(doc.image.as_bytes(), cleared.as_bytes());
        assert!(doc.image.has_product_string());
        assert_eq!(doc.image.frequency_khz(Region::BankA, Page::new(49).unwrap(), Slot::new(1).unwrap()), None);
        assert_eq!(
            status(&app),
            "New image: all presets and names cleared, on every band. Everything else is as in work.bin."
        );
        assert!(!app.is_dirty());

        // An unedited new image still has no file, so Save asks for a name, starting in the last folder.
        press(&mut app, KeyCode::Char('s'));
        let dialog = save_dialog(&app);
        assert!(!dialog.save_as);
        assert_eq!(dialog.name.text(), "");
        assert_eq!(dialog.browser.dir, resolved(dir.0.clone()));
        type_text(&mut app, "fresh");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(doc_path(&app).ends_with("fresh.bin"));
        assert_eq!(fs::read(dir.0.join("fresh.bin")).unwrap(), cleared.as_bytes());
    }

    #[test]
    fn new_needs_an_image_of_the_users_own_radio() {
        // With nothing open and no radio backup there is no radio to take the
        // settings and unknown bytes from.
        let mut app = App::new(None);
        press(&mut app, KeyCode::Char('n'));
        assert!(app.doc.is_none());
        assert_eq!(
            status(&app),
            "New starts from your radio's own image: read the radio (R) or open an image first."
        );
    }

    #[test]
    fn an_image_without_the_product_string_is_never_written() {
        // The demo image is marked so; so is any file that is not a radio's export.
        let mut app = App::new(Some(Document { path: None, image: sample::demo() }));
        assert!(!image(&app).has_product_string());
        press(&mut app, KeyCode::Char('W'));
        assert!(matches!(app.mode, Mode::Normal), "no confirmation, no transfer");
        assert!(status(&app).starts_with("Not written: this image lacks the radio's product string"));
        assert!(app.take_radio_requests().is_empty());

        let dir = TempDir::new("noproduct");
        let mut bytes = sample::radio_like();
        bytes[0x0F50] = b'X';
        let path = dir.0.join("odd.bin");
        fs::write(&path, &bytes).unwrap();
        let mut app = App::new(None);
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Char('/'));
        ctrl_u(&mut app);
        type_text(&mut app, &path.display().to_string());
        press(&mut app, KeyCode::Enter);
        assert!(app.doc.is_some());
        assert_eq!(status(&app), "This image lacks the radio's product string, so it cannot be written to a radio.");
        press(&mut app, KeyCode::Char('W'));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn new_while_dirty_asks_first() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('n'));
        assert!(matches!(app.mode, Mode::Confirm(Confirm::Discard { next: AfterDiscard::New })));
        press(&mut app, KeyCode::Esc);
        assert!(app.doc.as_ref().unwrap().path.is_some());
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.doc.as_ref().unwrap().path, None);
        assert!(!app.is_dirty());
    }

    #[test]
    fn quitting_an_edited_new_image_saves_through_the_dialog_then_quits() {
        let (dir, mut app) = app_with_work_copy("newquit");
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Right);
        type_text(&mut app, "6085");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('s')); // save first → no file yet → dialog
        assert!(!save_dialog(&app).save_as);
        press(&mut app, KeyCode::Esc); // cancelling the dialog cancels the quit
        assert!(!app.should_quit);
        assert!(app.is_dirty());

        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('s'));
        type_text(&mut app, "kept");
        press(&mut app, KeyCode::Enter);
        assert!(app.should_quit);
        let saved = EepromImage::load(&dir.0.join("kept.bin")).unwrap();
        let (r, p, s) = (Region::BankA, Page::new(1).unwrap(), Slot::new(1).unwrap());
        assert_eq!(saved.frequency_khz(r, p, s), Some(6085));
    }

    #[test]
    fn an_unedited_new_image_quits_without_asking() {
        let mut app = App::new(None);
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('q'));
        assert!(app.should_quit);
    }

    #[test]
    fn failed_save_reports_and_stays_dirty() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Char('x'));
        app.doc.as_mut().unwrap().path = Some(PathBuf::from("/nonexistent-dir/for/memory-manager/out.bin"));
        press(&mut app, KeyCode::Char('s'));
        let status = app.status.as_ref().unwrap();
        assert_eq!(status.kind, StatusKind::Error);
        assert!(status.text.starts_with("Save failed"));
        assert!(app.is_dirty());
        // Saving first on quit fails too, so the app keeps running.
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('s'));
        assert!(!app.should_quit);
    }

    fn browser(app: &App) -> &Browser {
        match &app.mode {
            Mode::Browse(browser) => browser,
            _ => panic!("expected the file browser"),
        }
    }

    #[test]
    fn open_shows_the_current_folder_with_the_file_selected() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Char('o'));
        let b = browser(&app);
        assert_eq!(b.dir, fixture("original-data-out.bin").parent().unwrap());
        assert_eq!(b.selected_entry().unwrap().name, "original-data-out.bin");
        let names: Vec<_> = b.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"images"), "folders are listed");
        assert!(names.contains(&"edited-9999-data-out.bin"));
        assert!(!names.contains(&"README.md"), "non-image files are hidden by default");
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn browsing_to_another_image_while_dirty_asks_first() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Up); // edited-9999 sorts just before original
        assert_eq!(browser(&app).selected_entry().unwrap().name, "edited-9999-data-out.bin");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Confirm(Confirm::Discard { next: AfterDiscard::Open(_) })));
        press(&mut app, KeyCode::Char('d'));
        assert!(!app.is_dirty());
        assert_eq!(doc_path(&app), fixture("edited-9999-data-out.bin"));
    }

    #[test]
    fn browser_enters_folders_and_opens_clean_images_directly() {
        let dir = TempDir::new("browse");
        fs::create_dir(dir.0.join("inner")).unwrap();
        fs::copy(fixture("edited-9999-data-out.bin"), dir.0.join("inner").join("x.bin")).unwrap();
        let mut app = App::new(None);
        app.mode = Mode::Browse(Browser::new(dir.0.clone(), None));
        assert_eq!(browser(&app).selected_entry().unwrap().name, "..");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter); // into "inner", x.bin preselected
        assert_eq!(browser(&app).selected_entry().unwrap().name, "x.bin");
        press(&mut app, KeyCode::Left);
        assert_eq!(browser(&app).selected_entry().unwrap().name, "inner");
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(doc_path(&app).ends_with("inner/x.bin"));
        assert!(app.status.as_ref().unwrap().text.starts_with("Opened"));
    }

    #[test]
    fn browser_reports_wrong_size_files_and_stays_open() {
        let dir = TempDir::new("browsebad");
        let work = dir.0.join("work.bin");
        fs::copy(fixture("original-data-out.bin"), &work).unwrap();
        fs::write(dir.0.join("short.bin"), [0u8; 100]).unwrap();
        let image = EepromImage::load(&work).unwrap();
        let mut app = App::new(Some(Document { path: Some(work), image }));
        press(&mut app, KeyCode::Char('o'));
        assert_eq!(browser(&app).filtered_files, 1);
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(browser(&app).selected_entry().unwrap().name, "work.bin");
        press(&mut app, KeyCode::Up);
        assert_eq!(browser(&app).selected_entry().unwrap().name, "short.bin");
        press(&mut app, KeyCode::Enter);
        let error = browser(&app).error.clone().unwrap();
        assert!(error.contains("found 100"), "{error}");
        press(&mut app, KeyCode::Esc);
        assert!(doc_path(&app).ends_with("work.bin"));
    }

    #[test]
    fn slash_in_the_browser_types_a_path_in_that_folder() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Char('/'));
        let dir = fixture("original-data-out.bin").parent().unwrap().display().to_string();
        assert_eq!(prompt(&app).input.text(), format!("{dir}/"));
        type_text(&mut app, "edited-9999-data-out.bin");
        press(&mut app, KeyCode::Enter);
        assert!(doc_path(&app).ends_with("edited-9999-data-out.bin"));
    }

    #[test]
    fn without_an_image_only_file_commands_work() {
        let mut app = App::new(None);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Char('S'));
        assert!(matches!(app.mode, Mode::Normal));
        press(&mut app, KeyCode::Char('o'));
        assert!(matches!(app.mode, Mode::Browse(_)));
        press(&mut app, KeyCode::Char('/'));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        press(&mut app, KeyCode::Enter);
        assert!(prompt(&app).error.is_some());
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.should_quit);
    }

    #[test]
    fn line_input_edits_at_the_cursor() {
        let mut input = LineInput::new("abc");
        input.left();
        input.insert('X');
        assert_eq!(input.text(), "abXc");
        input.home();
        input.delete();
        input.end();
        input.backspace();
        assert_eq!(input.text(), "bX");
        input.right();
        assert_eq!(input.cursor(), 2);
        input.clear();
        input.backspace();
        input.left();
        assert_eq!((input.text().as_str(), input.cursor()), ("", 0));
    }

    // ---- Radio transfers, driven with worker events ----

    use memory_manager::radio::protocol::Stats;
    use memory_manager::radio::worker::{RadioEvent, RadioRequest, Stage};
    use std::time::Duration;

    fn transfer(app: &App) -> &Transfer {
        match &app.mode {
            Mode::Radio(transfer) => transfer,
            _ => panic!("expected a radio transfer"),
        }
    }

    fn read_stats() -> Stats {
        Stats { packets: 752, verify: 47, ..Stats::default() }
    }

    fn radio_session(app: &mut App, direction: memory_manager::radio::protocol::Direction) {
        app.on_radio_event(RadioEvent::Stage(Stage::Connecting));
        app.on_radio_event(RadioEvent::Connected { port: "/dev/cu.usbserial-0001".into(), firmware: "1.0.0".into() });
        app.on_radio_event(RadioEvent::Stage(Stage::WaitingForRadio { direction, window: Duration::from_secs(120) }));
        app.on_radio_event(RadioEvent::Stage(Stage::Transferring { direction, stats: read_stats() }));
    }

    #[test]
    fn read_radio_opens_the_image_and_asks_where_to_save_it() {
        let (dir, mut app) = app_with_work_copy("radioread");
        press(&mut app, KeyCode::Char('R'));
        assert!(matches!(transfer(&app).kind, TransferKind::Read));
        assert_eq!(app.take_radio_requests(), vec![RadioRequest::Read]);

        radio_session(&mut app, memory_manager::radio::protocol::Direction::Out);
        assert!(transfer(&app).connection.as_deref().unwrap().contains("usbserial-0001"));
        app.on_radio_event(RadioEvent::Stage(Stage::Downloading { done: 47, total: 47 }));
        let edited = fs::read(fixture("edited-9999-data-out.bin")).unwrap();
        app.on_radio_event(RadioEvent::ReadFinished { image: edited.clone(), stats: read_stats() });

        let dialog = save_dialog(&app);
        let name = dialog.name.text();
        assert!(name.starts_with("radio-20") && name.ends_with(".bin"), "{name}");
        assert_eq!(dialog.browser.dir, resolved(dir.0.clone()));
        assert_eq!(app.doc.as_ref().unwrap().path, None);
        assert_eq!(image(&app).as_bytes(), &edited[..]);
        assert!(app.is_dirty(), "a received image is unsaved until saved");
        assert!(app.radio_backup.is_some());

        // Cancelling the save keeps it open and unsaved, so quitting asks.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('q'));
        assert!(matches!(app.mode, Mode::Confirm(Confirm::Discard { next: AfterDiscard::Quit })));
        press(&mut app, KeyCode::Esc);

        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Enter);
        assert!(!app.is_dirty());
        assert_eq!(fs::read(dir.0.join(&name)).unwrap(), edited);
    }

    #[test]
    fn read_radio_with_unsaved_edits_asks_first() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('R'));
        assert!(matches!(app.mode, Mode::Confirm(Confirm::Discard { next: AfterDiscard::ReadRadio })));
        assert!(app.take_radio_requests().is_empty());
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(app.mode, Mode::Radio(_)));
        assert_eq!(app.take_radio_requests(), vec![RadioRequest::Read]);
    }

    #[test]
    fn escape_cancels_a_running_transfer_and_quit_is_refused() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Char('R'));
        app.take_radio_requests();
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.should_quit);
        assert!(app.status.as_ref().unwrap().text.contains("Esc to cancel"));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.take_radio_requests(), vec![RadioRequest::Cancel]);
        assert!(transfer(&app).cancelling);
        app.on_radio_event(RadioEvent::Cancelled);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status.as_ref().unwrap().text.starts_with("Transfer cancelled"));
        assert!(app.doc.as_ref().unwrap().path.is_some(), "the open image is untouched");
    }

    #[test]
    fn failures_are_shown_until_closed() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Char('R'));
        app.on_radio_event(RadioEvent::Failed { message: "No bridge found: no USB serial port.".into() });
        assert!(matches!(&transfer(&app).outcome, Some(Outcome::Failed(m)) if m.contains("No bridge")));
        press(&mut app, KeyCode::Char('v'));
        assert!(matches!(app.mode, Mode::Radio(_)), "v only applies after a write");
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(!app.is_dirty());
    }

    /// A read the radio stopped verifying after the first block, as happens
    /// when RDS radio text gets into the export (hardware run of 2026-11-14).
    fn unverified_stats() -> Stats {
        Stats { packets: 752, verify: 1, ..Stats::default() }
    }

    #[test]
    fn an_unverified_read_is_never_kept_as_backup_or_written() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Char('R'));
        assert_eq!(app.take_radio_requests(), vec![RadioRequest::Read]);
        let original = fs::read(fixture("original-data-out.bin")).unwrap();
        app.on_radio_event(RadioEvent::ReadFinished { image: original.clone(), stats: unverified_stats() });

        let shown = app.status.as_ref().unwrap();
        assert_eq!(shown.kind, StatusKind::Error);
        assert!(shown.text.contains("did not verify this read (1 of 47 blocks)"), "{}", shown.text);
        assert!(shown.text.contains("RDS radio text"), "{}", shown.text);
        assert!(shown.text.ends_with("Tune FM to an empty frequency, with no station, and read again."));
        assert!(app.unverified);
        assert!(app.radio_backup.is_none(), "an unverified read is not the radio's memory");
        // The suggested name carries the warning into the file name.
        assert!(save_dialog(&app).name.text().ends_with("-unverified.bin"), "{}", save_dialog(&app).name.text());

        // Saved or not, it is not sent to the radio in this session.
        let dir = TempDir::new("unverified");
        let path = dir.0.join("radio.bin");
        ctrl_u(&mut app);
        type_text(&mut app, &path.display().to_string());
        press(&mut app, KeyCode::Enter);
        assert!(path.exists());
        press(&mut app, KeyCode::Char('W'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(status(&app).starts_with("Not written: the radio did not verify the read this image came from."));
        assert!(app.take_radio_requests().is_empty());

        // A verified read clears the mark and becomes the backup.
        press(&mut app, KeyCode::Char('R'));
        app.on_radio_event(RadioEvent::ReadFinished { image: original, stats: read_stats() });
        assert!(!app.unverified);
        assert!(app.radio_backup.is_some());
        assert!(!save_dialog(&app).name.text().contains("unverified"));
    }

    #[test]
    fn an_unverified_backup_is_not_saved_and_nothing_is_written() {
        let (dir, mut app) = app_with_work_copy("unverifiedbackup");
        press(&mut app, KeyCode::Char('W'));
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.take_radio_requests(), vec![RadioRequest::Read]);
        let radio = fs::read(fixture("edited-9999-data-out.bin")).unwrap();
        app.on_radio_event(RadioEvent::ReadFinished { image: radio, stats: unverified_stats() });
        match &transfer(&app).outcome {
            Some(Outcome::Failed(message)) => {
                assert!(message.starts_with("No backup saved and nothing written."), "{message}");
                assert!(message.contains("Tune FM to an empty frequency"), "{message}");
            }
            _ => panic!("expected a failure"),
        }
        let backups = fs::read_dir(&dir.0).unwrap().filter(|e| {
            e.as_ref().unwrap().file_name().to_string_lossy().starts_with("radio-backup-")
        });
        assert_eq!(backups.count(), 0);
        assert!(app.radio_backup.is_none());
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.take_radio_requests().is_empty(), "no write was started");
    }

    #[test]
    fn an_unverified_read_back_can_be_repeated() {
        let (dir, mut app) = app_with_work_copy("unverifiedcheck");
        let edited = fs::read(fixture("edited-9999-data-out.bin")).unwrap();
        fs::write(dir.0.join("work.bin"), &edited).unwrap();
        let image = EepromImage::load(&dir.0.join("work.bin")).unwrap();
        app.doc = Some(Document { path: Some(resolved(dir.0.join("work.bin"))), image });
        press(&mut app, KeyCode::Char('W'));
        press(&mut app, KeyCode::Char('y'));
        app.take_radio_requests();
        app.on_radio_event(RadioEvent::WriteFinished { stats: Stats { passes: 2, bytes: 24_064, ..Stats::default() } });
        press(&mut app, KeyCode::Char('v'));
        assert_eq!(app.take_radio_requests(), vec![RadioRequest::Read]);
        app.on_radio_event(RadioEvent::ReadFinished { image: edited.clone(), stats: unverified_stats() });
        assert!(
            matches!(&transfer(&app).outcome, Some(Outcome::Failed(m)) if m.starts_with("The radio did not verify the read-back"))
        );
        assert!(app.radio_backup.is_none());
        // v tries again.
        press(&mut app, KeyCode::Char('v'));
        assert_eq!(app.take_radio_requests(), vec![RadioRequest::Read]);
        app.on_radio_event(RadioEvent::ReadFinished { image: edited, stats: read_stats() });
        assert!(matches!(&transfer(&app).outcome, Some(Outcome::Verified { diff }) if diff.is_empty()));
    }

    #[test]
    fn write_needs_a_saved_file_first() {
        let (dir, mut app) = app_with_work_copy("radiosave");
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('W'));
        assert!(matches!(app.mode, Mode::Confirm(Confirm::SaveBeforeRadio)));
        press(&mut app, KeyCode::Char('s'));
        type_text(&mut app, "for-radio");
        press(&mut app, KeyCode::Enter);
        assert!(dir.0.join("for-radio.bin").exists());
        assert!(matches!(app.mode, Mode::Confirm(Confirm::WriteRadio { .. })));
        assert!(app.take_radio_requests().is_empty(), "nothing is sent before y");

        // Unsaved edits on a file are saved in place first.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Right);
        type_text(&mut app, "6085");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('W'));
        assert!(matches!(app.mode, Mode::Confirm(Confirm::SaveBeforeRadio)));
        press(&mut app, KeyCode::Char('s'));
        assert!(!app.is_dirty());
        assert!(matches!(app.mode, Mode::Confirm(Confirm::WriteRadio { .. })));
    }

    #[test]
    fn write_confirmation_compares_then_writes_and_reads_back() {
        let (dir, mut app) = app_with_work_copy("radiowrite");
        let edited = fs::read(fixture("edited-9999-data-out.bin")).unwrap();
        fs::write(dir.0.join("work.bin"), &edited).unwrap();
        let image = EepromImage::load(&dir.0.join("work.bin")).unwrap();
        app.doc = Some(Document { path: Some(resolved(dir.0.join("work.bin"))), image });

        press(&mut app, KeyCode::Char('W'));
        match &app.mode {
            Mode::Confirm(Confirm::WriteRadio { file, reference, diff, has_backup }) => {
                assert_eq!(file, "work.bin");
                // No radio backup in this session: nothing to compare with.
                assert!(reference.is_empty());
                assert!(diff.is_empty());
                assert!(!has_backup);
            }
            _ => panic!("expected the write confirmation"),
        }
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.take_radio_requests(), vec![RadioRequest::Write(edited.clone())]);

        let direction = memory_manager::radio::protocol::Direction::In;
        app.on_radio_event(RadioEvent::Stage(Stage::Uploading { done: 47, total: 47 }));
        radio_session(&mut app, direction);
        app.on_radio_event(RadioEvent::WriteFinished { stats: Stats { passes: 2, bytes: 24_064, ..Stats::default() } });
        assert!(matches!(transfer(&app).outcome, Some(Outcome::Written { warning: None })));

        press(&mut app, KeyCode::Char('v'));
        assert!(matches!(transfer(&app).kind, TransferKind::Verify { .. }));
        assert_eq!(app.take_radio_requests(), vec![RadioRequest::Read]);
        app.on_radio_event(RadioEvent::ReadFinished { image: edited.clone(), stats: read_stats() });
        assert!(matches!(&transfer(&app).outcome, Some(Outcome::Verified { diff }) if diff.is_empty()));
        assert!(app.radio_backup.is_some());
        // Reading back does not replace the open file.
        assert!(doc_path(&app).ends_with("work.bin"));

        press(&mut app, KeyCode::Esc);
        // With a backup from this session, the next write compares against the radio.
        press(&mut app, KeyCode::Char('W'));
        match &app.mode {
            Mode::Confirm(Confirm::WriteRadio { reference, diff, has_backup, .. }) => {
                assert!(reference.starts_with("the radio backup read at"));
                assert!(diff.is_empty());
                assert!(has_backup);
            }
            _ => panic!("expected the write confirmation"),
        }
    }

    #[test]
    fn backup_first_saves_the_radio_image_next_to_the_file() {
        let (dir, mut app) = app_with_work_copy("radiobackup");
        press(&mut app, KeyCode::Char('W'));
        press(&mut app, KeyCode::Char('b'));
        assert!(matches!(transfer(&app).kind, TransferKind::Backup));
        assert_eq!(app.take_radio_requests(), vec![RadioRequest::Read]);
        let radio = fs::read(fixture("edited-9999-data-out.bin")).unwrap();
        app.on_radio_event(RadioEvent::ReadFinished { image: radio.clone(), stats: read_stats() });

        let backups: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("radio-backup-"))
            .collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert_eq!(fs::read(dir.0.join(&backups[0])).unwrap(), radio);
        assert!(app.status.as_ref().unwrap().text.starts_with("Radio backup saved"));
        match &app.mode {
            Mode::Confirm(Confirm::WriteRadio { reference, diff, has_backup, .. }) => {
                assert!(reference.starts_with("the radio backup"));
                assert_eq!(diff.presets.len(), 1, "work.bin is the original; the radio has the 9999 edit");
                assert!(has_backup);
            }
            _ => panic!("expected the write confirmation again"),
        }
        assert!(doc_path(&app).ends_with("work.bin"));
    }

    #[test]
    fn unique_backup_names_never_overwrite() {
        let dir = TempDir::new("unique");
        assert!(unique_path(&dir.0, "b").ends_with("b.bin"));
        fs::write(dir.0.join("b.bin"), b"x").unwrap();
        fs::write(dir.0.join("b-2.bin"), b"x").unwrap();
        assert!(unique_path(&dir.0, "b").ends_with("b-3.bin"));
    }

    #[test]
    fn tilde_paths_expand_to_home() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(expand_path("  ~/x.bin "), Some(home.join("x.bin")));
        assert_eq!(expand_path("~"), Some(home));
        assert_eq!(expand_path("~other/x"), Some(PathBuf::from("~other/x")));
        assert_eq!(expand_path("   "), None);
    }
}

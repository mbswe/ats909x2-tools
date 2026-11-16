//! Rendering. Reads the [`App`] state and draws one frame.

use memory_manager::image::{
    Band, ImageDiff, NAME_LEN, NameRef, Page, PresetRef, Region, SW_MAX_KHZ, SW_MIN_KHZ, Slot,
    mode_label, name_text,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
    Tabs, Wrap,
};

use crate::browser::{Browser, EntryKind, format_size};
use std::time::Instant;

use memory_manager::radio::protocol::{BLOCK_COUNT, Direction, IN_TOTAL_BYTES, PACKET_COUNT};
use memory_manager::settings::{Setting, TIMERS};

use crate::app::{
    AfterDiscard, App, Confirm, DialogFocus, Document, Focus, LineInput, Mode, Outcome, Prompt, PromptKind,
    SaveDialog, StatusKind, Transfer, TransferKind, frequency_text, preset_label,
};

const ACCENT: Color = Color::Cyan;
const MODIFIED: Color = Color::Yellow;
const ERROR: Color = Color::LightRed;

pub fn draw(frame: &mut Frame, app: &App) {
    let [header, tabs, bands, body, details, status, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(4),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app);
    match &app.doc {
        Some(doc) => {
            draw_tabs(frame, tabs, app, doc);
            draw_bands(frame, bands, app, doc);
            draw_body(frame, body, app, doc);
            draw_details(frame, details, app, doc);
        }
        None => draw_empty(frame, bands.union(body).union(details)),
    }
    draw_status(frame, status, app);
    draw_keys(frame, keys, app);

    match &app.mode {
        Mode::Normal => {}
        Mode::Prompt(prompt) => draw_prompt(frame, prompt),

        Mode::Browse(browser) => draw_file_dialog(frame, browser, None),
        Mode::Save(dialog) => draw_file_dialog(frame, &dialog.browser, Some(dialog)),
        Mode::Radio(transfer) => draw_radio(frame, transfer, app.log_path.as_deref()),
        Mode::Settings(selected) => {
            if let Some(doc) = &app.doc {
                draw_settings(frame, doc, *selected);
            }
        }
        Mode::Confirm(Confirm::WriteRadio { file, reference, diff, has_backup }) => {
            draw_write_confirm(frame, file, reference, diff, *has_backup)
        }
        Mode::Confirm(confirm) => draw_confirm(frame, app, confirm),
        Mode::Help => draw_help(frame),
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let title = Span::styled(" ATS-909X2 Memory Manager ", Style::new().fg(Color::Black).bg(ACCENT).bold());
    let mut spans = vec![title, Span::raw(" ")];
    match &app.doc {
        Some(doc) => {
            let state = if doc.image.is_modified() {
                let n = doc.image.modified_byte_count();
                Span::styled(
                    format!(" ● unsaved: {n} byte{} ", if n == 1 { "" } else { "s" }),
                    Style::new().fg(Color::Black).bg(MODIFIED).bold(),
                )
            } else if doc.path.is_none() {
                Span::styled(" new, not saved yet ", Style::new().dim())
            } else {
                Span::styled(" saved ", Style::new().fg(Color::Green))
            };
            let used = 27 + state.width() + 1;
            let room = usize::from(area.width).saturating_sub(used);
            let name = match &doc.path {
                Some(path) => path.display().to_string(),
                None => "Untitled".to_owned(),
            };
            spans.push(Span::raw(truncate_left(&name, room)));
            spans.push(Span::raw(" "));
            spans.push(state);
        }
        None => spans.push(Span::styled("no file open", Style::new().dim())),
    }
    frame.render_widget(Line::from(spans), area);
}

fn draw_tabs(frame: &mut Frame, area: Rect, app: &App, doc: &Document) {
    let titles = Region::ALL.iter().map(|&region| {
        let marker = if doc.image.is_region_modified(region) { "●" } else { "" };
        Line::from(vec![
            Span::raw(region.label()),
            Span::styled(format!(" {}", region.offset_label()), Style::new().dim()),
            Span::styled(marker, Style::new().fg(MODIFIED)),
        ])
    });
    let tabs = Tabs::new(titles)
        .select(app.region.index())
        .highlight_style(Style::new().fg(ACCENT).bold().underlined())
        .divider("│");
    frame.render_widget(tabs, area);
}

/// The wavebands, with the selected one highlighted.
fn draw_bands(frame: &mut Frame, area: Rect, app: &App, doc: &Document) {
    let titles = Band::ALL.iter().map(|&band| {
        let marker = if doc.image.is_band_modified(app.region, band) { "●" } else { "" };
        Line::from(vec![
            Span::raw(band.label()),
            Span::styled(format!(" {}", band.pages()), Style::new().dim()),
            Span::styled(marker, Style::new().fg(MODIFIED)),
        ])
    });
    let tabs = Tabs::new(titles)
        .select(app.band.index())
        .highlight_style(Style::new().fg(ACCENT).bold().underlined())
        .divider("│");
    frame.render_widget(tabs, area);
}

fn draw_body(frame: &mut Frame, area: Rect, app: &App, doc: &Document) {
    let [pages, slots] = Layout::horizontal([Constraint::Length(27), Constraint::Min(20)]).areas(area);
    draw_pages(frame, pages, app, doc);
    draw_slots(frame, slots, app, doc);
}

fn pane(title: Line<'static>, focused: bool) -> Block<'static> {
    let block = Block::bordered().title(title);
    if focused {
        block.border_type(BorderType::Thick).border_style(Style::new().fg(ACCENT))
    } else {
        block.border_style(Style::new().dim())
    }
}

fn selection_style(focused: bool) -> Style {
    if focused {
        Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    }
}

fn draw_pages(frame: &mut Frame, area: Rect, app: &App, doc: &Document) {
    let focused = app.focus == Focus::Pages;
    let (region, band) = (app.region, app.band);
    let items: Vec<ListItem> = (1..=band.pages())
        .filter_map(|number| Page::new(number).ok())
        .map(|page| {
            let modified = doc.image.is_band_page_modified(region, band, page);
            // Only SW pages have names.
            let name = match band {
                Band::Sw => name_text(&doc.image.name(NameRef::SwPage(region, page))),
                _ => format!("{:<width$}", format!("{} page", band.label()), width = NAME_LEN),
            };
            let used = doc.image.used_presets(region, band, page);
            let line = Line::from(vec![
                Span::styled(if modified { "●" } else { " " }, Style::new().fg(MODIFIED)),
                Span::raw(format!("{:>2} ", page.number())),
                Span::raw(name),
                Span::styled(format!(" {used}/9"), Style::new().dim()),
            ]);
            ListItem::new(line)
        })
        .collect();
    let title = match band {
        Band::Sw => " Pages ".to_owned(),
        _ => format!(" {} pages ", band.label()),
    };
    let list = List::new(items)
        .block(pane(Line::from(title), focused))
        .highlight_style(selection_style(focused))
        .highlight_symbol("▶");
    let mut state = ListState::default().with_selected(Some(usize::from(app.page.number() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
}

/// The presets of the selected page, one row each.
fn presets_on_page(app: &App) -> impl Iterator<Item = PresetRef> {
    let (region, band, page) = (app.region, app.band, app.page);
    Slot::all().filter_map(move |slot| PresetRef::new(region, band, page, slot).ok())
}

fn draw_slots(frame: &mut Frame, area: Rect, app: &App, doc: &Document) {
    let focused = app.focus == Focus::Slots;
    let (region, band, page) = (app.region, app.band, app.page);
    let rows = presets_on_page(app).map(|at| {
        let khz = doc.image.frequency(at);
        let address = at.address();
        let raw = &doc.image.as_bytes()[address..address + 2];
        let name_modified = at.name().is_some_and(|name| doc.image.is_name_modified(name));
        let modified = doc.image.is_preset_modified(at) || name_modified;
        let was = if doc.image.is_preset_modified(at) {
            format!("was {}", frequency_text(band, doc.image.saved_frequency(at)))
        } else {
            String::new()
        };
        let frequency = match khz {
            Some(khz) => Cell::from(format!("{:>7} {}", band.number_text(khz), band.unit())),
            None => Cell::from(Span::styled("      —", Style::new().dim())),
        };
        // SW shows the stored mode here; the other bands the preset's name.
        let detail = match at.name() {
            Some(name) => name_text(&doc.image.name(name)),
            None => match doc.image.mode(at) {
                Some(0) | None => String::new(),
                Some(mode) => mode_label(mode).to_owned(),
            },
        };
        // The lock, and on bands whose column shows names (MW) the mode.
        let mut flags = Vec::new();
        if doc.image.is_locked(at) {
            flags.push("lock");
        }
        if at.name().is_some()
            && let Some(mode) = doc.image.mode(at)
            && mode != 0
        {
            flags.push(mode_label(mode));
        }
        let row = Row::new(vec![
            Cell::from(format!("{}{}", if modified { "●" } else { " " }, at.slot.number())),
            frequency,
            Cell::from(detail),
            Cell::from(flags.join(" ")),
            Cell::from(Span::styled(format!("0x{address:04X}"), Style::new().dim())),
            Cell::from(Span::styled(format!("{:02X} {:02X}", raw[0], raw[1]), Style::new().dim())),
            Cell::from(was),
        ]);
        if modified { row.style(Style::new().fg(MODIFIED)) } else { row }
    });
    let detail_header = if band.names_presets() { "Name" } else { "Mode" };
    let header = Row::new(["Slot", "Frequency", detail_header, "Flags", "Address", "Bytes", "Saved"])
        .style(Style::new().bold().dim());
    let title = match band {
        Band::Sw => {
            let name = name_text(&doc.image.name(NameRef::SwPage(region, page)));
            Line::from(format!(" Page {} · {} ", page.number(), name.trim_end()))
        }
        _ => Line::from(format!(" {} page {} ", band.label(), page.number())),
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(11),
            Constraint::Length(NAME_LEN as u16),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(5),
            Constraint::Min(0),
        ],
    )
    .header(header)
    .column_spacing(2)
    .block(pane(title, focused))
    .row_highlight_style(selection_style(focused));
    let mut state = TableState::default().with_selected(Some(usize::from(app.slot.number() - 1)));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_details(frame: &mut Frame, area: Rect, app: &App, doc: &Document) {
    let (region, page, at) = (app.region, app.page, app.at());
    // The name that belongs to the selection: the page's on SW, the preset's elsewhere.
    let name = at.name().unwrap_or(NameRef::SwPage(region, page));
    let name_at = name.address();
    let name_raw = doc.image.name(name);
    let owner = match name {
        NameRef::SwPage(..) => format!("page {}", page.number()),
        NameRef::Preset(_) => format!("slot {}", at.slot.number()),
    };
    let mut name_line = vec![
        Span::styled("Name ", Style::new().bold()),
        Span::raw(format!("{owner} @ 0x{name_at:04X}  ")),
        Span::styled(hex(&name_raw), Style::new().dim()),
        Span::raw(format!("  \"{}\"", name_text(&name_raw))),
    ];
    if doc.image.is_name_modified(name) {
        let saved = name_text(&doc.image.saved_name(name));
        name_line.push(Span::styled(format!("  was \"{saved}\""), Style::new().fg(MODIFIED)));
    }

    let freq_at = at.address();
    let raw = &doc.image.as_bytes()[freq_at..freq_at + 2];
    let mut slot_line = vec![
        Span::styled("Slot ", Style::new().bold()),
        Span::raw(format!("{} @ 0x{freq_at:04X}  ", at.slot.number())),
        Span::styled(hex(raw), Style::new().dim()),
        Span::raw(format!("  {}", frequency_text(at.band, doc.image.frequency(at)))),
    ];
    if doc.image.is_locked(at) {
        slot_line.push(Span::raw("  locked"));
    }
    if let (Some(mode), Some(mode_at)) = (doc.image.mode(at), at.mode_address())
        && mode != 0
    {
        slot_line.push(Span::raw(format!("  mode {} ", mode_label(mode))));
        slot_line.push(Span::styled(format!("{mode:02X} @ 0x{mode_at:04X}"), Style::new().dim()));
    }
    if doc.image.is_preset_modified(at) {
        let saved = frequency_text(at.band, doc.image.saved_frequency(at));
        slot_line.push(Span::styled(format!("  was {saved}"), Style::new().fg(MODIFIED)));
    }

    let block = Block::bordered()
        .title(format!(" {} · base {} · {} ", region.label(), region.offset_label(), at.band.label()))
        .border_style(Style::new().dim());
    let text = Text::from(vec![Line::from(name_line), Line::from(slot_line)]);
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_empty(frame: &mut Frame, area: Rect) {
    let text = vec![
        Line::from("No image open.".bold()),
        Line::from(""),
        Line::from("Press n for a new image, o to open a 12,032-byte DATA OUT .bin file,"),
        Line::from("or pass a file's path on the command line."),
    ];
    let height = text.len() as u16;
    let [_, middle, _] = Layout::vertical([Constraint::Fill(1), Constraint::Length(height), Constraint::Fill(1)])
        .areas(area);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), middle);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(status) = &app.status {
        let style = match status.kind {
            StatusKind::Info => Style::new().fg(Color::Green),
            StatusKind::Error => Style::new().fg(ERROR).bold(),
        };
        frame.render_widget(Line::from(Span::styled(format!(" {}", status.text), style)), area);
    }
}

fn draw_keys(frame: &mut Frame, area: Rect, app: &App) {
    let sw = app.band == Band::Sw;
    let pairs: Vec<(&str, &str)> = match (&app.mode, &app.doc, app.focus) {
        (Mode::Prompt(_), _, _) => vec![("Enter", "apply"), ("Esc", "cancel"), ("Ctrl+U", "clear")],
        (Mode::Confirm(_) | Mode::Browse(_) | Mode::Save(_) | Mode::Radio(_) | Mode::Settings(_) | Mode::Help, _, _) => {
            vec![]
        }
        (Mode::Normal, None, _) => {
            vec![("n", "new"), ("o", "open"), ("R", "read radio"), ("?", "help"), ("q", "quit")]
        }
        (Mode::Normal, Some(_), Focus::Pages) => vec![
            ("↑↓", "page"),
            ("→", "slots"),
            ("Tab", "bank"),
            ("b", "band"),
            if sw { ("Enter/r", "rename") } else { ("Enter", "open") },
            ("m", "settings"),
            ("u", "revert"),
            ("s", "save"),
            ("S", "save as"),
            ("o", "open"),
            ("n", "new"),
            ("R/W", "read/write radio"),
            ("?", "help"),
            ("q", "quit"),
        ],
        (Mode::Normal, Some(_), Focus::Slots) => vec![
            ("↑↓", "slot"),
            ("←", "pages"),
            ("PgUp/Dn", "page"),
            ("Tab", "bank"),
            ("b", "band"),
            ("Enter/0-9", "edit"),
            if sw { ("r", "rename page") } else { ("r", "name") },
            ("L", "lock"),
            ("x", "clear"),
            ("u", "revert"),
            ("s", "save"),
            ("S", "save as"),
            ("R/W", "read/write radio"),
            ("?", "help"),
            ("q", "quit"),
        ],
    };
    // The last two hints (help, quit) are always shown; others are dropped from the end when space runs out.
    let width = |(key, action): &(&str, &str)| key.chars().count() + action.chars().count() + 3;
    let split = pairs.len().saturating_sub(2);
    let (optional, essential) = pairs.split_at(split);
    let mut room = usize::from(area.width)
        .saturating_sub(1)
        .saturating_sub(essential.iter().map(width).sum());
    let mut shown: Vec<&(&str, &str)> = Vec::new();
    for pair in optional {
        if width(pair) > room {
            break;
        }
        room -= width(pair);
        shown.push(pair);
    }
    shown.extend(essential);
    let mut spans = vec![Span::raw(" ")];
    for (key, action) in shown {
        spans.push(Span::styled(*key, Style::new().fg(ACCENT).bold()));
        spans.push(Span::styled(format!(" {action}  "), Style::new().dim()));
    }
    frame.render_widget(Line::from(spans), area);
}

fn draw_prompt(frame: &mut Frame, prompt: &Prompt) {
    let (title, hint) = match prompt.kind {
        PromptKind::Frequency { at } if at.band == Band::Sw => (
            format!(" {} · page {} · slot {} ", at.region.label(), at.page().number(), at.slot.number()),
            format!("Frequency in whole kHz, {SW_MIN_KHZ}–{SW_MAX_KHZ}"),
        ),
        PromptKind::Frequency { at } => (
            format!(
                " {} · {} page {} · slot {} ",
                at.region.label(),
                at.band.label(),
                at.page().number(),
                at.slot.number()
            ),
            if at.band.in_mhz() {
                format!("Frequency in MHz, {}, in steps of {} kHz", at.band.range_text(), at.band.step_khz())
            } else {
                format!("Frequency in whole kHz, {}", at.band.range_text())
            },
        ),
        PromptKind::Name { name } => (
            match name {
                NameRef::SwPage(region, page) => format!(" Rename page {} · {} ", page.number(), region.label()),
                NameRef::Preset(at) => format!(" Name of {} · {} ", preset_label(at), at.region.label()),
            },
            format!("Up to {NAME_LEN} letters, digits or spaces; padded with spaces (0x20)"),
        ),
        PromptKind::Open => (" Open image ".to_owned(), "Path to a 12,032-byte .bin file".to_owned()),
    };
    let area = centered(frame.area(), 72, 6);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(title.bold());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [hint_area, input_area, _, error_area] = Layout::vertical([Constraint::Length(1); 4]).areas(inner);
    frame.render_widget(Line::from(hint.dim()), hint_area);

    draw_input(frame, input_area, Span::styled("› ", Style::new().fg(ACCENT)), &prompt.input, true);

    if let Some(error) = &prompt.error {
        frame.render_widget(Line::from(Span::styled(error.as_str(), Style::new().fg(ERROR))), error_area);
    }
}

/// One line of editable text after `label`, scrolled so the cursor stays visible.
fn draw_input(frame: &mut Frame, area: Rect, label: Span<'static>, input: &LineInput, cursor: bool) {
    let label_width = label.width() as u16;
    let width = usize::from(area.width.saturating_sub(label_width + 1)).max(1);
    let chars = input.chars();
    let start = input.cursor().saturating_sub(width);
    let visible: String = chars[start..chars.len().min(start + width)].iter().collect();
    frame.render_widget(Line::from(vec![label, Span::raw(visible)]), area);
    if cursor && area.width > label_width && area.height > 0 {
        let x = area.x + label_width + (input.cursor() - start) as u16;
        frame.set_cursor_position(Position::new(x.min(area.right().saturating_sub(1)), area.y));
    }
}

/// The Open dialog (`save` is `None`) or the Save / Save As dialog.
fn draw_file_dialog(frame: &mut Frame, browser: &Browser, save: Option<&SaveDialog>) {
    let screen = frame.area();
    let area = centered(screen, 90, screen.height.saturating_sub(2).max(8));
    frame.render_widget(Clear, area);
    let title = match save {
        None => " Open image ",
        Some(dialog) if dialog.save_as => " Save As ",
        Some(_) => " Save new image ",
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(title.bold());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let name_height = if save.is_some() { 1 } else { 0 };
    let [path_area, list_area, name_area, info_area, keys_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(name_height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let dir = browser.dir.display().to_string();
    let room = usize::from(path_area.width).saturating_sub(1);
    frame.render_widget(Line::from(truncate_left(&dir, room).bold()), path_area);

    let list_focused = save.is_none_or(|dialog| dialog.focus == DialogFocus::List);
    let rows = browser.entries.iter().map(|entry| {
        let (name, size, style) = match entry.kind {
            EntryKind::Parent => ("../".to_owned(), Span::raw(""), Style::new().fg(ACCENT)),
            EntryKind::Dir => (format!("{}/", entry.name), Span::styled("folder", Style::new().dim()), Style::new().fg(ACCENT)),
            EntryKind::File { size } if entry.is_image() => (
                entry.name.clone(),
                Span::styled(format!("{} ✓", format_size(size.unwrap_or_default())), Style::new().fg(Color::Green)),
                Style::new(),
            ),
            EntryKind::File { size } => (
                entry.name.clone(),
                Span::raw(size.map_or_else(|| "unreadable".to_owned(), format_size)),
                Style::new().dim(),
            ),
        };
        Row::new(vec![Cell::from(name), Cell::from(Line::from(size).alignment(Alignment::Right))]).style(style)
    });
    let table = Table::new(rows, [Constraint::Min(10), Constraint::Length(12)])
        .column_spacing(2)
        .row_highlight_style(selection_style(list_focused))
        .highlight_symbol("▶ ");
    let mut state = TableState::default().with_selected(Some(browser.selected));
    frame.render_stateful_widget(table, list_area, &mut state);

    if let Some(dialog) = save {
        let label_style = if dialog.focus == DialogFocus::Name {
            Style::new().fg(ACCENT).bold()
        } else {
            Style::new().dim()
        };
        let label = Span::styled("File name: ", label_style);
        draw_input(frame, name_area, label, &dialog.name, dialog.focus == DialogFocus::Name);
    }

    let error = save.and_then(|dialog| dialog.error.as_deref()).or(browser.error.as_deref());
    let info = match error {
        Some(error) => Line::from(Span::styled(error, Style::new().fg(ERROR))),
        None => {
            let images = browser.entries.iter().filter(|e| e.is_image()).count();
            let mut text = format!("{images} image{} of 12,032 bytes", if images == 1 { "" } else { "s" });
            if browser.filtered_files > 0 {
                let n = browser.filtered_files;
                text += &format!(" · {n} other file{} hidden", if n == 1 { "" } else { "s" });
            }
            if save.is_some() {
                text += " · .bin added if no extension";
            }
            Line::from(text.dim())
        }
    };
    frame.render_widget(info, info_area);

    let all = if browser.show_all { "images only" } else { "all files" };
    let hidden = if browser.show_hidden { "hide dotfiles" } else { "dotfiles" };
    let pairs: Vec<(&str, &str)> = match save.map(|dialog| dialog.focus) {
        None => vec![
            ("Enter", "open"),
            ("←", "up"),
            ("~", "home"),
            ("/", "type path"),
            ("a", all),
            (".", hidden),
            ("Esc", "cancel"),
        ],
        Some(DialogFocus::Name) => vec![("Enter", "save"), ("↑↓", "choose a folder"), ("Esc", "cancel")],
        Some(DialogFocus::List) => vec![
            ("Enter", "open folder / pick file"),
            ("←", "up a folder"),
            ("type", "file name"),
            ("Esc", "cancel"),
        ],
    };
    let mut keys = Vec::new();
    for (key, action) in pairs {
        keys.push(Span::styled(key, Style::new().fg(ACCENT).bold()));
        keys.push(Span::styled(format!(" {action}  "), Style::new().dim()));
    }
    frame.render_widget(Line::from(keys), keys_area);
}

fn draw_confirm(frame: &mut Frame, app: &App, confirm: &Confirm) {
    let (title, message, choices): (&str, String, &[(&str, &str)]) = match confirm {
        Confirm::Discard { next } => {
            let (file, bytes) = app
                .doc
                .as_ref()
                .map(|doc| (doc.display_name(), doc.image.modified_byte_count()))
                .unwrap_or_default();
            let (title, action) = match next {
                AfterDiscard::Quit => (" Quit ", "quitting".to_owned()),
                AfterDiscard::New => (" New image ", "starting a new image".to_owned()),
                AfterDiscard::Open(doc) => (" Open another file ", format!("opening {}", doc.display_name())),
                AfterDiscard::ReadRadio => (" Read from radio ", "reading the radio".to_owned()),
                AfterDiscard::WriteRadio => (" Write to radio ", "writing to the radio".to_owned()),
            };
            (
                title,
                format!("{file} has unsaved changes ({bytes} bytes). Save them before {action}?"),
                &[("s", "save first"), ("d", "discard"), ("Esc", "cancel")],
            )
        }
        Confirm::Overwrite { path, .. } => (
            " Replace file ",
            format!("{} already exists. Saving will overwrite it. Replace it?", path.display()),
            &[("y", "replace"), ("n/Esc", "choose another name")],
        ),
        Confirm::SaveBeforeRadio => (
            " Write to radio ",
            "Save the image first, so what goes into the radio is also a file on disk.".to_owned(),
            &[("s", "save and continue"), ("Esc", "cancel")],
        ),
        // Drawn by draw_write_confirm.
        Confirm::WriteRadio { .. } => return,
    };
    let area = centered(frame.area(), 64, 7);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MODIFIED))
        .title(title.bold());
    let mut lines = vec![Line::from(message), Line::from("")];
    let mut keys = Vec::new();
    for (key, action) in choices {
        keys.push(Span::styled(format!("[{key}]"), Style::new().fg(ACCENT).bold()));
        keys.push(Span::raw(format!(" {action}   ")));
    }
    lines.push(Line::from(keys));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(block), area);
}

fn bank_letter(region: memory_manager::image::Region) -> char {
    region.label().chars().last().unwrap_or('?')
}

/// A preset in few characters: `A49/1` on SW, `A FM1/3` on the other bands.
fn short_preset(at: PresetRef) -> String {
    let (bank, page, slot) = (bank_letter(at.region), at.page().number(), at.slot.number());
    match at.band {
        Band::Sw => format!("{bank}{page}/{slot}"),
        band => format!("{bank} {}{page}/{slot}", band.label()),
    }
}

fn draw_write_confirm(
    frame: &mut Frame,
    file: &str,
    reference: &str,
    diff: &ImageDiff,
    has_backup: bool,
) {
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let mut lines = vec![Line::from(vec![
        Span::raw("Write "),
        Span::styled(file.to_owned(), Style::new().bold()),
        Span::raw(" to the radio?"),
    ])];
    if !has_backup {
        lines.push(Line::from("Nothing to compare it with: the radio has not been read in this session.".dim()));
    } else if diff.presets.is_empty() && diff.names.is_empty() {
        lines.push(Line::from(format!("Compared with {reference}:")));
        lines.push(Line::from("  No preset slots or names differ.".dim()));
    } else {
        lines.push(Line::from(format!("Compared with {reference}:")));
        lines.push(Line::from(format!(
            "  {} and {} differ.",
            plural(diff.presets.len(), "preset slot"),
            plural(diff.names.len(), "name")
        )));
        let mut examples: Vec<String> = diff
            .presets
            .iter()
            .map(|&at| short_preset(at))
            .chain(diff.names.iter().map(|name| match *name {
                NameRef::SwPage(region, page) => format!("name {}{}", bank_letter(region), page.number()),
                NameRef::Preset(at) => format!("name {}", short_preset(at)),
            }))
            .take(8)
            .collect();
        if diff.presets.len() + diff.names.len() > examples.len() {
            examples.push("…".to_owned());
        }
        lines.push(Line::from(format!("  {}", examples.join(", ")).dim()));
    }
    if has_backup && !diff.settings.is_empty() {
        let names: Vec<&str> = diff.settings.iter().map(|setting| setting.label()).collect();
        lines.push(Line::from(format!("  Settings that differ: {}.", names.join(", "))));
    }
    if diff.other_bytes > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "⚠ {} outside presets and names differ: settings, timers or unknown bytes. MemoryManager \
                 never changes those, so the write puts back what this file holds. Either the radio's \
                 settings changed since the file was read, or the file comes from another radio.",
                plural(diff.other_bytes, "byte")
            ),
            Style::new().fg(ERROR).bold(),
        )));
    }
    if !has_backup {
        lines.push(Line::from(Span::styled(
            "⚠ No backup of the radio was read in this session.",
            Style::new().fg(MODIFIED),
        )));
    }
    lines.push(Line::from("DATA IN replaces the radio's memory with this image.".dim()));
    lines.push(Line::from(""));
    let mut keys = vec![Span::styled("[y]", Style::new().fg(ACCENT).bold()), Span::raw(" write   ")];
    if !has_backup {
        keys.push(Span::styled("[b]", Style::new().fg(ACCENT).bold()));
        keys.push(Span::raw(" back up the radio first   "));
    }
    keys.push(Span::styled("[Esc]", Style::new().fg(ACCENT).bold()));
    keys.push(Span::raw(" cancel"));
    lines.push(Line::from(keys));

    let area = centered(frame.area(), 76, lines.len() as u16 + 4);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MODIFIED))
        .title(" Write to radio ".bold());
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(block), area);
}

/// A text progress bar such as `━━━━━━────────`.
fn bar(ratio: f64, width: usize) -> Vec<Span<'static>> {
    let filled = ((ratio.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    vec![
        Span::styled("━".repeat(filled), Style::new().fg(ACCENT)),
        Span::styled("─".repeat(width - filled), Style::new().dim()),
    ]
}

fn step(done: bool, text: String) -> Line<'static> {
    if done {
        Line::from(vec![Span::styled("✓ ", Style::new().fg(Color::Green)), Span::raw(text)])
    } else {
        Line::from(vec![Span::styled("… ", Style::new().fg(ACCENT)), Span::raw(text)])
    }
}

fn draw_radio(frame: &mut Frame, transfer: &Transfer, log_path: Option<&std::path::Path>) {
    let title = match &transfer.kind {
        TransferKind::Read => " Read from radio ".to_owned(),
        TransferKind::Backup => " Back up the radio ".to_owned(),
        TransferKind::Write { file, .. } => format!(" Write {file} to radio "),
        TransferKind::Verify { .. } => " Read back and compare ".to_owned(),
    };
    let writing = matches!(transfer.kind, TransferKind::Write { .. });
    let mut lines: Vec<Line> = Vec::new();

    match &transfer.connection {
        Some(connection) => lines.push(step(true, format!("Bridge connected: {connection}"))),
        None => lines.push(step(false, "Connecting to the ESP32 bridge…".to_owned())),
    }
    if writing {
        let uploaded = transfer.waiting.is_some();
        match transfer.upload {
            _ if uploaded => lines.push(step(true, "Image uploaded to the bridge".to_owned())),
            Some((done, total)) => lines.push(step(false, format!("Uploading to the bridge {done}/{total}"))),
            None if transfer.connection.is_some() => lines.push(step(false, "Uploading to the bridge".to_owned())),
            None => {}
        }
    }

    if let Some((direction, armed_at, window)) = transfer.waiting {
        lines.push(Line::from(""));
        let (keys, name) = match direction {
            Direction::Out => ("hold FM + LIGHT, then press ENTER", "DATA OUT"),
            Direction::In => ("hold LIGHT + LW, then press ENTER", "DATA IN"),
        };
        let radio_done = transfer.download.is_some() || matches!(transfer.outcome, Some(Outcome::Written { .. }));
        if direction == Direction::Out && !radio_done {
            lines.push(Line::from(vec![
                Span::styled("▶ ", Style::new().fg(ACCENT).bold()),
                Span::styled("First tune FM to an empty frequency", Style::new().bold()),
                Span::raw(", with no station. RDS radio text can spoil the export."),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled(if radio_done { "✓ " } else { "▶ " }, Style::new().fg(if radio_done { Color::Green } else { ACCENT }).bold()),
            Span::styled("On the radio: ", Style::new().bold()),
            Span::styled(keys, Style::new().fg(ACCENT).bold()),
            Span::raw(format!("  ({name})")),
        ]));
        match &transfer.progress {
            None if transfer.active() => {
                // The bridge stays silent while the radio transfers and reports once at the end.
                let left = window.saturating_sub(Instant::now().saturating_duration_since(armed_at)).as_secs();
                let text = if left > 0 {
                    format!(
                        "  Start it within {}:{:02}; the bridge reports back when the radio has finished.",
                        left / 60,
                        left % 60
                    )
                } else {
                    "  Waiting for the bridge's report…".to_owned()
                };
                lines.push(Line::from(text.dim()));
            }
            None => {}
            Some((_, stats)) => {
                let (label, ratio) = match direction {
                    Direction::Out if stats.packets == 0 && stats.seen > 0 => (
                        format!("Radio is talking ({} transactions) but no packets were stored yet", stats.seen),
                        None,
                    ),
                    Direction::Out if stats.packets < PACKET_COUNT => (
                        format!("Radio sending {}/{PACKET_COUNT} packets", stats.packets),
                        Some(f64::from(stats.packets) / f64::from(PACKET_COUNT)),
                    ),
                    Direction::Out => (
                        format!("Radio verifying {}/{BLOCK_COUNT} blocks", stats.verify),
                        Some(f64::from(stats.verify) / BLOCK_COUNT as f64),
                    ),
                    Direction::In => (
                        format!("Radio reading, pass {} of 2", (stats.passes + 1).min(2)),
                        Some(f64::from(stats.bytes) / f64::from(IN_TOTAL_BYTES)),
                    ),
                };
                let mut spans = vec![Span::raw("  ")];
                if let Some(ratio) = ratio {
                    spans.extend(bar(if radio_done { 1.0 } else { ratio }, 30));
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::raw(label));
                lines.push(Line::from(spans));
            }
        }
    }

    if let Some((done, total)) = transfer.download {
        lines.push(Line::from(""));
        lines.push(step(done == total, format!("Downloaded from the bridge {done}/{total}")));
    }

    lines.push(Line::from(""));
    let mut keys: Vec<(&str, &str)> = Vec::new();
    match &transfer.outcome {
        None if transfer.cancelling => lines.push(Line::from("Cancelling…".dim())),
        None => keys.push(("Esc", "cancel")),
        Some(Outcome::Written { warning }) => {
            lines.push(Line::from(Span::styled(
                "✓ Done. The radio should show OK.",
                Style::new().fg(Color::Green).bold(),
            )));
            if let Some(warning) = warning {
                lines.push(Line::from(Span::styled(warning.clone(), Style::new().fg(MODIFIED))));
            }
            keys.push(("v", "read back and compare"));
            keys.push(("Esc", "close"));
        }
        Some(Outcome::Verified { diff }) => {
            if diff.is_empty() {
                lines.push(Line::from(Span::styled(
                    "✓ The radio's memory matches the written image exactly.",
                    Style::new().fg(Color::Green).bold(),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    format!(
                        "✗ The radio differs from the written image: {} slots, {} names, {} settings, {} other bytes.",
                        diff.presets.len(),
                        diff.names.len(),
                        diff.settings.len(),
                        diff.other_bytes
                    ),
                    Style::new().fg(ERROR).bold(),
                )));
            }
            keys.push(("Esc", "close"));
        }
        Some(Outcome::Failed(message)) => {
            lines.push(Line::from(Span::styled(format!("✗ {message}"), Style::new().fg(ERROR).bold())));
            if matches!(transfer.kind, TransferKind::Verify { .. }) {
                keys.push(("v", "read back again"));
            }
            keys.push(("Esc", "close"));
        }
    }
    let mut key_spans = Vec::new();
    for (key, action) in keys {
        key_spans.push(Span::styled(format!("[{key}]"), Style::new().fg(ACCENT).bold()));
        key_spans.push(Span::raw(format!(" {action}   ")));
    }
    lines.push(Line::from(key_spans));
    if let Some(path) = log_path {
        lines.push(Line::from(format!("Transfer log: {}", path.display()).dim()));
    }

    let area = centered(frame.area(), 78, lines.len() as u16 + 4);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(title.bold());
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}

/// The menu settings, editable, and the timers, shown only.
fn draw_settings(frame: &mut Frame, doc: &Document, selected: usize) {
    let image = &doc.image;
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("  {:<13}{:<18}{:<12}{}", "Setting", "Value", "Factory", "Where"), Style::new().bold().dim()),
    ])];
    for (index, setting) in Setting::ALL.into_iter().enumerate() {
        let modified = image.is_setting_modified(setting);
        let mut spans = vec![
            Span::styled(if modified { "●" } else { " " }, Style::new().fg(MODIFIED)),
            Span::raw(if index == selected { "▶" } else { " " }),
            Span::raw(format!("{:<13}", setting.label())),
            Span::raw(format!("{:<18}", setting.text(image.setting(setting)))),
            Span::styled(format!("{:<12}", setting.text(setting.factory())), Style::new().dim()),
            Span::styled(setting.location(), Style::new().dim()),
        ];
        if modified {
            spans.push(Span::styled(
                format!("  was {}", setting.text(image.saved_setting(setting))),
                Style::new().fg(MODIFIED),
            ));
        }
        let line = Line::from(spans);
        lines.push(if index == selected { line.style(selection_style(true)) } else { line });
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  Timers (shown only)", Style::new().bold())));
    for number in 1..=TIMERS {
        let timer = image.timer(number);
        lines.push(Line::from(format!(
            "  {number}  {}  {:<12}{}",
            timer.time_text(),
            timer.state_text(),
            timer.station_text()
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "  Writing changed settings to the radio has not been tested yet.".dim(),
    ));
    lines.push(Line::from(vec![
        Span::styled("  ↑↓", Style::new().fg(ACCENT).bold()),
        Span::raw(" choose   "),
        Span::styled("←→", Style::new().fg(ACCENT).bold()),
        Span::raw(" change   "),
        Span::styled("u", Style::new().fg(ACCENT).bold()),
        Span::raw(" revert   "),
        Span::styled("Esc", Style::new().fg(ACCENT).bold()),
        Span::raw(" close"),
    ]));
    let area = centered(frame.area(), 84, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(" Menu settings · MemoBank A, 0x0F08–0x0F09 ".bold());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_help(frame: &mut Frame) {
    let rows: &[(&str, &str)] = &[
        ("Tab / Shift+Tab", "next / previous MemoBank"),
        ("b / B", "next / previous band: FM, MW, LW, SW, AIR"),
        ("← → (h l)", "focus pages / slots"),
        ("↑ ↓ (k j)", "move selection"),
        ("PgUp / PgDn", "pages: ±10 · slots: previous / next page"),
        ("Home / End (g G)", "first / last item"),
        ("Enter / e", "rename an SW page, or edit the slot's frequency"),
        ("0–9", "in slots: start typing a new frequency"),
        ("r", "rename the SW page; on other bands, name the preset"),
        ("L", "lock or unlock the selected preset"),
        ("m", "menu settings and timers"),
        ("x / Del / Backspace", "clear the selected slot"),
        ("u", "revert the selected field to its saved bytes"),
        ("n", "new image (all presets and names cleared, on every band)"),
        ("o", "browse for an image to open"),
        ("s", "save; asks for a file name if the image is new"),
        ("S", "save as: choose folder and file name"),
        ("R", "read from radio (DATA OUT through the ESP32 bridge)"),
        ("W", "write the saved image to the radio (DATA IN)"),
        ("q / Ctrl+C", "quit"),
    ];
    let mut lines: Vec<Line> = rows
        .iter()
        .map(|(key, action)| {
            Line::from(vec![
                Span::styled(format!("{key:<20}"), Style::new().fg(ACCENT).bold()),
                Span::raw(*action),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(
        "Only edited fields are written; all other bytes are kept as loaded.".dim(),
    ));
    lines.push(Line::from(
        "MemoBank A, B and C are stored at EEPROM 0x0000, 0x1000 and 0x2000.".dim(),
    ));
    lines.push(Line::from(
        "FM and AIR are entered in MHz (91.30, 121.500); MW, LW and SW in whole kHz.".dim(),
    ));
    lines.push(Line::from("Press any key to close.".dim()));
    let area = centered(frame.area(), 92, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(" Keys ".bold());
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
}

/// Keeps the end of `text` (the file name) when it does not fit in `max` columns.
fn truncate_left(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let tail: String = text.chars().skip(count - (max - 1)).collect();
    format!("…{tail}")
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{TempDir, app_with_markers, app_with_original, fixture};
    use crate::app::{Document, RadioBackup, resolved};
    use memory_manager::image::EepromImage;
    use std::fs;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn renders_every_band_with_lock_mode_and_names() {
        let mut app = app_with_markers();
        let screen = render(&app, 120, 40);
        assert!(screen.contains("FM 4 │ MW 3 │ LW 1 │ SW 49 │ AIR 5"), "{screen}");

        // SW page 47: the preset stored in USB shows its mode.
        for _ in 0..46 {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Right);
        let screen = render(&app, 120, 40);
        assert!(screen.contains("14200 kHz  USB"), "{screen}");
        assert!(screen.contains("mode USB D8 @ 0x0E14"), "{screen}");

        // FM: frequencies in MHz, the locked preset, and names per preset.
        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        let screen = render(&app, 120, 40);
        assert!(screen.contains(" FM pages "), "{screen}");
        assert!(screen.contains(" 1 FM page    3/9"), "{screen}");
        assert!(screen.contains(" FM page 1 "), "{screen}");
        assert!(screen.contains("87.50 MHz") && screen.contains("108.00 MHz"), "{screen}");
        assert!(screen.contains("lock"), "{screen}");
        assert!(screen.contains("Name slot 3 @ 0x0014"), "{screen}");
        assert!(screen.contains("AA A3  91.30 MHz  locked"), "{screen}");
        assert!(screen.contains("MemoBank A · base 0x0000 · FM"), "{screen}");
        let last_line = screen.lines().last().unwrap();
        assert!(last_line.contains("b band") && last_line.contains("L lock"), "{last_line}");

        press(&mut app, KeyCode::Char('r'));
        for c in "RADIO 1".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let screen = render(&app, 120, 40);
        assert!(screen.contains(" Name of FM page 1 slot 3 · MemoBank A "), "{screen}");
        press(&mut app, KeyCode::Enter);
        let screen = render(&app, 120, 40);
        assert!(screen.contains("91.30 MHz  RADIO 1"), "{screen}");
        assert!(screen.contains("unsaved: 10 bytes"), "{screen}");

        // AIR needs three decimals; its prompt says how to type it.
        press(&mut app, KeyCode::Char('B'));
        press(&mut app, KeyCode::Enter);
        let screen = render(&app, 120, 40);
        assert!(screen.contains(" MemoBank A · AIR page 1 · slot 3 "), "{screen}");
        assert!(screen.contains("Frequency in MHz, 118.000–137.000 MHz, in steps of 5 kHz"), "{screen}");
        assert!(screen.contains("121.500"), "{screen}");
    }

    #[test]
    fn an_mw_preset_in_usb_shows_its_mode() {
        let mut bytes = memory_manager::sample::with_presets_on_every_band();
        bytes[0x07C6..0x07C8].copy_from_slice(&522u16.to_le_bytes());
        bytes[0x0803] = 0xD8;
        bytes[0x0200..0x020A].copy_from_slice(b"999@@@@@@@");
        let image = EepromImage::from_bytes(&bytes).unwrap();
        let mut app = App::new(Some(Document { path: None, image }));
        for c in "bbb".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Right);
        for _ in 0..3 {
            press(&mut app, KeyCode::Down);
        }
        let screen = render(&app, 120, 40);
        assert!(screen.contains("Flags"), "{screen}");
        assert!(screen.contains("522 kHz  999         USB"), "{screen}");
        assert!(screen.contains("mode USB D8 @ 0x0803"), "{screen}");
    }

    #[test]
    fn the_settings_screen_shows_settings_and_timers() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Right);
        let screen = render(&app, 120, 40);
        assert!(screen.contains("Menu settings · MemoBank A, 0x0F08–0x0F09"), "{screen}");
        assert!(screen.contains("Backlight    30 s"), "{screen}");
        assert!(screen.contains("0x0F09 bits 2–3  was 20 s"), "{screen}");
        assert!(screen.contains("Beep         off"), "{screen}");
        assert!(screen.contains("Timers (shown only)"), "{screen}");
        assert!(screen.contains("1  00:00  off, buzzer FM 100.00 MHz"), "{screen}");
        assert!(screen.contains("has not been tested yet"), "{screen}");
        for (w, h) in [(60, 20), (40, 10), (20, 5)] {
            render(&app, w, h);
        }

        // A write lists changed settings when there is a backup to compare with.
        press(&mut app, KeyCode::Esc);
        let original = EepromImage::load(&fixture("original-data-out.bin")).unwrap();
        app.radio_backup = Some(RadioBackup { image: original, taken: "12:00".into() });
        let dir = TempDir::new("render-settings");
        let path = dir.0.join("set.bin");
        app.doc.as_mut().unwrap().image.save(&path).unwrap();
        app.doc.as_mut().unwrap().path = Some(resolved(path));
        press(&mut app, KeyCode::Char('W'));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Settings that differ: Backlight."), "{screen}");
    }

    #[test]
    fn renders_pages_slots_and_file_state() {
        let mut app = app_with_original();
        let screen = render(&app, 120, 40);
        assert!(screen.contains("original-data-out.bin"), "{screen}");
        assert!(screen.contains("saved"));
        assert!(screen.contains("MemoBank A 0x0000"), "{screen}");
        assert!(screen.contains("MemoBank C 0x2000"));
        assert!(screen.contains(" 1 BBC 1 "), "{screen}");
        assert!(!screen.contains("BBC@"));
        assert!(screen.contains("42 42 43 40 31 40"), "raw bytes stay visible in the details");
        assert!(screen.contains(" 9410 kHz"));
        assert!(screen.contains("0x0840"));

        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Right);
        for c in "9999".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        let screen = render(&app, 120, 40);
        assert!(screen.contains("unsaved: 2 bytes"), "{screen}");
        assert!(screen.contains(" 9999 kHz"));
        assert!(screen.contains("was 9765 kHz"));
        assert!(screen.contains("0F 27"));

        // Help and quit stay visible in the key bar on narrow terminals.
        let last_line = render(&app, 60, 20).lines().last().unwrap().to_owned();
        assert!(last_line.contains("? help") && last_line.contains("q quit"), "{last_line}");
    }

    #[test]
    fn every_mode_renders_at_any_size_without_panicking() {
        let mut app = app_with_original();
        let sizes = [(120, 40), (80, 24), (40, 12), (20, 6), (5, 3), (1, 1)];
        let keys: &[&[KeyCode]] = &[
            &[],
            &[KeyCode::Char('?')],
            &[KeyCode::Esc, KeyCode::Char('r')],
            &[KeyCode::Esc, KeyCode::Right, KeyCode::Char('1')],
            &[KeyCode::Esc, KeyCode::Char('x'), KeyCode::Char('q')],
            &[KeyCode::Esc, KeyCode::Char('o')],
            &[KeyCode::Char('a'), KeyCode::Char('.'), KeyCode::Char('/')],
            &[KeyCode::Esc, KeyCode::Char('S')],
            &[KeyCode::Tab],
            &[KeyCode::BackTab, KeyCode::Enter],
            &[KeyCode::Esc, KeyCode::Esc, KeyCode::Char('n')],
            &[KeyCode::Char('d')],
            &[KeyCode::Char('W')],
            &[KeyCode::Esc, KeyCode::Char('R')],
        ];
        for sequence in keys {
            for &code in *sequence {
                press(&mut app, code);
            }
            for (w, h) in sizes {
                render(&app, w, h);
            }
        }
        for (w, h) in sizes {
            render(&App::new(None), w, h);
        }
    }

    #[test]
    fn browser_lists_folders_and_marks_images() {
        // A folder of its own, so files added next to the fixtures do not affect the counts.
        let dir = TempDir::new("render-browse");
        fs::create_dir(dir.0.join("Sketches")).unwrap();
        fs::copy(fixture("original-data-out.bin"), dir.0.join("radio.bin")).unwrap();
        fs::copy(fixture("edited-9999-data-out.bin"), dir.0.join("backup.bin")).unwrap();
        fs::write(dir.0.join("README.md"), b"notes").unwrap();
        let path = resolved(dir.0.join("radio.bin"));
        let image = EepromImage::load(&path).unwrap();
        let mut app = App::new(Some(Document { path: Some(path), image }));

        press(&mut app, KeyCode::Char('o'));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Open image"), "{screen}");
        assert!(screen.contains("../"));
        assert!(screen.contains("Sketches/"));
        assert!(screen.contains("▶ radio.bin"));
        assert!(screen.contains("backup.bin"));
        assert!(screen.contains("12,032 B ✓"));
        assert!(screen.contains("2 images of 12,032 bytes · 1 other file hidden"));
        assert!(!screen.contains("README.md"));

        press(&mut app, KeyCode::Char('a'));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("README.md"));
        assert!(screen.contains("5 B"));
        assert!(screen.contains("images only"));
    }

    #[test]
    fn save_as_dialog_and_untitled_image_render() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Char('S'));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Save As"), "{screen}");
        assert!(screen.contains("File name: original-data-out.bin"));
        assert!(screen.contains("Enter save"));
        assert!(screen.contains("12,032 B ✓"));

        press(&mut app, KeyCode::Enter); // existing file
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Replace file"), "{screen}");
        assert!(screen.contains("overwrite"));

        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('n'));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Untitled"), "{screen}");
        assert!(screen.contains("new, not saved yet"));
        assert!(screen.contains("all presets and names cleared"));
        press(&mut app, KeyCode::Char('s'));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Save new image"), "{screen}");
    }

    #[test]
    fn radio_panels_render_each_stage() {
        use memory_manager::radio::protocol::{Direction, Stats};
        use memory_manager::radio::worker::{RadioEvent, Stage};
        use std::time::Duration;

        let sizes = [(100, 30), (60, 16), (20, 6), (1, 1)];
        let mut app = app_with_original();
        press(&mut app, KeyCode::Char('R'));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Read from radio"), "{screen}");
        assert!(screen.contains("Connecting to the ESP32 bridge"));

        app.on_radio_event(RadioEvent::Connected { port: "/dev/cu.usbserial-0001".into(), firmware: "1.0.0".into() });
        app.on_radio_event(RadioEvent::Stage(Stage::WaitingForRadio {
            direction: Direction::Out,
            window: Duration::from_secs(120),
        }));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("On the radio: hold FM + LIGHT, then press ENTER  (DATA OUT)"), "{screen}");
        assert!(screen.contains("First tune FM to an empty frequency, with no station."), "{screen}");
        assert!(screen.contains("Start it within 1:59") || screen.contains("Start it within 2:00"), "{screen}");
        assert!(screen.contains("[Esc] cancel"));

        app.log_path = Some(std::path::PathBuf::from("/tmp/radio.log"));
        let stats = Stats { seen: 12, ..Stats::default() };
        app.on_radio_event(RadioEvent::Stage(Stage::Transferring { direction: Direction::Out, stats }));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Radio is talking (12 transactions) but no packets were stored yet"), "{screen}");
        assert!(screen.contains("Transfer log: /tmp/radio.log"));

        let stats = Stats { packets: 376, ..Stats::default() };
        app.on_radio_event(RadioEvent::Stage(Stage::Transferring { direction: Direction::Out, stats }));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Radio sending 376/752 packets"), "{screen}");
        for (w, h) in sizes {
            render(&app, w, h);
        }

        app.on_radio_event(RadioEvent::Failed { message: "The radio stopped before the transfer was complete.".into() });
        let screen = render(&app, 100, 30);
        assert!(screen.contains("✗ The radio stopped"), "{screen}");
        assert!(screen.contains("[Esc] close"));
        press(&mut app, KeyCode::Esc);

        // Write: confirmation, then progress and the read-back offer.
        let dir = TempDir::new("render-radio");
        let path = dir.0.join("radio.bin");
        fs::copy(fixture("edited-9999-data-out.bin"), &path).unwrap();
        let image = EepromImage::load(&path).unwrap();
        let mut app = App::new(Some(Document { path: Some(resolved(path)), image }));
        press(&mut app, KeyCode::Char('W'));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Write radio.bin to the radio?"), "{screen}");
        assert!(screen.contains("Nothing to compare it with"), "{screen}");
        assert!(screen.contains("No backup of the radio"));
        assert!(screen.contains("[b] back up the radio first"));
        // With a backup read in this session, the confirmation lists what the file changes.
        press(&mut app, KeyCode::Esc);
        let backup = EepromImage::from_bytes(&fs::read(fixture("original-data-out.bin")).unwrap()).unwrap();
        app.radio_backup = Some(RadioBackup { image: backup, taken: "12:00".into() });
        press(&mut app, KeyCode::Char('W'));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Compared with the radio backup read at 12:00:"), "{screen}");
        assert!(screen.contains("1 preset slot and 0 names differ"));
        assert!(screen.contains("A49/1"));
        assert!(!screen.contains("No backup of the radio"));
        for (w, h) in sizes {
            render(&app, w, h);
        }
        press(&mut app, KeyCode::Char('y'));
        app.on_radio_event(RadioEvent::Stage(Stage::Uploading { done: 12, total: 47 }));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Write radio.bin to radio"), "{screen}");
        assert!(screen.contains("Uploading to the bridge 12/47"));
        app.on_radio_event(RadioEvent::Stage(Stage::WaitingForRadio {
            direction: Direction::In,
            window: Duration::from_secs(120),
        }));
        let stats = Stats { bytes: 18_000, passes: 1, ..Stats::default() };
        app.on_radio_event(RadioEvent::Stage(Stage::Transferring { direction: Direction::In, stats }));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("Image uploaded to the bridge"), "{screen}");
        assert!(screen.contains("hold LIGHT + LW, then press ENTER"));
        assert!(screen.contains("Radio reading, pass 2 of 2"));
        app.on_radio_event(RadioEvent::WriteFinished { stats: Stats { passes: 2, ..Stats::default() } });
        let screen = render(&app, 100, 30);
        assert!(screen.contains("✓ Done. The radio should show OK."), "{screen}");
        assert!(screen.contains("[v] read back and compare"));
    }

    #[test]
    fn write_confirmation_warns_about_foreign_bytes() {
        let dir = TempDir::new("render-foreign");
        let path = dir.0.join("odd.bin");
        let original = fs::read(fixture("original-data-out.bin")).unwrap();
        let mut bytes = original.clone();
        bytes[0x0E52] ^= 1; // a byte the editor never changes: padding after page 49's presets
        fs::write(&path, &bytes).unwrap();
        let image = EepromImage::load(&path).unwrap();
        let mut app = App::new(Some(Document { path: Some(resolved(path)), image }));
        let backup = EepromImage::from_bytes(&original).unwrap();
        app.radio_backup = Some(RadioBackup { image: backup, taken: "12:00".into() });
        press(&mut app, KeyCode::Char('W'));
        let screen = render(&app, 100, 30);
        assert!(screen.contains("⚠ 1 byte outside presets and names differ"), "{screen}");
    }

    #[test]
    fn long_prompt_input_scrolls_to_the_cursor() {
        let mut app = app_with_original();
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Char('/'));
        for c in "/a/very/long/path/that/does/not/fit/in/the/prompt/at/all/".repeat(3).chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Char('Z'));
        let screen = render(&app, 60, 20);
        assert!(screen.contains("all/Z"), "{screen}");
    }

    #[test]
    fn truncate_left_keeps_the_file_name() {
        assert_eq!(truncate_left("/a/b/file.bin", 50), "/a/b/file.bin");
        assert_eq!(truncate_left("/a/b/file.bin", 9), "…file.bin");
        assert_eq!(truncate_left("/a/b/file.bin", 0), "");
    }
}

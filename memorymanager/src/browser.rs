//! Directory listing for the Open dialog. Knows nothing about drawing.

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use memory_manager::image::IMAGE_LEN;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Parent,
    Dir,
    /// `None` when the size could not be read, e.g. a broken symlink.
    File { size: Option<u64> },
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
}

impl Entry {
    /// A file with exactly the length of a supported image.
    pub fn is_image(&self) -> bool {
        self.kind == (EntryKind::File { size: Some(IMAGE_LEN as u64) })
    }
}

pub struct Browser {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    /// Show files whose size rules them out as images.
    pub show_all: bool,
    /// Show names starting with a dot.
    pub show_hidden: bool,
    /// Visible files left out because they are not image-sized.
    pub filtered_files: usize,
    pub error: Option<String>,
}

impl Browser {
    /// Lists `start`, falling back to the working directory and then `/` if
    /// it cannot be read. Selects `select` when it is in the listing.
    pub fn new(start: PathBuf, select: Option<&Path>) -> Self {
        let mut browser = Self {
            dir: start.clone(),
            entries: Vec::new(),
            selected: 0,
            show_all: false,
            show_hidden: false,
            filtered_files: 0,
            error: None,
        };
        let fallbacks = [Some(start), std::env::current_dir().ok(), Some(PathBuf::from("/"))];
        let mut first_error = None;
        for dir in fallbacks.into_iter().flatten() {
            match browser.load(dir, select) {
                Ok(()) => break,
                Err(err) => {
                    first_error.get_or_insert(err);
                }
            }
        }
        if browser.entries.is_empty() {
            browser.error = first_error;
        }
        browser
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    pub fn move_by(&mut self, delta: isize) {
        let last = self.entries.len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    pub fn enter(&mut self, dir: PathBuf) {
        let _ = self.load(dir, None);
    }

    pub fn parent(&mut self) {
        let current = self.dir.clone();
        if let Some(parent) = current.parent() {
            let _ = self.load(parent.to_path_buf(), Some(&current));
        }
    }

    pub fn home(&mut self) {
        match std::env::var_os("HOME") {
            Some(home) => {
                let _ = self.load(PathBuf::from(home), None);
            }
            None => self.error = Some("HOME is not set.".into()),
        }
    }

    pub fn toggle_all(&mut self) {
        self.show_all = !self.show_all;
        self.reload();
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.reload();
    }

    /// Re-reads the current directory, keeping the selection on the same name.
    fn reload(&mut self) {
        let keep = self.selected_entry().map(|e| e.path.clone());
        let _ = self.load(self.dir.clone(), keep.as_deref());
    }

    /// Replaces the listing with `dir`. On failure the current listing stays
    /// and the error is shown.
    fn load(&mut self, dir: PathBuf, select: Option<&Path>) -> Result<(), String> {
        let (entries, filtered) = match self.list(&dir) {
            Ok(listing) => listing,
            Err(err) => {
                let message = format!("Cannot open folder {}: {err}", dir.display());
                self.error = Some(message.clone());
                return Err(message);
            }
        };
        self.selected = select
            .and_then(|want| entries.iter().position(|e| e.path == want))
            .or_else(|| entries.iter().position(Entry::is_image))
            .unwrap_or(0);
        self.dir = dir;
        self.entries = entries;
        self.filtered_files = filtered;
        self.error = None;
        Ok(())
    }

    fn list(&self, dir: &Path) -> io::Result<(Vec<Entry>, usize)> {
        let mut entries = Vec::new();
        let mut filtered = 0;
        for item in fs::read_dir(dir)? {
            let item = item?;
            let name = item.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && !self.show_hidden {
                continue;
            }
            let path = item.path();
            // Follow symlinks so linked folders and images behave like the real thing.
            let kind = match fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => EntryKind::Dir,
                Ok(meta) => EntryKind::File { size: Some(meta.len()) },
                Err(_) => EntryKind::File { size: None },
            };
            let entry = Entry { name, path, kind };
            if matches!(kind, EntryKind::File { .. }) && !entry.is_image() && !self.show_all {
                filtered += 1;
                continue;
            }
            entries.push(entry);
        }
        entries.sort_by(compare);
        if let Some(parent) = dir.parent() {
            entries.insert(
                0,
                Entry { name: "..".into(), path: parent.to_path_buf(), kind: EntryKind::Parent },
            );
        }
        Ok((entries, filtered))
    }
}

/// Folders first, then files, each by case-insensitive name.
fn compare(a: &Entry, b: &Entry) -> Ordering {
    let rank = |e: &Entry| matches!(e.kind, EntryKind::File { .. }) as u8;
    rank(a)
        .cmp(&rank(b))
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.name.cmp(&b.name))
}

/// `12,032 B`, `1.5 MB`, …
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1_000_000 {
        let digits = bytes.to_string();
        let mut grouped = String::new();
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                grouped.push(',');
            }
            grouped.push(c);
        }
        return format!("{grouped} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::TempDir;

    fn names(browser: &Browser) -> Vec<&str> {
        browser.entries.iter().map(|e| e.name.as_str()).collect()
    }

    fn sample_tree() -> TempDir {
        let tmp = TempDir::new("browser");
        let root = &tmp.0;
        fs::create_dir(root.join("Sub")).unwrap();
        fs::create_dir(root.join("alpha")).unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join("b-image.bin"), vec![0; IMAGE_LEN]).unwrap();
        fs::write(root.join("A-image.dump"), vec![0; IMAGE_LEN]).unwrap();
        fs::write(root.join(".hidden.bin"), vec![0; IMAGE_LEN]).unwrap();
        fs::write(root.join("short.bin"), [0; 100]).unwrap();
        fs::write(root.join("notes.txt"), b"hi").unwrap();
        fs::write(root.join("Sub").join("inner.bin"), vec![0; IMAGE_LEN]).unwrap();
        tmp
    }

    #[test]
    fn lists_folders_then_images_and_counts_the_rest() {
        let tmp = sample_tree();
        let browser = Browser::new(tmp.0.clone(), None);
        assert_eq!(browser.error, None);
        assert_eq!(names(&browser), ["..", "alpha", "Sub", "A-image.dump", "b-image.bin"]);
        assert_eq!(browser.filtered_files, 2);
        // The first image is preselected.
        assert_eq!(browser.selected_entry().unwrap().name, "A-image.dump");
        assert!(browser.entries[3].is_image());
        assert_eq!(browser.entries[1].kind, EntryKind::Dir);
        assert_eq!(browser.entries[0].kind, EntryKind::Parent);
    }

    #[test]
    fn toggles_show_other_and_hidden_files_and_keep_the_selection() {
        let tmp = sample_tree();
        let mut browser = Browser::new(tmp.0.clone(), Some(&tmp.0.join("b-image.bin")));
        assert_eq!(browser.selected_entry().unwrap().name, "b-image.bin");
        browser.toggle_all();
        assert_eq!(
            names(&browser),
            ["..", "alpha", "Sub", "A-image.dump", "b-image.bin", "notes.txt", "short.bin"]
        );
        assert_eq!(browser.filtered_files, 0);
        assert_eq!(browser.selected_entry().unwrap().name, "b-image.bin");
        assert!(!browser.entries[6].is_image());
        browser.toggle_hidden();
        assert!(names(&browser).contains(&".git"));
        assert!(names(&browser).contains(&".hidden.bin"));
        browser.toggle_all();
        browser.toggle_hidden();
        assert_eq!(names(&browser), ["..", "alpha", "Sub", "A-image.dump", "b-image.bin"]);
    }

    #[test]
    fn enters_folders_and_returns_to_the_parent_with_it_selected() {
        let tmp = sample_tree();
        let mut browser = Browser::new(tmp.0.clone(), None);
        browser.enter(tmp.0.join("Sub"));
        assert_eq!(names(&browser), ["..", "inner.bin"]);
        browser.parent();
        assert_eq!(browser.dir, tmp.0);
        assert_eq!(browser.selected_entry().unwrap().name, "Sub");
    }

    #[test]
    fn unreadable_folder_keeps_the_current_listing() {
        let tmp = sample_tree();
        let mut browser = Browser::new(tmp.0.clone(), None);
        browser.enter(tmp.0.join("missing"));
        assert!(browser.error.as_deref().unwrap().starts_with("Cannot open folder"));
        assert_eq!(browser.dir, tmp.0);
        assert_eq!(browser.entries.len(), 5);
    }

    #[test]
    fn missing_start_folder_falls_back_to_the_working_directory() {
        let browser = Browser::new(PathBuf::from("/no/such/folder/anywhere"), None);
        assert_eq!(browser.dir, std::env::current_dir().unwrap());
        assert_eq!(browser.error, None);
    }

    #[test]
    fn selection_is_clamped() {
        let tmp = sample_tree();
        let mut browser = Browser::new(tmp.0.clone(), None);
        browser.move_by(-100);
        assert_eq!(browser.selected, 0);
        browser.move_by(100);
        assert_eq!(browser.selected, 4);
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(100), "100 B");
        assert_eq!(format_size(12_032), "12,032 B");
        assert_eq!(format_size(999_999), "999,999 B");
        assert_eq!(format_size(1_500_000), "1.5 MB");
        assert_eq!(format_size(2_000_000_000), "2.0 GB");
    }
}

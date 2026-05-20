use std::{
    fmt, fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use rayon::prelude::*;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Default)]
pub enum SizeMode {
    #[default]
    Physical,
    #[cfg(test)]
    Logical,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct EntryNode {
    pub path: PathBuf,
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub direct_size: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub error_count: u64,
    pub children: Vec<EntryNode>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScanReport {
    pub root: EntryNode,
    pub size_mode: String,
}

#[derive(Clone, Debug)]
pub struct ControlledScan {
    pub root: Option<EntryNode>,
    pub canceled: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ScanProgressSnapshot {
    pub scanned: u64,
    pub files: u64,
    pub dirs: u64,
    pub errors: u64,
    pub current_path: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct ScanControl {
    canceled: Arc<AtomicBool>,
    progress: Arc<ScanProgressState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCanceled;

impl fmt::Display for ScanCanceled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("scan canceled")
    }
}

impl std::error::Error for ScanCanceled {}

#[derive(Debug, Default)]
struct ScanProgressState {
    scanned: AtomicU64,
    files: AtomicU64,
    dirs: AtomicU64,
    errors: AtomicU64,
    current_path: Mutex<PathBuf>,
}

impl ScanControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.canceled.store(true, Ordering::Relaxed);
    }

    pub fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> ScanProgressSnapshot {
        ScanProgressSnapshot {
            scanned: self.progress.scanned.load(Ordering::Relaxed),
            files: self.progress.files.load(Ordering::Relaxed),
            dirs: self.progress.dirs.load(Ordering::Relaxed),
            errors: self.progress.errors.load(Ordering::Relaxed),
            current_path: self
                .progress
                .current_path
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .clone(),
        }
    }

    fn record_visit(&self, path: &Path, kind: Option<&EntryKind>) {
        self.progress.scanned.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut current_path) = self.progress.current_path.lock() {
            *current_path = path.to_path_buf();
        }

        match kind {
            Some(EntryKind::Directory) => {
                self.progress.dirs.fetch_add(1, Ordering::Relaxed);
            }
            Some(EntryKind::File) | Some(EntryKind::Symlink) => {
                self.progress.files.fetch_add(1, Ordering::Relaxed);
            }
            Some(EntryKind::Error) => {
                self.progress.errors.fetch_add(1, Ordering::Relaxed);
            }
            Some(EntryKind::Other) | None => {}
        }
    }

    fn record_errors(&self, count: u64) {
        self.progress.errors.fetch_add(count, Ordering::Relaxed);
    }
}

pub fn scan(root: impl AsRef<Path>, size_mode: SizeMode) -> EntryNode {
    let root = normalize_root(root.as_ref());
    scan_path(root, size_mode, None)
        .root
        .expect("uncontrolled scans cannot be canceled before producing a root")
}

pub fn scan_controlled(
    root: impl AsRef<Path>,
    size_mode: SizeMode,
    control: &ScanControl,
) -> Result<EntryNode, ScanCanceled> {
    let outcome = scan_controlled_partial(root, size_mode, control);
    if outcome.canceled {
        Err(ScanCanceled)
    } else {
        outcome.root.ok_or(ScanCanceled)
    }
}

pub fn scan_controlled_partial(
    root: impl AsRef<Path>,
    size_mode: SizeMode,
    control: &ScanControl,
) -> ControlledScan {
    let root = normalize_root(root.as_ref());
    scan_path(root, size_mode, Some(control))
}

pub fn human_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = size as f64;
    let mut unit = 0;

    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", size, UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn flatten<'a>(node: &'a EntryNode, out: &mut Vec<&'a EntryNode>) {
    out.push(node);
    for child in &node.children {
        flatten(child, out);
    }
}

pub fn find_by_path<'a>(node: &'a EntryNode, path: &Path) -> Option<&'a EntryNode> {
    if node.path == path {
        return Some(node);
    }

    for child in &node.children {
        if let Some(found) = find_by_path(child, path) {
            return Some(found);
        }
    }

    None
}

pub fn sorted_children<'a>(
    node: &'a EntryNode,
    sort: SortKey,
    reversed: bool,
    filter: &str,
) -> Vec<&'a EntryNode> {
    let filter = filter.trim().to_lowercase();
    let mut children: Vec<_> = node
        .children
        .iter()
        .filter(|child| {
            filter.is_empty()
                || child.name.to_lowercase().contains(&filter)
                || child
                    .path
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&filter)
        })
        .collect();

    match sort {
        SortKey::Size => children.sort_by(|a, b| {
            b.size
                .cmp(&a.size)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        SortKey::Name => children.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| b.size.cmp(&a.size))
        }),
        SortKey::Kind => children.sort_by(|a, b| {
            kind_rank(&a.kind)
                .cmp(&kind_rank(&b.kind))
                .then_with(|| b.size.cmp(&a.size))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
    }

    if reversed {
        children.reverse();
    }

    children
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Size,
    Name,
    Kind,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Size => "size",
            SortKey::Name => "name",
            SortKey::Kind => "kind",
        }
    }
}

fn scan_path(path: PathBuf, size_mode: SizeMode, control: Option<&ScanControl>) -> ControlledScan {
    if control.is_some_and(ScanControl::is_canceled) {
        return ControlledScan {
            root: None,
            canceled: true,
        };
    }

    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) => {
            if let Some(control) = control {
                control.record_visit(&path, Some(&EntryKind::Error));
            }

            return ControlledScan {
                root: Some(EntryNode {
                    name: display_name(&path),
                    path,
                    kind: EntryKind::Error,
                    size: 0,
                    direct_size: 0,
                    file_count: 0,
                    dir_count: 0,
                    error_count: 1,
                    children: Vec::new(),
                    error: Some(err.to_string()),
                }),
                canceled: false,
            };
        }
    };

    let file_type = metadata.file_type();
    let direct_size = file_size(&metadata, size_mode);

    if file_type.is_dir() {
        if let Some(control) = control {
            control.record_visit(&path, Some(&EntryKind::Directory));
        }

        let mut read_errors = 0;
        let children = match fs::read_dir(&path) {
            Ok(entries) => entries
                .filter_map(|entry| match entry {
                    Ok(entry) => Some(entry.path()),
                    Err(_) => {
                        read_errors += 1;
                        None
                    }
                })
                .collect::<Vec<_>>(),
            Err(err) => {
                if let Some(control) = control {
                    control.record_errors(1);
                }

                return ControlledScan {
                    root: Some(EntryNode {
                        name: display_name(&path),
                        path,
                        kind: EntryKind::Directory,
                        size: direct_size,
                        direct_size,
                        file_count: 0,
                        dir_count: 1,
                        error_count: 1,
                        children: Vec::new(),
                        error: Some(err.to_string()),
                    }),
                    canceled: false,
                };
            }
        };

        if let Some(control) = control {
            control.record_errors(read_errors);
        }

        let child_scans = children
            .into_par_iter()
            .map(|child| scan_path(child, size_mode, control))
            .collect::<Vec<_>>();
        let canceled = child_scans.iter().any(|child| child.canceled);
        let mut children = child_scans
            .into_iter()
            .filter_map(|child| child.root)
            .collect::<Vec<_>>();

        children.sort_by(|a, b| {
            b.size
                .cmp(&a.size)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        let child_size = children.iter().map(|child| child.size).sum::<u64>();
        let file_count = children.iter().map(|child| child.file_count).sum::<u64>();
        let dir_count = 1 + children.iter().map(|child| child.dir_count).sum::<u64>();
        let error_count = read_errors + children.iter().map(|child| child.error_count).sum::<u64>();

        ControlledScan {
            root: Some(EntryNode {
                name: display_name(&path),
                path,
                kind: EntryKind::Directory,
                size: direct_size + child_size,
                direct_size,
                file_count,
                dir_count,
                error_count,
                children,
                error: None,
            }),
            canceled,
        }
    } else {
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };

        if let Some(control) = control {
            control.record_visit(&path, Some(&kind));
        }

        ControlledScan {
            root: Some(EntryNode {
                name: display_name(&path),
                path,
                kind,
                size: direct_size,
                direct_size,
                file_count: u64::from(file_type.is_file() || file_type.is_symlink()),
                dir_count: 0,
                error_count: 0,
                children: Vec::new(),
                error: None,
            }),
            canceled: false,
        }
    }
}

fn normalize_root(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

fn file_size(metadata: &fs::Metadata, size_mode: SizeMode) -> u64 {
    match size_mode {
        SizeMode::Physical => physical_size(metadata),
        #[cfg(test)]
        SizeMode::Logical => metadata.len(),
    }
}

#[cfg(unix)]
fn physical_size(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn physical_size(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

fn kind_rank(kind: &EntryKind) -> u8 {
    match kind {
        EntryKind::Directory => 0,
        EntryKind::File => 1,
        EntryKind::Symlink => 2,
        EntryKind::Other => 3,
        EntryKind::Error => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_human_sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(10 * 1024 * 1024), "10 MB");
    }

    #[test]
    fn scans_and_sorts_children_by_size() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("small.txt"), b"small").unwrap();
        fs::write(dir.path().join("large.txt"), vec![0; 4096]).unwrap();

        let root = scan(dir.path(), SizeMode::Logical);
        let names = root
            .children
            .iter()
            .map(|child| child.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["large.txt", "small.txt"]);
        assert_eq!(root.file_count, 2);
    }

    #[test]
    fn filters_sorted_children_by_name_or_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("movie.iso"), vec![0; 100]).unwrap();
        fs::write(dir.path().join("notes.txt"), vec![0; 200]).unwrap();

        let root = scan(dir.path(), SizeMode::Logical);
        let filtered = sorted_children(&root, SortKey::Size, true, "movie");

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "movie.iso");
    }

    #[test]
    fn controlled_scan_reports_progress() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one.txt"), b"one").unwrap();

        let control = ScanControl::new();
        scan_controlled(dir.path(), SizeMode::Logical, &control).unwrap();
        let snapshot = control.snapshot();

        assert!(snapshot.scanned >= 2);
        assert_eq!(snapshot.files, 1);
        assert!(snapshot.dirs >= 1);
    }

    #[test]
    fn controlled_scan_can_be_canceled_before_start() {
        let dir = tempfile::tempdir().unwrap();
        let control = ScanControl::new();
        control.cancel();

        let result = scan_controlled(dir.path(), SizeMode::Logical, &control);

        assert_eq!(result.unwrap_err(), ScanCanceled);
    }

    #[test]
    fn controlled_partial_scan_keeps_scanned_root_after_cancel() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one.txt"), b"one").unwrap();
        fs::write(dir.path().join("two.txt"), b"two").unwrap();

        let control = ScanControl::new();
        control.cancel();
        let result = scan_controlled_partial(dir.path(), SizeMode::Logical, &control);

        assert!(result.canceled);
        assert!(result.root.is_none());

        let control = ScanControl::new();
        let result = scan_controlled_partial(dir.path(), SizeMode::Logical, &control);

        assert!(!result.canceled);
        assert_eq!(result.root.unwrap().file_count, 2);
    }
}

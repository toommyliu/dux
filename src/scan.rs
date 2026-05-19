use std::{
    fs,
    path::{Path, PathBuf},
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

pub fn scan(root: impl AsRef<Path>, size_mode: SizeMode) -> EntryNode {
    let root = normalize_root(root.as_ref());
    scan_path(root, size_mode)
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

fn scan_path(path: PathBuf, size_mode: SizeMode) -> EntryNode {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) => {
            return EntryNode {
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
            };
        }
    };

    let file_type = metadata.file_type();
    let direct_size = file_size(&metadata, size_mode);

    if file_type.is_dir() {
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
                return EntryNode {
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
                };
            }
        };

        let mut children: Vec<_> = children
            .into_par_iter()
            .map(|child| scan_path(child, size_mode))
            .collect();

        children.sort_by(|a, b| {
            b.size
                .cmp(&a.size)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        let child_size = children.iter().map(|child| child.size).sum::<u64>();
        let file_count = children.iter().map(|child| child.file_count).sum::<u64>();
        let dir_count = 1 + children.iter().map(|child| child.dir_count).sum::<u64>();
        let error_count = read_errors + children.iter().map(|child| child.error_count).sum::<u64>();

        EntryNode {
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
        }
    } else {
        EntryNode {
            name: display_name(&path),
            path,
            kind: if file_type.is_symlink() {
                EntryKind::Symlink
            } else if file_type.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            },
            size: direct_size,
            direct_size,
            file_count: u64::from(file_type.is_file() || file_type.is_symlink()),
            dir_count: 0,
            error_count: 0,
            children: Vec::new(),
            error: None,
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
}

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

pub fn trash_path(path: &Path) -> Result<()> {
    trash::delete(path).with_context(|| format!("failed to move {} to trash", path.display()))
}

pub fn delete_permanently(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;

    if metadata.is_dir() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to delete directory {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("failed to delete file {}", path.display()))
    }
}

pub fn root_paths(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort_by(|a, b| {
        a.components()
            .count()
            .cmp(&b.components().count())
            .then_with(|| a.cmp(b))
    });

    let mut roots = Vec::new();
    for path in paths {
        if !roots.iter().any(|root: &PathBuf| path.starts_with(root)) {
            roots.push(path);
        }
    }

    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permanently_deletes_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delete-me.txt");
        fs::write(&path, b"delete me").unwrap();

        delete_permanently(&path).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn permanently_deletes_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delete-me");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("nested.txt"), b"delete me").unwrap();

        delete_permanently(&path).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn root_paths_collapses_nested_targets() {
        let paths = root_paths(vec![
            PathBuf::from("/tmp/a/b"),
            PathBuf::from("/tmp/a"),
            PathBuf::from("/tmp/c"),
        ]);

        assert_eq!(
            paths,
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/c")]
        );
    }
}

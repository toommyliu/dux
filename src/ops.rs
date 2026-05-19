use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
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

pub fn open_in_file_manager(path: &Path) -> Result<()> {
    let is_dir = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .is_dir();
    let invocation = file_manager_invocation(path, is_dir);

    Command::new(&invocation.program)
        .args(&invocation.args)
        .spawn()
        .with_context(|| format!("failed to open {} in file manager", path.display()))?;

    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileManagerInvocation {
    program: &'static str,
    args: Vec<String>,
}

fn file_manager_invocation(path: &Path, _is_dir: bool) -> FileManagerInvocation {
    #[cfg(target_os = "macos")]
    {
        FileManagerInvocation {
            program: "open",
            args: vec!["-R".to_string(), path.display().to_string()],
        }
    }

    #[cfg(target_os = "windows")]
    {
        FileManagerInvocation {
            program: "explorer",
            args: vec![format!("/select,{}", path.display())],
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = if _is_dir {
            path.to_path_buf()
        } else {
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        };

        FileManagerInvocation {
            program: "xdg-open",
            args: vec![target.display().to_string()],
        }
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

    #[test]
    fn file_manager_invocation_reveals_paths() {
        let path = PathBuf::from("/tmp/example/file.txt");
        let invocation = file_manager_invocation(&path, false);

        #[cfg(target_os = "macos")]
        assert_eq!(
            invocation,
            FileManagerInvocation {
                program: "open",
                args: vec!["-R".to_string(), "/tmp/example/file.txt".to_string()]
            }
        );

        #[cfg(target_os = "windows")]
        assert_eq!(
            invocation,
            FileManagerInvocation {
                program: "explorer",
                args: vec!["/select,/tmp/example/file.txt".to_string()]
            }
        );

        #[cfg(all(unix, not(target_os = "macos")))]
        assert_eq!(
            invocation,
            FileManagerInvocation {
                program: "xdg-open",
                args: vec!["/tmp/example".to_string()]
            }
        );
    }
}

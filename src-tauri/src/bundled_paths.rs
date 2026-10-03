//! Finding files that ship next to the app (SoX, ONNX Runtime, preset voice clips).
//! The installed app, the portable zip and `tauri dev` each put them in a slightly
//! different place relative to the executable, so every lookup tries all of them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The directories searched for bundled files: the three levels above the executable
/// (installed layout, `target/release`, `target/debug`), then the working directory.
pub(crate) fn search_roots() -> Vec<PathBuf> {
    roots_for(std::env::current_exe().ok().as_deref(), std::env::current_dir().ok())
}

fn roots_for(exe: Option<&Path>, cwd: Option<PathBuf>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(exe) = exe {
        roots.extend(exe.ancestors().skip(1).take(3).map(Path::to_path_buf));
    }
    roots.extend(cwd);
    let mut seen: HashSet<PathBuf> = HashSet::new();
    roots.retain(|root| seen.insert(root.clone()));
    roots
}

/// Looks for `<root>/binaries/<dir>/<file>`, `<root>/resources/binaries/<dir>/<file>` and
/// `<root>/<dir>/<file>` under each root, plus any `extra` paths relative to the root.
/// Roots are tried in order, and within a root the standard layouts come first.
pub(crate) fn find_bundled_file(roots: &[PathBuf], dir: &str, file: &str, extra: &[String]) -> Option<PathBuf> {
    roots
        .iter()
        .flat_map(|root| {
            [
                root.join("binaries").join(dir).join(file),
                root.join("resources").join("binaries").join(dir).join(file),
                root.join(dir).join(file),
            ]
            .into_iter()
            .chain(extra.iter().map(move |relative| root.join(relative)))
        })
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("voicereader-bundled-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn finds_each_standard_layout() {
        for relative in [
            "binaries/tool/tool.bin",
            "resources/binaries/tool/tool.bin",
            "tool/tool.bin",
        ] {
            let root = temp_root("layout");
            touch(&root.join(relative));
            let found = find_bundled_file(&[root.clone()], "tool", "tool.bin", &[]);
            assert_eq!(found, Some(root.join(relative)));
            std::fs::remove_dir_all(&root).ok();
        }
    }

    #[test]
    fn earlier_roots_win_and_missing_files_are_skipped() {
        let first = temp_root("first");
        let second = temp_root("second");
        let empty = temp_root("empty");
        touch(&first.join("tool").join("tool.bin"));
        touch(&second.join("tool").join("tool.bin"));
        let roots = [empty.clone(), first.clone(), second.clone()];
        assert_eq!(
            find_bundled_file(&roots, "tool", "tool.bin", &[]),
            Some(first.join("tool").join("tool.bin"))
        );
        assert_eq!(find_bundled_file(&[empty.clone()], "tool", "tool.bin", &[]), None);
        assert_eq!(find_bundled_file(&roots, "tool", "other.bin", &[]), None);
        for root in [first, second, empty] {
            std::fs::remove_dir_all(&root).ok();
        }
    }

    #[test]
    fn extra_layouts_are_found_but_directories_do_not_count() {
        let root = temp_root("extra");
        let extra = vec!["binaries/tool.bin".to_string(), "tool.bin".to_string()];
        // A directory with the file's name is not a hit.
        std::fs::create_dir_all(root.join("tool.bin")).unwrap();
        assert_eq!(find_bundled_file(&[root.clone()], "tool", "tool.bin", &extra), None);
        std::fs::remove_dir_all(root.join("tool.bin")).unwrap();

        touch(&root.join("binaries").join("tool.bin"));
        assert_eq!(
            find_bundled_file(&[root.clone()], "tool", "tool.bin", &extra),
            Some(root.join("binaries").join("tool.bin"))
        );
        // The standard layouts take precedence over the extras within a root.
        touch(&root.join("tool").join("tool.bin"));
        assert_eq!(
            find_bundled_file(&[root.clone()], "tool", "tool.bin", &extra),
            Some(root.join("tool").join("tool.bin"))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn roots_are_the_three_parent_directories_then_cwd_without_duplicates() {
        let exe = Path::new("/a/b/c/d/app.exe");
        let roots = roots_for(Some(exe), Some(PathBuf::from("/x")));
        assert_eq!(
            roots,
            [PathBuf::from("/a/b/c/d"), PathBuf::from("/a/b/c"), PathBuf::from("/a/b"), PathBuf::from("/x")]
        );
        let roots = roots_for(Some(exe), Some(PathBuf::from("/a/b/c")));
        assert_eq!(roots, [PathBuf::from("/a/b/c/d"), PathBuf::from("/a/b/c"), PathBuf::from("/a/b")]);
        assert_eq!(roots_for(None, Some(PathBuf::from("/x"))), [PathBuf::from("/x")]);
        assert!(roots_for(None, None).is_empty());
    }
}

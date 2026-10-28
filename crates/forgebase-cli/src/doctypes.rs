//! Loading the DocType files of a directory into a registry.

use std::fs;
use std::path::{Path, PathBuf};

use forgebase_core::meta::{DocType, Registry};

/// Loads every `*.toml` file directly in `dir` into a registry.
///
/// Rules: not recursive (subdirectories are ignored); the extension match is case-sensitive
/// (`*.TOML` is ignored); names starting with `.` are skipped (editor and macOS `._` files);
/// files are processed sorted by name; an entry named `*.toml` that is not a readable regular
/// file (for example a dangling symlink) is reported as a problem; a directory with such a name
/// is ignored. Symlinks to files are followed.
///
/// Every problem is reported, each prefixed with its file name: unreadable files, invalid
/// definitions and duplicate DocType names.
pub fn load_dir(dir: &Path) -> Result<Registry, Vec<String>> {
    let entries = fs::read_dir(dir).map_err(|e| {
        vec![format!(
            "cannot read DocType directory {}: {e}",
            dir.display()
        )]
    })?;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut problems = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|e| {
                vec![format!(
                    "cannot read DocType directory {}: {e}",
                    dir.display()
                )]
            })?
            .path();
        let hidden = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'));
        if hidden || path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        if path.is_file() {
            files.push(path);
        } else if path.is_dir() {
            // A directory named `x.toml` is not a DocType file.
        } else {
            // For example a dangling symlink: do not skip it silently.
            problems.push(format!(
                "{}: cannot read: not a readable file (dangling symlink?)",
                path.display()
            ));
        }
    }
    files.sort();

    let mut registry = Registry::new();
    for path in &files {
        let shown = path.display();
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                problems.push(format!("{shown}: cannot read: {e}"));
                continue;
            }
        };
        match DocType::from_toml_str(&text) {
            Err(e) => problems.push(format!("{shown}: {e}")),
            Ok(doctype) => {
                if let Err(errors) = registry.add(doctype) {
                    for e in errors {
                        problems.push(format!("{shown}: {e}"));
                    }
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(registry)
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "name = \"{n}\"\nlabel = \"L\"\n[[fields]]\nname = \"f\"\ntype = \"text\"\n";

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forgebase-cli-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn good(name: &str) -> String {
        GOOD.replace("{n}", name)
    }

    #[test]
    fn loads_toml_files_and_ignores_everything_else() {
        let dir = scratch("ok");
        fs::write(dir.join("b.toml"), good("beta")).unwrap();
        fs::write(dir.join("a.toml"), good("alpha")).unwrap();
        fs::write(dir.join("notes.md"), "not a doctype").unwrap();
        fs::write(dir.join("broken.toml.bak"), "garbage").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("c.toml"), good("gamma")).unwrap();
        fs::create_dir(dir.join("dir.toml")).unwrap();

        let registry = load_dir(&dir).unwrap();
        let names: Vec<&str> = registry.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn errors_name_the_file_and_are_all_reported() {
        let dir = scratch("bad");
        fs::write(dir.join("good.toml"), good("good")).unwrap();
        fs::write(dir.join("bad.toml"), "name = ").unwrap();
        fs::write(dir.join("dup.toml"), good("good")).unwrap();
        fs::write(dir.join("invalid.toml"), good("Bad Name")).unwrap();

        let problems = load_dir(&dir).unwrap_err();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("bad.toml")));
        assert!(problems.iter().any(|p| p.contains("invalid.toml")));
        assert!(problems.iter().any(|p| p.contains("good.toml")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hidden_files_are_skipped_and_the_extension_is_case_sensitive() {
        let dir = scratch("hidden");
        fs::write(dir.join("a.toml"), good("alpha")).unwrap();
        fs::write(dir.join(".hidden.toml"), "garbage").unwrap();
        fs::write(dir.join("._a.toml"), "\u{0}\u{5}binary").unwrap();
        fs::write(dir.join("UPPER.TOML"), "garbage").unwrap();

        let registry = load_dir(&dir).unwrap();
        let names: Vec<&str> = registry.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["alpha"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_is_reported_with_its_path() {
        let dir = scratch("dangling");
        fs::write(dir.join("a.toml"), good("alpha")).unwrap();
        std::os::unix::fs::symlink(dir.join("missing-target"), dir.join("link.toml")).unwrap();
        // A symlink to a real file is followed.
        fs::write(dir.join("real.txt"), good("beta")).unwrap();
        std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("ok.toml")).unwrap();

        let problems = load_dir(&dir).unwrap_err();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("link.toml"), "{problems:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_directory_is_an_error() {
        let problems = load_dir(Path::new("/nonexistent/forgebase/doctypes")).unwrap_err();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("cannot read DocType directory"));
    }

    #[test]
    fn empty_directory_gives_an_empty_registry() {
        let dir = scratch("empty");
        assert!(load_dir(&dir).unwrap().is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}

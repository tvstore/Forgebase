//! Reading the DocType files of a directory (`--doctypes DIR`, and the guard of
//! [`testing`](crate::testing)).

use std::fs;
use std::path::{Path, PathBuf};

/// The `*.toml` files directly in `dir`, sorted by name, and the problems found on the way.
///
/// Rules: not recursive (subdirectories are ignored); the extension match is case-sensitive
/// (`*.TOML` is ignored); names starting with `.` are skipped (editor and macOS `._` files);
/// an entry named `*.toml` that is not a readable regular file (for example a dangling symlink)
/// is reported as a problem; a directory with such a name is ignored. Symlinks to files are
/// followed. `Err` when the directory cannot be read.
pub(crate) fn toml_files(dir: &Path) -> Result<(Vec<PathBuf>, Vec<String>), String> {
    let unreadable =
        |e: std::io::Error| format!("cannot read DocType directory {}: {e}", dir.display());
    let entries = fs::read_dir(dir).map_err(unreadable)?;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut problems = Vec::new();
    for entry in entries {
        let path = entry.map_err(unreadable)?.path();
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
    Ok((files, problems))
}

/// One DocType file read from disk: the path as shown in messages and the text.
pub(crate) struct Source {
    pub path: String,
    pub text: String,
}

/// Reads every `*.toml` file of `dir` ([`toml_files`] has the rules). Every problem is
/// returned, each naming its file; the readable files are returned whatever the problems.
pub(crate) fn read_dir(dir: &Path) -> (Vec<Source>, Vec<String>) {
    let (files, mut problems) = match toml_files(dir) {
        Ok(found) => found,
        Err(problem) => return (Vec::new(), vec![problem]),
    };
    let mut sources = Vec::new();
    for path in files {
        let shown = path.display().to_string();
        match fs::read_to_string(&path) {
            Ok(text) => sources.push(Source { path: shown, text }),
            Err(e) => problems.push(format!("{shown}: cannot read: {e}")),
        }
    }
    (sources, problems)
}

#[cfg(test)]
mod tests {
    use forgebase_core::meta::Registry;

    use super::*;
    use crate::compose::compose;

    /// The DocTypes of one directory, loaded as the ad-hoc app `local`.
    fn load_dir(dir: &Path) -> Result<Registry, Vec<String>> {
        match compose(&[], Some(dir)) {
            Ok(composed) => Ok(composed.registry),
            Err(error) => Err(error.problems().map(str::to_owned).collect()),
        }
    }

    const GOOD: &str = "name = \"{n}\"\nlabel = \"L\"\n[[fields]]\nname = \"f\"\ntype = \"text\"\n";

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forgebase-test-{tag}-{}", std::process::id()));
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

    fn linking(name: &str, target: &str) -> String {
        format!(
            "name = \"{name}\"\nlabel = \"L\"\n[[fields]]\nname = \"f\"\ntype = \"link\"\ntarget = \"{target}\"\n"
        )
    }

    #[test]
    fn links_between_files_load_in_any_order_and_unknown_targets_name_the_file() {
        let dir = scratch("links");
        // `a` links to `b`, which is loaded later; `b` links to itself.
        fs::write(dir.join("a.toml"), linking("a", "b")).unwrap();
        fs::write(dir.join("b.toml"), linking("b", "b")).unwrap();
        assert_eq!(load_dir(&dir).unwrap().len(), 2);

        fs::write(dir.join("c.toml"), linking("c", "nowhere")).unwrap();
        let problems = load_dir(&dir).unwrap_err();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("c.toml")
                && problems[0].contains("field `f` links to unknown doctype `nowhere`"),
            "{problems:?}"
        );

        // A target in a file that failed to load is not reported again.
        fs::write(dir.join("d.toml"), "name = ").unwrap();
        fs::write(dir.join("c.toml"), linking("c", "d")).unwrap();
        let problems = load_dir(&dir).unwrap_err();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("d.toml"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn child_table_problems_name_the_file() {
        let dir = scratch("children");
        let parent = |name: &str, target: &str| {
            format!(
                "name = \"{name}\"\nlabel = \"P\"\n[[fields]]\nname = \"rows\"\ntype = \"table\"\ntarget = \"{target}\"\n"
            )
        };
        let child = "name = \"row\"\nlabel = \"R\"\nis_child = true\n";
        fs::write(dir.join("p.toml"), parent("p", "row")).unwrap();
        fs::write(dir.join("row.toml"), child).unwrap();
        assert_eq!(load_dir(&dir).unwrap().len(), 2);

        // Used twice: reported once, with the child's file.
        fs::write(dir.join("q.toml"), parent("q", "row")).unwrap();
        let problems = load_dir(&dir).unwrap_err();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("row.toml")
                && problems[0].contains("used by more than one table field: p.rows, q.rows"),
            "{problems:?}"
        );

        // Unknown target and a target that is not a child, each with the parent's file.
        fs::remove_file(dir.join("q.toml")).unwrap();
        fs::write(dir.join("p.toml"), parent("p", "nowhere")).unwrap();
        let problems = load_dir(&dir).unwrap_err();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|p| p.contains("p.toml") && p.contains("unknown doctype `nowhere`")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("row.toml") && p.contains("not used by any table field"))
        );
        fs::write(dir.join("p.toml"), parent("p", "p")).unwrap();
        let problems = load_dir(&dir).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("p.toml") && p.contains("not a child doctype")),
            "{problems:?}"
        );
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

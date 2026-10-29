//! `forgebase new-app`: writes a small app crate that builds and passes its own tests.
//!
//! The files are in `templates/app/` and embedded with `include_str!`. They are plain text with
//! four placeholders that are replaced in one pass over the text (no template engine, and a
//! replaced value is never scanned again): `{{name}}`, `{{title}}` (the name as words, for labels
//! and the role), `{{forgebase_path}}` (the `crates/forgebase` directory of the checkout) and
//! `{{rust_version}}` (the MSRV of this Forgebase).

use std::fs;
use std::path::{Path, PathBuf};

use crate::compose::app_name_problems;
use crate::error::{CliError, bullets};

/// `(path in the new crate, template)`. A path may contain placeholders too.
const FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        include_str!("../templates/app/Cargo.toml.tpl"),
    ),
    (".gitignore", include_str!("../templates/app/.gitignore")),
    ("README.md", include_str!("../templates/app/README.md")),
    ("AGENTS.md", include_str!("../templates/app/AGENTS.md")),
    ("src/lib.rs", include_str!("../templates/app/src/lib.rs")),
    ("src/main.rs", include_str!("../templates/app/src/main.rs")),
    (
        "doctypes/{{name}}_note.toml",
        include_str!("../templates/app/doctypes/note.toml"),
    ),
    (
        "migrations/README.md",
        include_str!("../templates/app/migrations/README.md"),
    ),
    (
        "tests/app.rs",
        include_str!("../templates/app/tests/app.rs"),
    ),
];

/// Rust keywords: `<name>::app()` in the generated `src/main.rs` would not compile, and Cargo does
/// not refuse them as package names. The lowercase ones of the Rust reference ("Keywords": strict,
/// 2018+ and reserved; the weak keywords `union` and `macro_rules` are ordinary identifiers in
/// a path, so they are allowed). `Self` and the like cannot happen: app names are lowercase.
const KEYWORDS: &[&str] = &[
    // Strict.
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", // Strict since the 2018 edition, and `gen` (2024).
    "async", "await", "dyn", "gen", // Reserved for future use.
    "abstract", "become", "box", "do", "final", "macro", "override", "priv", "try", "typeof",
    "unsized", "virtual", "yield",
];

/// Names that cannot be the name of the crate: it would clash with a crate it uses or with the
/// standard library.
const RESERVED: &[&str] = &[
    "forgebase",
    "tokio",
    "std",
    "core",
    "alloc",
    "test",
    "proc_macro",
];

/// The longest app name: the sample DocType `<name>_note` must fit the DocType name limit.
const MAX_NAME_LEN: usize = 54;

/// Creates `<dir>/<name>/` and returns its path.
pub(crate) fn new_app(name: &str, forgebase_path: &Path, dir: &Path) -> Result<PathBuf, CliError> {
    let mut problems = app_name_problems(name);
    if problems.is_empty() && name.len() > MAX_NAME_LEN {
        problems.push(format!(
            "app name {name:?} is {} bytes long, the maximum is {MAX_NAME_LEN} (the sample DocType `{name}_note` must fit the 59 bytes of a DocType name)",
            name.len()
        ));
    }
    if KEYWORDS.contains(&name) {
        problems.push(format!(
            "app name `{name}` is a Rust keyword and cannot be the name of a crate here"
        ));
    }
    if RESERVED.contains(&name) {
        problems.push(format!(
            "app name `{name}` cannot be used as a crate name here"
        ));
    }
    if !problems.is_empty() {
        return Err(CliError::NewApp(format!(
            "invalid app name:\n{}",
            bullets(&problems)
        )));
    }

    let crate_dir = forgebase_crate_dir(forgebase_path)?;
    let target = dir.join(name);
    if target.symlink_metadata().is_ok() {
        return Err(CliError::NewApp(format!(
            "{} already exists; new-app never overwrites",
            target.display()
        )));
    }

    let values = [
        ("{{name}}", name.to_owned()),
        ("{{title}}", title(name)),
        ("{{forgebase_path}}", toml_escape(&crate_dir)?),
        (
            "{{rust_version}}",
            env!("CARGO_PKG_RUST_VERSION").to_owned(),
        ),
    ];
    let render = |text: &str| render(text, &values);

    // A missing `--dir` (and its missing parents) is created and stays when a later step fails:
    // only the directory of the app itself is removed again.
    fs::create_dir_all(dir)
        .map_err(|e| CliError::NewApp(format!("cannot create {}: {e}", dir.display())))?;
    generate(&target, &render, forgebase_path, FILES)?;
    Ok(target)
}

/// Creates `target` and writes `files` (and the lock file of the checkout) into it. From the
/// moment the directory exists it is ours: when anything fails it is removed again, so a failed
/// run leaves nothing half written.
fn generate(
    target: &Path,
    render: &dyn Fn(&str) -> String,
    checkout: &Path,
    files: &[(&str, &str)],
) -> Result<(), CliError> {
    fs::create_dir(target)
        .map_err(|e| CliError::NewApp(format!("cannot create {}: {e}", target.display())))?;
    let written = write_files(target, render, checkout, files);
    if written.is_err() {
        let _ = fs::remove_dir_all(target);
    }
    written
}

/// `text` with every known placeholder replaced, in one pass: the replaced values are not
/// scanned again (a path may contain `{{name}}`), and an unknown `{{` is left as it is.
fn render(text: &str, values: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("{{") {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match values.iter().find(|(key, _)| rest.starts_with(key)) {
            Some((key, value)) => {
                out.push_str(value);
                rest = &rest[key.len()..];
            }
            None => {
                out.push_str("{{");
                rest = &rest[2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// What the command prints: where the app is and what to do next.
pub(crate) fn report(created: &Path) -> String {
    let path = created.display();
    let quoted = shell_quote(&created.display().to_string());
    format!(
        "created {path}\n\nnext steps:\n  cd {quoted}\n  export FORGEBASE_TEST_DATABASE_URL=postgres://user:password@localhost:5432/postgres\n  cargo test\n\nREADME.md says how to migrate, serve and extend the app; AGENTS.md has the rules for whoever maintains it.\n"
    )
}

/// `text` as one shell word: unchanged when it has only safe characters, else in single quotes.
fn shell_quote(text: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_./-+:@%,=~".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

fn write_files(
    target: &Path,
    render: &dyn Fn(&str) -> String,
    checkout: &Path,
    files: &[(&str, &str)],
) -> Result<(), CliError> {
    let io = |path: &Path, error: std::io::Error| {
        CliError::NewApp(format!("cannot write {}: {error}", path.display()))
    };
    for (path, template) in files {
        let file = target.join(render(path));
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        fs::write(&file, render(template)).map_err(|e| io(&file, e))?;
    }
    // The versions Forgebase was tested with: the checkout's lock file. Cargo drops the entries
    // the app does not use and adds the app itself; see docs/guide/first-app.md.
    let lock = checkout.join("Cargo.lock");
    if lock.is_file() {
        let to = target.join("Cargo.lock");
        fs::copy(&lock, &to).map_err(|e| io(&to, e))?;
    }
    Ok(())
}

/// The `crates/forgebase` directory of the checkout, as the path to write in `Cargo.toml`: the
/// path as given when it is absolute, else made absolute (a relative path is relative to where
/// the command runs, not to the new crate).
fn forgebase_crate_dir(checkout: &Path) -> Result<PathBuf, CliError> {
    let manifest = checkout.join("crates/forgebase/Cargo.toml");
    let is_forgebase = fs::read_to_string(&manifest).is_ok_and(|text| {
        text.lines()
            .any(|line| line.trim() == "name = \"forgebase\"")
    });
    if !is_forgebase {
        return Err(CliError::NewApp(format!(
            "{} is not a Forgebase checkout: {} is missing or is not the forgebase crate",
            checkout.display(),
            manifest.display()
        )));
    }
    let root = if checkout.is_absolute() {
        checkout.to_path_buf()
    } else {
        fs::canonicalize(checkout)
            .map_err(|e| CliError::NewApp(format!("cannot resolve {}: {e}", checkout.display())))?
    };
    Ok(root.join("crates/forgebase"))
}

/// A path as the contents of a TOML basic string.
fn toml_escape(path: &Path) -> Result<String, CliError> {
    let text = path.to_str().ok_or_else(|| {
        CliError::NewApp(format!("the path {} is not valid UTF-8", path.display()))
    })?;
    if text.chars().any(char::is_control) {
        return Err(CliError::NewApp(format!(
            "the path {text:?} contains a control character"
        )));
    }
    Ok(text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `my_app` as `My App`, for labels and the name of the sample role.
fn title(name: &str) -> String {
    name.split('_')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        assert_eq!(title("demo"), "Demo");
        assert_eq!(title("my_app"), "My App");
        assert_eq!(title("a1_b2"), "A1 B2");
    }

    #[test]
    fn render_replaces_in_one_pass() {
        let values = [
            ("{{name}}", "a{{title}}b".to_owned()),
            ("{{title}}", "T".to_owned()),
        ];
        // A replaced value is not scanned again; an unknown or unfinished `{{` stays.
        assert_eq!(
            render("x {{name}} {{title}} {{other}} {{ {{name", &values),
            "x a{{title}}b T {{other}} {{ {{name"
        );
        assert_eq!(render("no placeholder", &values), "no placeholder");
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("./my_app"), "./my_app");
        assert_eq!(shell_quote("/tmp/a-b/c.d"), "/tmp/a-b/c.d");
        assert_eq!(shell_quote("/tmp/my apps/x"), "'/tmp/my apps/x'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("a$b;c"), "'a$b;c'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn a_failed_write_removes_the_directory_it_created() {
        let parent = std::env::temp_dir().join(format!(
            "forgebase-test-new-app-cleanup-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir_all(&parent).expect("parent");
        let target = parent.join("demo");
        // `a.txt` is a file, so writing `a.txt/b.txt` fails after the first file was written.
        let files = [("a.txt", "one"), ("a.txt/b.txt", "two")];
        let error = generate(&target, &|text| text.to_owned(), &parent, &files)
            .expect_err("the second file cannot be written");
        assert!(error.to_string().contains("cannot write"), "{error}");
        assert!(!target.exists(), "the half-written directory is removed");
        // The parent, which was not created by `generate`, stays.
        assert!(parent.is_dir());
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn every_template_placeholder_is_known() {
        let known = [
            "{{name}}",
            "{{title}}",
            "{{forgebase_path}}",
            "{{rust_version}}",
        ];
        for (path, text) in FILES {
            for part in path.split("{{").skip(1).chain(text.split("{{").skip(1)) {
                let placeholder = format!("{{{{{}", part.split("}}").next().unwrap_or_default());
                assert!(
                    known.contains(&format!("{placeholder}}}}}").as_str()),
                    "{path}: unknown placeholder {placeholder}}}}}"
                );
            }
        }
    }
}

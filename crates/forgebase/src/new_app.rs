//! `forgebase new-app`: writes a small app crate that builds and passes its own tests.
//!
//! The files are in `templates/app/` and embedded with `include_str!`. They are plain text with
//! four placeholders that are replaced with simple string replacement (no template engine):
//! `{{name}}`, `{{title}}` (the name as words, for labels and the role), `{{forgebase_path}}` (the
//! `crates/forgebase` directory of the checkout) and `{{rust_version}}` (the MSRV of this
//! Forgebase).

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

/// Names that cannot be the name of the crate: it would clash with a crate it uses or with the
/// standard library. (Cargo refuses Rust keywords itself, with a clear message.)
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
    if RESERVED.contains(&name) {
        problems.push(format!(
            "app name `{name}` cannot be used as a crate name here"
        ));
    }
    if !problems.is_empty() {
        return Err(CliError::NewApp(bullets(&problems)));
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
    let render = |text: &str| {
        values.iter().fold(text.to_owned(), |text, (key, value)| {
            text.replace(key, value)
        })
    };

    let io = |action: &str, path: &Path, error: std::io::Error| {
        CliError::NewApp(format!("cannot {action} {}: {error}", path.display()))
    };
    fs::create_dir_all(dir).map_err(|e| io("create", dir, e))?;
    fs::create_dir(&target).map_err(|e| io("create", &target, e))?;
    // From here on the directory is ours: when anything fails it is removed again, so a failed
    // run leaves nothing half written.
    let written = write_files(&target, &render, forgebase_path);
    if let Err(error) = written {
        let _ = fs::remove_dir_all(&target);
        return Err(error);
    }
    Ok(target)
}

/// What the command prints: where the app is and what to do next.
pub(crate) fn report(created: &Path) -> String {
    let path = created.display();
    format!(
        "created {path}\n\nnext steps:\n  cd {path}\n  export FORGEBASE_TEST_DATABASE_URL=postgres://user:password@localhost:5432/postgres\n  cargo test\n\nREADME.md says how to migrate, serve and extend the app; AGENTS.md has the rules for whoever maintains it.\n"
    )
}

fn write_files(
    target: &Path,
    render: &dyn Fn(&str) -> String,
    checkout: &Path,
) -> Result<(), CliError> {
    let io = |path: &Path, error: std::io::Error| {
        CliError::NewApp(format!("cannot write {}: {error}", path.display()))
    };
    for (path, template) in FILES {
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

//! `forgebase new-app`: what it writes and what it refuses. No database is needed. That the
//! generated crate builds and passes its own tests is checked by the CI job `app-template` (it
//! needs the network and a few minutes; CONTRIBUTING.md says how to run it locally).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

use forgebase_core::meta::DocType;

/// A scratch directory, removed when the value is dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "forgebase-test-new-app-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("scratch directory");
        // Resolved, so that comparisons with canonical paths hold (macOS: /var -> /private/var).
        Self(fs::canonicalize(dir).expect("canonical"))
    }

    /// A directory that looks like a Forgebase checkout, with a lock file.
    fn checkout(&self) -> PathBuf {
        let root = self.0.join("checkout");
        fs::create_dir_all(root.join("crates/forgebase")).expect("checkout");
        fs::write(
            root.join("crates/forgebase/Cargo.toml"),
            "[package]\nname = \"forgebase\"\n",
        )
        .expect("manifest");
        fs::write(root.join("Cargo.lock"), "# a lock file\nversion = 4\n").expect("lock");
        root
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn new_app(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_forgebase"))
        .arg("new-app")
        .args(args)
        .current_dir(cwd)
        .env_remove("FORGEBASE_DATABASE_URL")
        .output()
        .expect("run forgebase")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn files(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, found: &mut Vec<String>) {
        for entry in fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, root, found);
            } else {
                let relative = path.strip_prefix(root).expect("inside");
                found.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut found = Vec::new();
    walk(root, root, &mut found);
    found.sort();
    found
}

#[test]
fn it_writes_the_crate_with_every_placeholder_replaced() {
    let scratch = Scratch::new();
    let checkout = scratch.checkout();
    let output = new_app(
        &scratch.0,
        &["my_app", "--forgebase-path", checkout.to_str().unwrap()],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let created = scratch.0.join("my_app");
    // The default `--dir` is the current directory: the path is shown as `./my_app`.
    assert!(stdout.starts_with("created ./my_app\n"), "{stdout}");
    assert!(stdout.contains("cd ./my_app"), "{stdout}");
    assert!(stdout.contains("cargo test"), "{stdout}");

    assert_eq!(
        files(&created),
        [
            ".gitignore",
            "AGENTS.md",
            "Cargo.lock",
            "Cargo.toml",
            "README.md",
            "doctypes/my_app_note.toml",
            "migrations/README.md",
            "src/lib.rs",
            "src/main.rs",
            "tests/app.rs",
        ]
    );
    for file in files(&created) {
        let text = fs::read_to_string(created.join(&file)).expect("text");
        if file != "Cargo.lock" {
            assert!(!text.contains("{{"), "{file} still has a placeholder");
        }
    }

    let manifest = fs::read_to_string(created.join("Cargo.toml")).expect("manifest");
    let path = checkout.join("crates/forgebase");
    let dependency = format!("forgebase = {{ path = \"{}\"", path.display());
    assert_eq!(manifest.matches(&dependency).count(), 2, "{manifest}");
    assert!(manifest.contains("name = \"my_app\""), "{manifest}");
    assert!(manifest.contains("\n[workspace]"), "{manifest}");
    assert!(manifest.contains(&format!(
        "rust-version = \"{}\"",
        env!("CARGO_PKG_RUST_VERSION")
    )));
    // The feature of the test helpers is on the dev-dependency only.
    let (normal, dev) = manifest.split_once("[dev-dependencies]").expect("sections");
    assert!(!normal.contains("testing"));
    assert!(dev.contains("features = [\"testing\"]"));

    let lib = fs::read_to_string(created.join("src/lib.rs")).expect("lib");
    assert!(lib.contains("App::new(\"my_app\")"));
    assert!(lib.contains("\"doctypes/my_app_note.toml\""));
    // The checkout's lock file is copied as it is.
    assert_eq!(
        fs::read_to_string(created.join("Cargo.lock")).expect("lock"),
        "# a lock file\nversion = 4\n"
    );
}

#[test]
fn the_sample_doctype_is_valid_and_open_to_the_sample_role() {
    let scratch = Scratch::new();
    let checkout = scratch.checkout();
    let output = new_app(
        &scratch.0,
        &["my_app", "--forgebase-path", checkout.to_str().unwrap()],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text =
        fs::read_to_string(scratch.0.join("my_app/doctypes/my_app_note.toml")).expect("file");
    let doctype = DocType::from_toml_str(&text).expect("a valid DocType");
    assert_eq!(doctype.name, "my_app_note");
    assert_eq!(doctype.permissions.len(), 1);
    assert_eq!(doctype.permissions[0].role, "My App User");
    let tests = fs::read_to_string(scratch.0.join("my_app/tests/app.rs")).expect("tests");
    assert!(
        tests.contains("\"My App User\""),
        "the test uses the same role"
    );
}

#[test]
fn a_relative_forgebase_path_is_resolved_from_where_the_command_runs() {
    let scratch = Scratch::new();
    let checkout = scratch.checkout();
    // `--dir` is relative too, and does not exist yet.
    let output = new_app(
        &scratch.0,
        &["demo", "--forgebase-path", "checkout", "--dir", "apps/here"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let manifest =
        fs::read_to_string(scratch.0.join("apps/here/demo/Cargo.toml")).expect("manifest");
    let expected = format!("path = \"{}\"", checkout.join("crates/forgebase").display());
    assert!(manifest.contains(&expected), "{manifest}");
}

#[test]
fn it_works_without_a_lock_file_in_the_checkout() {
    let scratch = Scratch::new();
    let checkout = scratch.checkout();
    fs::remove_file(checkout.join("Cargo.lock")).expect("remove");
    let output = new_app(
        &scratch.0,
        &["demo", "--forgebase-path", checkout.to_str().unwrap()],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!scratch.0.join("demo/Cargo.lock").exists());
}

#[test]
fn it_refuses_an_existing_directory_and_leaves_it_alone() {
    let scratch = Scratch::new();
    let checkout = scratch.checkout();
    let existing = scratch.0.join("demo");
    fs::create_dir_all(&existing).expect("dir");
    fs::write(existing.join("mine.txt"), "keep").expect("file");
    let output = new_app(
        &scratch.0,
        &["demo", "--forgebase-path", checkout.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("error: "),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("already exists"),
        "{}",
        stderr(&output)
    );
    assert_eq!(files(&existing), ["mine.txt"]);
    assert_eq!(
        fs::read_to_string(existing.join("mine.txt")).expect("file"),
        "keep"
    );
}

#[test]
fn it_refuses_a_path_that_is_not_a_forgebase_checkout() {
    let scratch = Scratch::new();
    let elsewhere = scratch.0.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("dir");
    // Missing manifest.
    let output = new_app(
        &scratch.0,
        &["demo", "--forgebase-path", elsewhere.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("not a Forgebase checkout"),
        "{}",
        stderr(&output)
    );
    // A manifest of another crate.
    fs::create_dir_all(elsewhere.join("crates/forgebase")).expect("dir");
    fs::write(
        elsewhere.join("crates/forgebase/Cargo.toml"),
        "[package]\nname = \"other\"\n",
    )
    .expect("manifest");
    let output = new_app(
        &scratch.0,
        &["demo", "--forgebase-path", elsewhere.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("not a Forgebase checkout"),
        "{}",
        stderr(&output)
    );
    assert!(!scratch.0.join("demo").exists(), "nothing is created");
}

#[test]
fn it_refuses_names_that_are_not_app_names_or_crate_names() {
    let scratch = Scratch::new();
    let checkout = scratch.checkout();
    let long = "a".repeat(55);
    let cases = [
        ("My-App", "must match"),
        ("1app", "must match"),
        ("", "must not be empty"),
        ("local", "reserved"),
        ("forgebase", "cannot be used as a crate name"),
        ("core", "cannot be used as a crate name"),
        (long.as_str(), "the maximum is 54"),
    ];
    for (name, expected) in cases {
        let output = new_app(
            &scratch.0,
            &[name, "--forgebase-path", checkout.to_str().unwrap()],
        );
        assert_eq!(output.status.code(), Some(1), "name {name:?}");
        assert!(
            stderr(&output).contains(expected),
            "name {name:?}: {}",
            stderr(&output)
        );
    }
    // Nothing but the checkout was created.
    assert_eq!(fs::read_dir(&scratch.0).expect("dir").count(), 1);
}

#[test]
fn a_name_of_54_bytes_is_accepted() {
    let scratch = Scratch::new();
    let checkout = scratch.checkout();
    let name = "a".repeat(54);
    let output = new_app(
        &scratch.0,
        &[&name, "--forgebase-path", checkout.to_str().unwrap()],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = fs::read_to_string(scratch.0.join(format!("{name}/doctypes/{name}_note.toml")))
        .expect("file");
    DocType::from_toml_str(&text).expect("the longest sample DocType name is valid");
}

#[test]
fn the_path_option_is_required() {
    let scratch = Scratch::new();
    let output = new_app(&scratch.0, &["demo"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("--forgebase-path"),
        "{}",
        stderr(&output)
    );
}

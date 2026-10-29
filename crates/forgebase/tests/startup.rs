//! Startup errors of the app model (M5.1, ADR 0012): every way the apps of a binary can be
//! wrong is reported by `Forgebase::check`, all problems together. No database needed.

use forgebase::{App, DocTypeFile, DocTypeHooks, Forgebase, MigrationFile};

/// A static list of DocType files, from `(path, toml)` pairs.
macro_rules! files {
    ($(($path:literal, $toml:expr)),* $(,)?) => {
        const { &[$(DocTypeFile::new($path, $toml)),*] }
    };
}

/// A static list of migrations, from `(version, name, sql)`.
macro_rules! sql {
    ($(($version:expr, $name:literal, $sql:literal)),* $(,)?) => {
        const { &[$(MigrationFile::new($version, $name, concat!($name, ".sql"), $sql)),*] }
    };
}

const CUSTOMER: &str = r#"
name = "customer"
label = "Customer"
[[fields]]
name = "title"
type = "data"
"#;

/// Links to `customer`.
const ORDER: &str = r#"
name = "order"
label = "Order"
[[fields]]
name = "customer"
type = "link"
target = "customer"
"#;

/// Links to `order` (of a dependency) and to `customer` (of a dependency of a dependency).
const INVOICE: &str = r#"
name = "invoice"
label = "Invoice"
[[fields]]
name = "order"
type = "link"
target = "order"
[[fields]]
name = "customer"
type = "link"
target = "customer"
"#;

const CHILD: &str = r#"
name = "order_line"
label = "Order Line"
is_child = true
"#;

struct Nothing;
impl DocTypeHooks for Nothing {}

fn hooks() -> impl DocTypeHooks + 'static {
    Nothing
}

fn crm() -> App {
    App::new("crm").doctypes(files![("doctypes/customer.toml", CUSTOMER)])
}

fn sales() -> App {
    App::new("sales")
        .depends_on(&["crm"])
        .doctypes(files![("doctypes/order.toml", ORDER)])
}

/// The problems of a binary, one string per problem.
fn problems(binary: &Forgebase) -> Vec<String> {
    match binary.check() {
        Ok(()) => Vec::new(),
        Err(error) => error.problems().map(str::to_owned).collect(),
    }
}

fn only(binary: &Forgebase) -> String {
    let problems = problems(binary);
    assert_eq!(problems.len(), 1, "{problems:#?}");
    problems.into_iter().next().expect("one problem")
}

#[test]
fn a_valid_setup_has_no_problem() {
    assert_eq!(problems(&Forgebase::new()), Vec::<String>::new());
    let binary = Forgebase::new().app(crm()).app(sales());
    assert_eq!(problems(&binary), Vec::<String>::new());
}

#[test]
fn an_invalid_app_name_is_reported() {
    for bad in ["Sales", "my app", "", "1st", "sales-app"] {
        let message = only(&Forgebase::new().app(App::new(bad)));
        assert!(
            message.contains(&format!("app name {bad:?} ")),
            "{bad:?}: {message}"
        );
    }
    // `local` names the --doctypes directory.
    let message = only(&Forgebase::new().app(App::new("local")));
    assert!(message.contains("`local` is reserved"), "{message}");
}

#[test]
fn a_duplicate_app_name_is_reported() {
    let binary = Forgebase::new().app(crm()).app(App::new("crm"));
    assert_eq!(only(&binary), "app `crm` is registered more than once");
}

#[test]
fn a_missing_dependency_is_reported() {
    let binary = Forgebase::new().app(sales());
    assert_eq!(
        problems(&binary)[0],
        "app `sales` depends on `crm`, which is not registered"
    );
}

#[test]
fn a_dependency_registered_after_its_dependent_is_reported_and_never_sorted() {
    let binary = Forgebase::new().app(sales()).app(crm());
    let all = problems(&binary);
    assert!(
        all[0].starts_with("app `sales` depends on `crm`, which is registered after it;"),
        "{all:?}"
    );
}

#[test]
fn an_app_cannot_depend_on_itself() {
    let binary = Forgebase::new().app(App::new("crm").depends_on(&["crm"]));
    assert_eq!(only(&binary), "app `crm` depends on itself");
}

#[test]
fn a_doctype_defined_by_two_apps_names_both() {
    let binary = Forgebase::new()
        .app(crm())
        .app(App::new("other").doctypes(files![("doctypes/customer.toml", CUSTOMER)]));
    let message = only(&binary);
    assert!(
        message.contains("app `other`")
            && message.contains("doctype `customer` is already defined by app `crm`")
            && message.contains("doctypes/customer.toml"),
        "{message}"
    );
}

#[test]
fn a_doctype_defined_twice_in_one_app_names_both_files() {
    let binary = Forgebase::new().app(App::new("crm").doctypes(files![
        ("doctypes/a.toml", CUSTOMER),
        ("doctypes/b.toml", CUSTOMER),
    ]));
    let message = only(&binary);
    assert!(
        message.contains("doctypes/b.toml")
            && message.contains("already defined in doctypes/a.toml"),
        "{message}"
    );
}

#[test]
fn a_broken_doctype_file_names_the_app_and_the_file() {
    let binary = Forgebase::new().app(App::new("crm").doctypes(files![
        ("doctypes/ok.toml", CUSTOMER),
        ("doctypes/broken.toml", "name = "),
    ]));
    let message = only(&binary);
    assert!(
        message.starts_with("app `crm`: doctypes/broken.toml: "),
        "{message}"
    );
}

#[test]
fn hooks_for_an_unknown_doctype_are_reported() {
    let binary = Forgebase::new().app(crm().hooks("nope", hooks()));
    assert_eq!(
        only(&binary),
        "app `crm`: hooks registered for unknown DocType `nope`"
    );
}

#[test]
fn hooks_for_a_doctype_of_an_app_that_is_not_a_dependency_are_reported() {
    // `other` is registered, but sales does not depend on it.
    let other = App::new("other").doctypes(files![("doctypes/customer.toml", CUSTOMER)]);
    let binary = Forgebase::new()
        .app(other)
        .app(App::new("sales").hooks("customer", hooks()));
    assert_eq!(
        only(&binary),
        "app `sales`: hooks registered for DocType `customer` of app `other`, \
         which `sales` does not depend on"
    );
}

#[test]
fn hooks_for_a_doctype_of_a_dependency_are_allowed_also_through_another_app() {
    // sales -> crm, and a third app that depends on sales only may hook crm's DocType.
    let third = App::new("third")
        .depends_on(&["sales"])
        .hooks("customer", hooks())
        .hooks("order", hooks());
    let binary = Forgebase::new()
        .app(crm().hooks("customer", hooks()))
        .app(sales().hooks("customer", hooks()))
        .app(third);
    assert_eq!(problems(&binary), Vec::<String>::new());
}

#[test]
fn hooks_for_a_child_doctype_are_reported() {
    let app = App::new("crm")
        .doctypes(files![
            ("doctypes/order.toml", "name = \"order\"\nlabel = \"O\"\n[[fields]]\nname = \"lines\"\ntype = \"table\"\ntarget = \"order_line\"\n"),
            ("doctypes/order_line.toml", CHILD),
        ])
        .hooks("order_line", hooks());
    let message = only(&Forgebase::new().app(app));
    assert!(
        message.starts_with("app `crm`: hooks for `order_line`: ") && message.contains("child"),
        "{message}"
    );
}

#[test]
fn a_link_to_an_app_that_is_not_a_dependency_is_reported() {
    // sales links to crm's customer but does not depend on crm.
    let sales = App::new("sales").doctypes(files![("doctypes/order.toml", ORDER)]);
    let binary = Forgebase::new().app(crm()).app(sales);
    let message = only(&binary);
    assert!(
        message.contains("app `sales`")
            && message.contains("doctypes/order.toml")
            && message.contains("field `customer` of doctype `order` refers to `customer`")
            && message.contains("belongs to app `crm`")
            && message.contains("does not depend on `crm`"),
        "{message}"
    );
}

#[test]
fn a_link_to_a_dependency_of_a_dependency_is_allowed() {
    // invoice -> order (sales) and, through sales, customer (crm) is reachable too.
    let billing = App::new("billing")
        .depends_on(&["sales"])
        .doctypes(files![("doctypes/invoice.toml", INVOICE)]);
    let binary = Forgebase::new().app(crm()).app(sales()).app(billing);
    assert_eq!(problems(&binary), Vec::<String>::new());
}

#[test]
fn unknown_link_targets_still_name_the_app_and_file() {
    let app = App::new("sales").doctypes(files![("doctypes/order.toml", ORDER)]);
    let message = only(&Forgebase::new().app(app));
    assert!(
        message.starts_with("app `sales`: doctypes/order.toml: ")
            && message.contains("links to unknown doctype `customer`"),
        "{message}"
    );
}

#[test]
fn migration_versions_must_be_positive_and_increasing() {
    let migrations =
        |m: &'static [MigrationFile]| Forgebase::new().app(App::new("crm").migrations(m));
    assert!(
        problems(&migrations(sql![
            (1, "a", "SELECT 1"),
            (2, "b", "SELECT 2")
        ]))
        .is_empty()
    );
    let cases: [(&'static [MigrationFile], &str); 4] = [
        (
            sql![(1, "a", "SELECT 1"), (1, "b", "SELECT 2")],
            "app `crm`: migration versions must increase: 1 (b, b.sql) comes after 1 (a)",
        ),
        (
            sql![(2, "a", "SELECT 1"), (1, "b", "SELECT 2")],
            "app `crm`: migration versions must increase: 1 (b, b.sql) comes after 2 (a)",
        ),
        (
            sql![(0, "a", "SELECT 1")],
            "app `crm`: migration version 0 (a, a.sql) must be positive",
        ),
        (
            sql![(-3, "a", "SELECT 1")],
            "app `crm`: migration version -3 (a, a.sql) must be positive",
        ),
    ];
    for (list, expected) in cases {
        assert_eq!(only(&migrations(list)), expected);
    }
}

#[test]
fn migrations_added_in_several_calls_must_still_increase() {
    let app = App::new("crm")
        .migrations(sql![(2, "a", "SELECT 1")])
        .migrations(sql![(1, "b", "SELECT 2")]);
    assert!(only(&Forgebase::new().app(app)).contains("must increase"));
}

#[test]
fn every_problem_is_reported_together_and_printed_in_two_lists() {
    let binary = Forgebase::new()
        .app(App::new("Bad Name"))
        .app(sales().hooks("nope", hooks()))
        .app(App::new("broken").doctypes(files![("doctypes/x.toml", "name = ")]));
    let error = binary.check().expect_err("invalid");
    let text = error.to_string();
    assert_eq!(
        text.matches("\n  - ").count(),
        error.problems().count(),
        "{text}"
    );
    assert!(error.problems().count() >= 3, "{text}");
    assert!(
        text.starts_with("invalid app setup:\n  - app name \"Bad Name\" "),
        "{text}"
    );
    assert!(
        text.contains("\ncannot load DocType files:\n  - app `broken`: doctypes/x.toml: "),
        "{text}"
    );
    // A broken file hides the reference and hook checks, which would only add follow-up noise.
    assert!(!text.contains("hooks registered for unknown"), "{text}");
}

// ---- run_from ---------------------------------------------------------------------------------

/// `ExitCode` has no `PartialEq`; its `Debug` text names the code.
fn exit(code: std::process::ExitCode) -> String {
    format!("{code:?}")
}

#[test]
fn run_from_returns_exit_codes_instead_of_exiting_the_process() {
    use std::process::ExitCode;

    // Help is a success, and bad usage is 2: both are returned, the test process goes on.
    let stock = || Forgebase::new();
    assert_eq!(
        exit(stock().run_from(["forgebase", "--help"])),
        exit(ExitCode::SUCCESS)
    );
    assert_eq!(
        exit(stock().run_from(["forgebase", "bogus"])),
        exit(ExitCode::from(2))
    );
    // No apps and no --doctypes.
    assert_eq!(
        exit(stock().run_from(["forgebase", "migrate"])),
        exit(ExitCode::from(2))
    );
}

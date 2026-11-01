//! Human-readable output of `forgebase migrate`.

use std::fmt::Write as _;

use forgebase_db::MigrateReport;

/// Renders what a migrate did, or in a dry run would do. The app migrations are mentioned only
/// when the binary has apps (`apps` is how many).
pub fn migrate_report(report: &MigrateReport, dry_run: bool, apps: usize) -> String {
    let mut text = String::new();
    if report.migrations.is_empty() {
        text.push_str("framework migrations: up to date\n");
    } else {
        let verb = if dry_run { "pending" } else { "applied" };
        let _ = writeln!(
            text,
            "framework migrations: {} {verb}",
            report.migrations.len()
        );
        for migration in &report.migrations {
            let _ = writeln!(text, "  {:04} {}", migration.version, migration.name);
        }
    }
    let Some(sync) = &report.sync else {
        text.push_str(
            "schema sync: not planned yet, it needs the tables the pending framework migrations create; run without --dry-run to apply them\n",
        );
        app_migrations(&mut text, report, dry_run, apps);
        fixtures(&mut text, report, dry_run);
        return text;
    };
    if sync.steps.is_empty() {
        text.push_str("schema sync: up to date\n");
    } else {
        let verb = if dry_run { "would apply" } else { "applied" };
        let _ = writeln!(text, "schema sync: {verb} {} step(s)", sync.steps.len());
        for step in &sync.steps {
            let _ = writeln!(text, "{step}");
        }
    }
    app_migrations(&mut text, report, dry_run, apps);
    schedules(&mut text, report, dry_run);
    fixtures(&mut text, report, dry_run);
    if !sync.warnings.is_empty() {
        let _ = writeln!(text, "warnings: {}", sync.warnings.len());
        for warning in &sync.warnings {
            let _ = writeln!(text, "  - {warning}");
        }
    }
    if dry_run {
        text.push_str("dry run: nothing was changed\n");
    }
    text
}

/// The changes to the schedules, when there are any (nothing is printed when they are up to date).
fn schedules(text: &mut String, report: &MigrateReport, dry_run: bool) {
    if report.schedules.is_empty() {
        return;
    }
    let verb = if dry_run { "would change" } else { "changed" };
    let _ = writeln!(text, "schedules: {verb} {}", report.schedules.len());
    for line in &report.schedules {
        let _ = writeln!(text, "  {line}");
    }
}

/// One line per app that has fixtures (nothing for the others): what was done, or in a dry run
/// what would be.
fn fixtures(text: &mut String, report: &MigrateReport, dry_run: bool) {
    for app in &report.fixtures {
        if !report.fixtures_planned {
            let _ = writeln!(
                text,
                "fixtures: {}: {} record(s), not planned yet (they are applied after the pending framework migrations)",
                app.app, app.records
            );
        } else if dry_run {
            let _ = writeln!(
                text,
                "fixtures: {}: would insert {}, update {}, keep {}, {} unchanged",
                app.app, app.inserted, app.updated, app.kept, app.unchanged
            );
        } else {
            let _ = writeln!(
                text,
                "fixtures: {}: {} inserted, {} updated, {} kept, {} unchanged",
                app.app, app.inserted, app.updated, app.kept, app.unchanged
            );
        }
    }
}

/// The app migrations, in the order they run (or would run).
fn app_migrations(text: &mut String, report: &MigrateReport, dry_run: bool, apps: usize) {
    if apps == 0 && report.app_migrations.is_empty() {
        return;
    }
    if report.app_migrations.is_empty() {
        text.push_str("app migrations: up to date\n");
        return;
    }
    let verb = if dry_run { "pending" } else { "applied" };
    let _ = writeln!(
        text,
        "app migrations: {} {verb}",
        report.app_migrations.len()
    );
    for migration in &report.app_migrations {
        let _ = writeln!(
            text,
            "  {} {:04} {}",
            migration.app, migration.version, migration.name
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forgebase_db::{AppliedAppMigration, MIGRATIONS, SyncReport, Warning};

    fn sync(steps: usize, warnings: Vec<Warning>, dry_run: bool) -> SyncReport {
        SyncReport {
            dry_run,
            steps: (0..steps)
                .map(|i| forgebase_db::Step::RegisterDocType {
                    doctype: format!("d{i}"),
                })
                .collect(),
            warnings,
        }
    }

    #[test]
    fn up_to_date() {
        let report = MigrateReport {
            migrations: vec![],
            sync: Some(sync(0, vec![], false)),
            app_migrations: vec![],
            schedules: vec![],
            workflow_warnings: vec![],
            fixtures: vec![],
            fixtures_planned: true,
            fixtures_failure: None,
        };
        assert_eq!(
            migrate_report(&report, false, 0),
            "framework migrations: up to date\nschema sync: up to date\n"
        );
    }

    #[test]
    fn schedule_changes_are_listed_only_when_there_are_some() {
        let mut report = MigrateReport {
            migrations: vec![],
            sync: Some(sync(0, vec![], false)),
            app_migrations: vec![],
            schedules: vec![],
            workflow_warnings: vec![],
            fixtures: vec![],
            fixtures_planned: true,
            fixtures_failure: None,
        };
        assert!(!migrate_report(&report, false, 0).contains("schedules"));
        report.schedules = vec!["add crm.digest (daily 06:30)".to_owned()];
        assert!(
            migrate_report(&report, false, 0)
                .contains("schedules: changed 1\n  add crm.digest (daily 06:30)\n")
        );
        assert!(migrate_report(&report, true, 0).contains("schedules: would change 1\n"));
    }

    #[test]
    fn applied_steps_and_warnings() {
        let report = MigrateReport {
            migrations: MIGRATIONS.iter().collect(),
            sync: Some(sync(
                1,
                vec![Warning::DocTypeNotInRegistry {
                    doctype: "old".to_owned(),
                }],
                false,
            )),
            app_migrations: vec![],
            schedules: vec![],
            workflow_warnings: vec![],
            fixtures: vec![],
            fixtures_planned: true,
            fixtures_failure: None,
        };
        let text = migrate_report(&report, false, 0);
        assert!(
            text.starts_with(
                "framework migrations: 12 applied\n  0001 init\n  0002 series\n  0003 auth\n  0004 apps\n  0005 jobs\n  0006 schedules\n  0007 finished_jobs_index\n  0008 workflow_log\n  0009 saved_views\n  0010 webhooks\n  0011 fixtures\n  0012 custom_fields\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("schema sync: applied 1 step(s)\n-- register DocType \"d0\""),
            "{text}"
        );
        assert!(text.contains("warnings: 1\n  - DocType \"old\""), "{text}");
        assert!(!text.contains("dry run"));
    }

    #[test]
    fn dry_run_with_pending_migrations_has_no_sync_plan() {
        let report = MigrateReport {
            migrations: MIGRATIONS.iter().collect(),
            sync: None,
            app_migrations: vec![],
            schedules: vec![],
            workflow_warnings: vec![],
            fixtures: vec![],
            fixtures_planned: true,
            fixtures_failure: None,
        };
        let text = migrate_report(&report, true, 0);
        assert!(text.contains("12 pending"), "{text}");
        assert!(text.contains("schema sync: not planned yet"), "{text}");
    }

    #[test]
    fn dry_run_says_nothing_was_changed() {
        let report = MigrateReport {
            migrations: vec![],
            sync: Some(sync(2, vec![], true)),
            app_migrations: vec![],
            schedules: vec![],
            workflow_warnings: vec![],
            fixtures: vec![],
            fixtures_planned: true,
            fixtures_failure: None,
        };
        let text = migrate_report(&report, true, 0);
        assert!(text.contains("would apply 2 step(s)"), "{text}");
        assert!(text.ends_with("dry run: nothing was changed\n"), "{text}");
    }

    #[test]
    fn fixtures_are_listed_one_line_per_app() {
        let app = |name: &str, counts: [usize; 4]| forgebase_db::AppFixtures {
            app: name.to_owned(),
            records: counts.iter().sum(),
            inserted: counts[0],
            updated: counts[1],
            kept: counts[2],
            unchanged: counts[3],
        };
        let mut report = MigrateReport {
            migrations: vec![],
            sync: Some(sync(0, vec![], false)),
            app_migrations: vec![],
            schedules: vec![],
            workflow_warnings: vec![],
            fixtures: vec![],
            fixtures_planned: true,
            fixtures_failure: None,
        };
        // No fixtures: nothing is said.
        assert!(!migrate_report(&report, false, 2).contains("fixtures"));
        report.fixtures = vec![app("crm", [2, 1, 0, 5]), app("sales", [0, 0, 0, 3])];
        assert_eq!(
            migrate_report(&report, false, 2),
            "framework migrations: up to date\nschema sync: up to date\napp migrations: up to date\n\
             fixtures: crm: 2 inserted, 1 updated, 0 kept, 5 unchanged\n\
             fixtures: sales: 0 inserted, 0 updated, 0 kept, 3 unchanged\n"
        );
        let dry = migrate_report(&report, true, 2);
        assert!(
            dry.contains("fixtures: crm: would insert 2, update 1, keep 0, 5 unchanged\n"),
            "{dry}"
        );
        // While framework migrations are pending the plan is not made.
        report.sync = None;
        report.fixtures_planned = false;
        let text = migrate_report(&report, true, 2);
        assert!(
            text.contains("fixtures: crm: 8 record(s), not planned yet"),
            "{text}"
        );
    }

    #[test]
    fn app_migrations_are_listed_in_order_when_the_binary_has_apps() {
        let migration = |app: &str, version: i32, name: &str| AppliedAppMigration {
            app: app.to_owned(),
            version,
            name: name.to_owned(),
        };
        let mut report = MigrateReport {
            migrations: vec![],
            sync: Some(sync(0, vec![], false)),
            app_migrations: vec![],
            schedules: vec![],
            workflow_warnings: vec![],
            fixtures: vec![],
            fixtures_planned: true,
            fixtures_failure: None,
        };
        // No apps: nothing is said. Apps without pending migrations: up to date.
        assert!(!migrate_report(&report, false, 0).contains("app migrations"));
        assert_eq!(
            migrate_report(&report, false, 2),
            "framework migrations: up to date\nschema sync: up to date\napp migrations: up to date\n"
        );
        report.app_migrations = vec![migration("crm", 1, "a"), migration("sales", 12, "b")];
        assert_eq!(
            migrate_report(&report, false, 2),
            "framework migrations: up to date\nschema sync: up to date\n\
             app migrations: 2 applied\n  crm 0001 a\n  sales 0012 b\n"
        );
        let dry = migrate_report(&report, true, 2);
        assert!(dry.contains("app migrations: 2 pending"), "{dry}");
        assert!(dry.ends_with("dry run: nothing was changed\n"), "{dry}");
        // While framework migrations are pending, the app migrations are still listed.
        report.sync = None;
        let text = migrate_report(&report, true, 2);
        assert!(text.contains("schema sync: not planned yet"), "{text}");
        assert!(text.ends_with("  sales 0012 b\n"), "{text}");
    }
}

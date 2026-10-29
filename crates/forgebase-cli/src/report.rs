//! Human-readable output of `forgebase migrate`.

use std::fmt::Write as _;

use forgebase_db::MigrateReport;

/// Renders what a migrate did, or in a dry run would do.
pub fn migrate_report(report: &MigrateReport, dry_run: bool) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use forgebase_db::{MIGRATIONS, SyncReport, Warning};

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
        };
        assert_eq!(
            migrate_report(&report, false),
            "framework migrations: up to date\nschema sync: up to date\n"
        );
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
        };
        let text = migrate_report(&report, false);
        assert!(
            text.starts_with(
                "framework migrations: 3 applied\n  0001 init\n  0002 series\n  0003 auth\n"
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
        };
        let text = migrate_report(&report, true);
        assert!(text.contains("3 pending"), "{text}");
        assert!(text.contains("schema sync: not planned yet"), "{text}");
    }

    #[test]
    fn dry_run_says_nothing_was_changed() {
        let report = MigrateReport {
            migrations: vec![],
            sync: Some(sync(2, vec![], true)),
        };
        let text = migrate_report(&report, true);
        assert!(text.contains("would apply 2 step(s)"), "{text}");
        assert!(text.ends_with("dry run: nothing was changed\n"), "{text}");
    }
}

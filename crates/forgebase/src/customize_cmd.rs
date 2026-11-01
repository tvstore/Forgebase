//! The `customize field` commands (M10.1, ADR 0023): add, list, disable and enable the custom
//! fields of a site. They run as the system context, like `webhook` and `user`; the change is
//! recorded as made by `cli`. The work is in `forgebase_db` (one transaction under the migrate
//! lock), which the Administrator-only API uses too.

use std::fmt::Write as _;
use std::io::Read;
use std::time::Duration;

use forgebase_core::meta::{DocField, Registry};
use forgebase_core::overlay::parse_field_toml;
use forgebase_db::{CustomField, DbError};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::args::FieldCommand;
use crate::config::SiteContext;
use crate::error::CliError;
use crate::run::write_stdout;

/// Who a command line change is recorded as.
const ACTOR: &str = "cli";

/// The longest wait for the migrate lock (the default of `migrate`).
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// The field of `add`, read from the file (or standard input for `-`) and parsed. Called before
/// the site's pool is opened, so a bad file never touches the database. `None` for the other
/// commands.
pub(crate) fn read_field(command: &FieldCommand) -> Result<Option<DocField>, CliError> {
    let FieldCommand::Add { field, .. } = command else {
        return Ok(None);
    };
    let mut text = String::new();
    if field == "-" {
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(CliError::Stdin)?;
    } else {
        text = std::fs::read_to_string(field)
            .map_err(|e| CliError::Customize(format!("cannot read {field}: {e}")))?;
    }
    parse_field_toml(&text)
        .map(Some)
        .map_err(|e| CliError::Customize(e.to_string()))
}

/// Runs a `customize field` command. `base` is the registry of the site's apps (with the
/// `--doctypes` directory for `add`), without its custom fields; `field` is what [`read_field`]
/// returned.
pub(crate) async fn field(
    site: &SiteContext,
    base: &Registry,
    command: FieldCommand,
    field: Option<DocField>,
) -> Result<(), CliError> {
    let mut client = site.pool.get().await.map_err(DbError::from)?;
    match command {
        FieldCommand::Add { doctype, .. } => {
            let Some(field) = field else {
                return Err(CliError::Customize("no field definition".to_owned()));
            };
            let stored = forgebase_db::add_custom_field(
                &mut client,
                base,
                &site.site.name,
                &doctype,
                field,
                ACTOR,
                LOCK_TIMEOUT,
            )
            .await?;
            write_stdout(&format!(
                "added custom field {}.{} ({}); its column exists now, and a running server or worker uses it within 10 seconds\n",
                stored.doctype,
                stored.name,
                stored.type_name()
            ))
        }
        FieldCommand::List { doctype } => {
            let fields = forgebase_db::list_custom_fields(&**client, doctype.as_deref()).await?;
            write_stdout(&table(&fields))
        }
        FieldCommand::Disable { doctype, name } => {
            forgebase_db::set_custom_field_enabled(
                &mut client,
                &doctype,
                &name,
                false,
                ACTOR,
                LOCK_TIMEOUT,
            )
            .await?;
            write_stdout(&format!(
                "disabled custom field {doctype}.{name}; it is hidden everywhere, its column and data are kept\n"
            ))
        }
        FieldCommand::Enable { doctype, name } => {
            forgebase_db::set_custom_field_enabled(
                &mut client,
                &doctype,
                &name,
                true,
                ACTOR,
                LOCK_TIMEOUT,
            )
            .await?;
            write_stdout(&format!(
                "enabled custom field {doctype}.{name}; it is shown again with its data\n"
            ))
        }
    }
}

/// The table of `list`.
fn table(fields: &[CustomField]) -> String {
    let header = [
        "DOCTYPE", "NAME", "TYPE", "LABEL", "ENABLED", "MODIFIED", "BY",
    ];
    let rows: Vec<[String; 7]> = fields
        .iter()
        .map(|f| {
            [
                f.doctype.clone(),
                f.name.clone(),
                f.type_name().to_owned(),
                f.label().to_owned(),
                if f.enabled { "yes" } else { "no" }.to_owned(),
                rfc3339(f.modified_at),
                f.modified_by.clone(),
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..header.len())
        .map(|i| {
            rows.iter()
                .map(|row| row[i].chars().count())
                .chain([header[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut text = String::new();
    let mut line = |cells: &[&str]| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            let _ = write!(out, "{cell:<width$}  ", width = widths[i]);
        }
        text.push_str(out.trim_end());
        text.push('\n');
    };
    line(&header);
    for row in &rows {
        line(&row.each_ref().map(String::as_str));
    }
    text
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_list_shows_one_line_per_field() {
        let field = CustomField {
            doctype: "customer".to_owned(),
            name: "custom_vip".to_owned(),
            definition: json!({"name": "custom_vip", "label": "VIP", "type": "check"}),
            enabled: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            modified_at: OffsetDateTime::UNIX_EPOCH,
            modified_by: "cli".to_owned(),
        };
        let text = table(&[field]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("DOCTYPE"), "{text}");
        assert!(
            lines[1].contains("custom_vip")
                && lines[1].contains("check")
                && lines[1].contains("VIP")
                && lines[1].contains("no")
                && lines[1].contains("1970-01-01T00:00:00Z"),
            "{text}"
        );
    }
}

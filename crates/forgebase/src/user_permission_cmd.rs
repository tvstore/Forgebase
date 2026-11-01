//! The `user permission` commands (M10.3, ADR 0025): add, remove and list the documents a user is
//! restricted to. They run as the system context, like the other `user` commands; the work is in
//! `forgebase_db::user_permissions`, which the Administrator-only API uses too. A change applies
//! on the user's next request, so no server needs a restart or a reload.

use std::fmt::Write as _;

use forgebase_core::meta::Registry;
use forgebase_core::permission::MAX_USER_PERMISSIONS;
use forgebase_db::DbError;
use forgebase_db::user_permissions::{
    UserPermission, add_user_permission, list_user_permissions, remove_user_permission,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::args::PermissionCommand;
use crate::config::SiteContext;
use crate::error::CliError;
use crate::run::write_stdout;

/// Runs a `user permission` command. `registry` is the site's effective registry: the DocType
/// of `add` must be in it.
pub(crate) async fn permission(
    site: &SiteContext,
    registry: &Registry,
    command: PermissionCommand,
) -> Result<(), CliError> {
    let mut client = site.pool.get().await.map_err(DbError::from)?;
    let client = &mut **client;
    let text = match command {
        PermissionCommand::Add {
            user,
            doctype,
            value,
            ..
        } => {
            if add_user_permission(client, registry, &user, &doctype, &value).await? {
                format!(
                    "restricted user {user} to {doctype} {value}; it applies on the user's next request (at most {MAX_USER_PERMISSIONS} per DocType)\n"
                )
            } else {
                format!("user {user} is restricted to {doctype} {value} already\n")
            }
        }
        PermissionCommand::Remove {
            user,
            doctype,
            value,
        } => {
            if remove_user_permission(client, &user, &doctype, &value).await? {
                format!(
                    "removed the restriction of user {user} to {doctype} {value}; it applies on the user's next request\n"
                )
            } else {
                format!("user {user} is not restricted to {doctype} {value}\n")
            }
        }
        PermissionCommand::List { user, doctype } => {
            let rows = list_user_permissions(&*client, user.as_deref(), doctype.as_deref()).await?;
            table(&rows)
        }
    };
    write_stdout(&text)
}

/// The table of `list`: sorted by user, DocType and value, the header always present.
fn table(rows: &[UserPermission]) -> String {
    let header = ["USER", "DOCTYPE", "VALUE", "ADDED"];
    let rows: Vec<[String; 4]> = rows
        .iter()
        .map(|p| {
            [
                p.user.clone(),
                p.doctype.clone(),
                p.value.clone(),
                rfc3339(p.created_at),
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

    #[test]
    fn the_list_shows_one_line_per_row_under_a_header() {
        let row = UserPermission {
            user: "ann".to_owned(),
            doctype: "customer_group".to_owned(),
            value: "Retail".to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        let text = table(&[row]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("USER"), "{text}");
        assert_eq!(
            lines[1],
            "ann   customer_group  Retail  1970-01-01T00:00:00Z"
        );
        assert_eq!(table(&[]).lines().count(), 1);
    }
}

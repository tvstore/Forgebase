//! The permission model, phase 1 (M3.2, ADR 0011): rules, rights, the caller and the one
//! evaluator that decides.
//!
//! A DocType declares `[[permissions]]` rules: each names a role and the [`Right`]s it grants,
//! optionally only on documents the caller owns (`if_owner`). There is **no inheritance and no
//! deny rule**: access is the union of what the caller's roles grant, and a DocType without
//! rules is inaccessible to everyone except the [`ADMINISTRATOR`] role and the trusted
//! [`Caller::System`] context (deny by default). Child DocTypes have no rules: their rows follow
//! the parent.
//!
//! [`evaluate`] is the **only** place that makes a permission decision. It is pure (no I/O, no
//! clock): `forgebase-db` calls it and turns the [`Access`] it returns into SQL (for read access,
//! `WHERE owner = $caller`) or into a refusal. Nothing else compares roles.

use std::fmt;

use crate::meta::DocType;
use crate::method::MethodAccess;
use crate::workflow::Transition;

/// The built-in role that bypasses every permission rule.
pub const ADMINISTRATOR: &str = "Administrator";

/// Longest allowed role name, in characters.
pub const MAX_ROLE_LEN: usize = 140;

/// Checks a role name: 1 to [`MAX_ROLE_LEN`] characters, not blank, no control characters. The
/// same rule applies to the roles of users (`forgebase-db`'s `auth` module) and to the roles in
/// `[[permissions]]`, so a rule can always name a role a user can have. The error is the reason,
/// ready to follow "invalid role: ".
pub fn check_role_name(role: &str) -> Result<(), &'static str> {
    let length = role.chars().count();
    if length == 0 || length > MAX_ROLE_LEN {
        Err("must be 1 to 140 characters")
    } else if role.trim().is_empty() {
        Err("must not be blank")
    } else if role.chars().any(char::is_control) {
        Err("must not contain control characters")
    } else {
        Ok(())
    }
}

/// One thing a caller may do with documents of a DocType.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Right {
    /// See documents (get and list).
    Read,
    /// Change a document.
    Write,
    /// Create a document.
    Create,
    /// Delete a document.
    Delete,
    /// Submit a draft (submittable DocTypes).
    Submit,
    /// Cancel a submitted document (submittable DocTypes).
    Cancel,
    /// Amend a cancelled document (submittable DocTypes).
    Amend,
    /// Download documents as a file (M9.3, ADR 0021). Needs `read` as well.
    Export,
    /// Upload a file of documents (M9.3, ADR 0021). Needs `create` (and `write` to upsert).
    Import,
}

impl Right {
    /// Every right, in the order of the `[[permissions]]` keys.
    pub const ALL: [Right; 9] = [
        Right::Read,
        Right::Write,
        Right::Create,
        Right::Delete,
        Right::Submit,
        Right::Cancel,
        Right::Amend,
        Right::Export,
        Right::Import,
    ];

    /// The key used in `[[permissions]]` and in messages: `read`, `write`, ...
    pub fn name(self) -> &'static str {
        match self {
            Right::Read => "read",
            Right::Write => "write",
            Right::Create => "create",
            Right::Delete => "delete",
            Right::Submit => "submit",
            Right::Cancel => "cancel",
            Right::Amend => "amend",
            Right::Export => "export",
            Right::Import => "import",
        }
    }
}

impl fmt::Display for Right {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A `[[permissions]]` entry of a DocType: the rights one role has, all `false` by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRule {
    /// The role the rule is for, see [`check_role_name`].
    pub role: String,
    /// May read.
    pub read: bool,
    /// May write.
    pub write: bool,
    /// May create.
    pub create: bool,
    /// May delete.
    pub delete: bool,
    /// May submit. Only allowed on a submittable DocType.
    pub submit: bool,
    /// May cancel. Only allowed on a submittable DocType.
    pub cancel: bool,
    /// May amend. Only allowed on a submittable DocType.
    pub amend: bool,
    /// May export documents as a file (needs `read` too).
    pub export: bool,
    /// May import documents from a file (needs `create`, and `write` to upsert).
    pub import: bool,
    /// The rights apply to documents whose `owner` is the caller only. (A create is always for
    /// a new document the caller owns, so `if_owner` does not restrict create.)
    pub if_owner: bool,
}

impl PermissionRule {
    /// A rule for `role` that grants nothing yet: set the rights with struct update syntax
    /// (`PermissionRule { read: true, ..PermissionRule::new("Sales User") }`).
    pub fn new(role: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            read: false,
            write: false,
            create: false,
            delete: false,
            submit: false,
            cancel: false,
            amend: false,
            export: false,
            import: false,
            if_owner: false,
        }
    }

    /// Whether the rule grants `right` (ignoring `if_owner`).
    pub fn grants(&self, right: Right) -> bool {
        match right {
            Right::Read => self.read,
            Right::Write => self.write,
            Right::Create => self.create,
            Right::Delete => self.delete,
            Right::Submit => self.submit,
            Right::Cancel => self.cancel,
            Right::Amend => self.amend,
            Right::Export => self.export,
            Right::Import => self.import,
        }
    }
}

/// Who performs an operation: a real user, or the trusted system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// A user with the roles they have. The roles come from the database (`fb_user_role`) at the
    /// time the caller is built; this type does not check them. The user is
    /// an administrator if `roles` contains [`ADMINISTRATOR`].
    User {
        /// The username, recorded as `owner` and `modified_by` and given to hooks.
        username: String,
        /// The user's roles.
        roles: Vec<String>,
    },
    /// The trusted system context: bypasses every permission rule. For code that is not acting for
    /// a user, such as the command-line tool, migrations, jobs and tests. **Never build one from
    /// request data.** `actor` is recorded as `owner` and `modified_by` and given to hooks, like
    /// a username.
    System {
        /// Who to record.
        actor: String,
    },
}

impl Caller {
    /// A user caller.
    pub fn user(username: impl Into<String>, roles: Vec<String>) -> Self {
        Caller::User {
            username: username.into(),
            roles,
        }
    }

    /// The trusted system caller recorded as `actor`.
    pub fn system(actor: impl Into<String>) -> Self {
        Caller::System {
            actor: actor.into(),
        }
    }

    /// The name recorded as `owner` / `modified_by` and given to hooks: the username, or the
    /// system actor.
    pub fn name(&self) -> &str {
        match self {
            Caller::User { username, .. } => username,
            Caller::System { actor } => actor,
        }
    }

    /// True for the system context and for a user with the [`ADMINISTRATOR`] role: the callers
    /// the rules do not apply to.
    pub fn bypasses_permissions(&self) -> bool {
        match self {
            Caller::System { .. } => true,
            Caller::User { roles, .. } => roles.iter().any(|r| r == ADMINISTRATOR),
        }
    }
}

/// How much of a DocType's documents a caller may use a right on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Every document.
    All,
    /// Only documents whose `owner` is the caller (`if_owner`).
    OwnerOnly,
    /// No document. Not a decision about one document: it does not depend on the data.
    Denied,
}

/// Decides how far `caller` may use `right` on documents of `doctype`. The one permission
/// decision of Forgebase (see the module documentation).
///
/// - The system context and an administrator get [`Access::All`] for every right on every DocType.
/// - Otherwise the result is the union over the rules whose role the caller has and that grant
///   `right`: a rule without `if_owner` gives [`Access::All`]; if there is none, a rule with
///   `if_owner` gives [`Access::OwnerOnly`]; if there is none, [`Access::Denied`].
/// - A DocType without rules (a child DocType too) is [`Access::Denied`] for everyone else.
///
/// A role name is compared exactly (case-sensitive, like the roles of users).
pub fn evaluate(doctype: &DocType, caller: &Caller, right: Right) -> Access {
    if caller.bypasses_permissions() {
        return Access::All;
    }
    let Caller::User { roles, .. } = caller else {
        return Access::Denied; // unreachable: the system caller bypasses above
    };
    let mut access = Access::Denied;
    for rule in &doctype.permissions {
        if rule.grants(right) && roles.contains(&rule.role) {
            if !rule.if_owner {
                return Access::All;
            }
            access = Access::OwnerOnly;
        }
    }
    access
}

/// Whether `caller` may run a query report (M8.4, ADR 0016) that names `roles`: the system
/// context and an administrator always may; anyone else needs one of the roles. A report with
/// no roles is run by nobody else (deny by default). **Row-level rules do not apply inside a
/// report**: this is the whole access check.
pub fn can_run_report(roles: &[String], caller: &Caller) -> bool {
    match caller {
        Caller::System { .. } => true,
        Caller::User { roles: have, .. } => {
            caller.bypasses_permissions() || roles.iter().any(|r| have.contains(r))
        }
    }
}

/// Whether `caller` may call an app method (M9.1, ADR 0019) with this `access`: with
/// [`MethodAccess::AnyUser`] any caller; with [`MethodAccess::Roles`] the same rule as
/// [`can_run_report`] (one of the roles, or the Administrator; the system context always). This
/// is only the **gate**: the documents a method reads and writes are checked by [`evaluate`] for
/// the caller, as for any other request.
pub fn can_call_method(access: &MethodAccess, caller: &Caller) -> bool {
    match access {
        MethodAccess::AnyUser => true,
        MethodAccess::Roles(roles) => can_run_report(roles, caller),
    }
}

/// Decides how far `caller` may take `transition` of the workflow of `doctype` (M8.1, ADR
/// 0015, the transition-role rule that extends ADR 0011): **the role and the right**.
///
/// - The system context and an administrator get [`Access::All`], as in [`evaluate`].
/// - Otherwise the caller needs one of the transition's `roles` **and** the right that the
///   docstatus move needs ([`Move::right`](crate::workflow::Move::right): write, submit or
///   cancel). The answer is then what [`evaluate`] says for that right: [`Access::All`], or
///   [`Access::OwnerOnly`] when only an `if_owner` rule grants it (the caller applies it to the
///   document). A role never grants a right: a transition role without the right is
///   [`Access::Denied`], and so is the right without a transition role.
/// - A DocType without a workflow, or a transition that does not belong to it (an unknown
///   state, a docstatus move a workflow does not allow), is [`Access::Denied`].
///
/// The right is decided by [`evaluate`] and nowhere else; this function only adds the role.
pub fn transition_access(doctype: &DocType, caller: &Caller, transition: &Transition) -> Access {
    let Some(workflow) = &doctype.workflow else {
        return Access::Denied;
    };
    let Some(movement) = workflow.move_of(transition) else {
        return Access::Denied;
    };
    let has_role = match caller {
        Caller::User { roles, .. } => transition.roles.iter().any(|r| roles.contains(r)),
        Caller::System { .. } => true,
    };
    if !caller.bypasses_permissions() && !has_role {
        return Access::Denied;
    }
    evaluate(doctype, caller, movement.right())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::Naming;

    fn doctype(permissions: Vec<PermissionRule>) -> DocType {
        DocType {
            name: "thing".into(),
            label: "Thing".into(),
            naming: Naming::Random,
            fields: vec![],
            is_child: false,
            submittable: true,
            permissions,
            title_field: None,
            list_fields: None,
            icon: None,
            workflow: None,
        }
    }

    fn user(roles: &[&str]) -> Caller {
        Caller::user("u", roles.iter().map(|r| (*r).to_owned()).collect())
    }

    /// A rule granting exactly `right`.
    fn only(role: &str, right: Right, if_owner: bool) -> PermissionRule {
        let mut rule = PermissionRule::new(role);
        rule.if_owner = if_owner;
        match right {
            Right::Read => rule.read = true,
            Right::Write => rule.write = true,
            Right::Create => rule.create = true,
            Right::Delete => rule.delete = true,
            Right::Submit => rule.submit = true,
            Right::Cancel => rule.cancel = true,
            Right::Amend => rule.amend = true,
            Right::Export => rule.export = true,
            Right::Import => rule.import = true,
        }
        rule
    }

    #[test]
    fn a_report_is_for_its_roles_the_administrator_and_the_system() {
        let roles = vec!["Manager".to_owned()];
        assert!(can_run_report(&roles, &user(&["Manager"])));
        assert!(can_run_report(&roles, &user(&["Clerk", "Manager"])));
        assert!(!can_run_report(&roles, &user(&["Clerk"])));
        assert!(!can_run_report(&roles, &user(&["manager"])));
        assert!(!can_run_report(&roles, &user(&[])));
        assert!(can_run_report(&roles, &user(&[ADMINISTRATOR])));
        assert!(can_run_report(&roles, &Caller::system("cli")));
        // No roles: only the callers that bypass.
        assert!(!can_run_report(&[], &user(&["Manager"])));
        assert!(can_run_report(&[], &user(&[ADMINISTRATOR])));
    }

    #[test]
    fn a_method_is_for_its_roles_the_administrator_or_any_user() {
        let roles = MethodAccess::Roles(vec!["Manager".to_owned()]);
        assert!(can_call_method(&roles, &user(&["Manager"])));
        assert!(!can_call_method(&roles, &user(&["Clerk"])));
        assert!(!can_call_method(&roles, &user(&[])));
        assert!(can_call_method(&roles, &user(&[ADMINISTRATOR])));
        assert!(can_call_method(&MethodAccess::AnyUser, &user(&[])));
        assert!(can_call_method(&MethodAccess::AnyUser, &user(&["Clerk"])));
        // A list that somehow is empty admits nobody but the Administrator.
        let empty = MethodAccess::Roles(vec![]);
        assert!(!can_call_method(&empty, &user(&["Manager"])));
        assert!(can_call_method(&empty, &user(&[ADMINISTRATOR])));
    }

    #[test]
    fn matrix_of_rights_roles_and_if_owner() {
        for granted in Right::ALL {
            for if_owner in [false, true] {
                let d = doctype(vec![only("Clerk", granted, if_owner)]);
                for asked in Right::ALL {
                    let expected = match (asked == granted, if_owner) {
                        (false, _) => Access::Denied,
                        (true, false) => Access::All,
                        (true, true) => Access::OwnerOnly,
                    };
                    assert_eq!(
                        evaluate(&d, &user(&["Clerk"]), asked),
                        expected,
                        "granted {granted} if_owner={if_owner}, asked {asked}"
                    );
                    // Without the role nothing is granted; a similar role is not the role.
                    for other in [&[][..], &["Other"], &["clerk"], &["Clerk "]] {
                        assert_eq!(evaluate(&d, &user(other), asked), Access::Denied);
                    }
                }
            }
        }
    }

    #[test]
    fn no_rules_means_denied_except_for_administrator_and_system() {
        let d = doctype(vec![]);
        for right in Right::ALL {
            assert_eq!(evaluate(&d, &user(&["Clerk"]), right), Access::Denied);
            assert_eq!(evaluate(&d, &user(&[]), right), Access::Denied);
            assert_eq!(evaluate(&d, &user(&[ADMINISTRATOR]), right), Access::All);
            assert_eq!(
                evaluate(&d, &user(&["Clerk", ADMINISTRATOR]), right),
                Access::All
            );
            assert_eq!(evaluate(&d, &Caller::system("cli"), right), Access::All);
        }
    }

    #[test]
    fn administrator_is_not_limited_by_if_owner_rules() {
        let d = doctype(vec![only("Clerk", Right::Read, true)]);
        assert_eq!(
            evaluate(&d, &user(&[ADMINISTRATOR]), Right::Read),
            Access::All
        );
        assert_eq!(
            evaluate(&d, &user(&[ADMINISTRATOR]), Right::Delete),
            Access::All
        );
    }

    #[test]
    fn roles_union_and_the_wider_rule_wins() {
        let d = doctype(vec![
            only("Reader", Right::Read, false),
            only("Writer", Right::Write, false),
            only("Own", Right::Read, true),
            only("Own", Right::Delete, true),
        ]);
        let both = user(&["Reader", "Writer"]);
        assert_eq!(evaluate(&d, &both, Right::Read), Access::All);
        assert_eq!(evaluate(&d, &both, Right::Write), Access::All);
        assert_eq!(evaluate(&d, &both, Right::Delete), Access::Denied);
        // OwnerOnly from one role and All from another: All.
        let mixed = user(&["Own", "Reader"]);
        assert_eq!(evaluate(&d, &mixed, Right::Read), Access::All);
        assert_eq!(evaluate(&d, &mixed, Right::Delete), Access::OwnerOnly);
        // Declaration order does not matter.
        let reversed = doctype(d.permissions.iter().rev().cloned().collect());
        assert_eq!(evaluate(&reversed, &mixed, Right::Read), Access::All);
        // One rule can grant several rights.
        let multi = doctype(vec![PermissionRule {
            read: true,
            write: true,
            ..PermissionRule::new("Clerk")
        }]);
        assert_eq!(
            evaluate(&multi, &user(&["Clerk"]), Right::Write),
            Access::All
        );
        assert_eq!(
            evaluate(&multi, &user(&["Clerk"]), Right::Create),
            Access::Denied
        );
    }

    #[test]
    fn caller_helpers() {
        assert_eq!(Caller::user("ann", vec![]).name(), "ann");
        assert_eq!(Caller::system("cli").name(), "cli");
        assert!(Caller::system("cli").bypasses_permissions());
        assert!(user(&[ADMINISTRATOR]).bypasses_permissions());
        assert!(!user(&["administrator"]).bypasses_permissions());
        assert!(!user(&[]).bypasses_permissions());
    }

    #[test]
    fn right_names() {
        let names: Vec<&str> = Right::ALL.iter().map(|r| r.name()).collect();
        assert_eq!(
            names,
            [
                "read", "write", "create", "delete", "submit", "cancel", "amend", "export",
                "import"
            ]
        );
        assert_eq!(Right::Amend.to_string(), "amend");
    }

    #[test]
    fn role_name_rules() {
        assert!(check_role_name("Sales Manager").is_ok());
        assert!(check_role_name("Verkäufer").is_ok());
        assert!(check_role_name(&"r".repeat(140)).is_ok());
        assert!(check_role_name(&"r".repeat(141)).is_err());
        for bad in ["", "   ", "a\tb", "a\0b", "\n"] {
            assert!(check_role_name(bad).is_err(), "{bad:?}");
        }
    }

    fn with_workflow(permissions: Vec<PermissionRule>) -> DocType {
        let mut doctype = DocType::from_toml_str(
            r#"
name = "order"
label = "Order"
submittable = true
[workflow]
[[workflow.states]]
name = "Draft"
docstatus = 0
[[workflow.states]]
name = "Pending"
docstatus = 0
[[workflow.states]]
name = "Approved"
docstatus = 1
[[workflow.states]]
name = "Cancelled"
docstatus = 2
[[workflow.transitions]]
action = "Send"
from = "Draft"
to = "Pending"
roles = ["Clerk"]
[[workflow.transitions]]
action = "Approve"
from = "Pending"
to = "Approved"
roles = ["Manager"]
[[workflow.transitions]]
action = "Cancel"
from = "Approved"
to = "Cancelled"
roles = ["Manager"]
"#,
        )
        .expect("valid DocType");
        doctype.permissions = permissions;
        doctype
    }

    fn take(doctype: &DocType, caller: &Caller, action: &str) -> Access {
        let workflow = doctype.workflow.as_ref().expect("workflow");
        let transition = workflow
            .transitions
            .iter()
            .find(|t| t.action == action)
            .expect("transition");
        transition_access(doctype, caller, transition)
    }

    #[test]
    fn a_transition_needs_a_role_and_the_right_of_its_move() {
        let d = with_workflow(vec![
            only("Clerk", Right::Write, false),
            only("Manager", Right::Submit, false),
            only("Auditor", Right::Cancel, false),
            only("Owner", Right::Submit, true),
        ]);
        // Draft to draft needs write: Clerk has the role and the right.
        assert_eq!(take(&d, &user(&["Clerk"]), "Send"), Access::All);
        // Approve moves 0 to 1, which needs the submit right: Manager has the role and the right.
        assert_eq!(take(&d, &user(&["Manager"]), "Approve"), Access::All);
        // The role without the right: Manager has no cancel right.
        assert_eq!(take(&d, &user(&["Manager"]), "Cancel"), Access::Denied);
        // The right without the role: Auditor may cancel but is not a role of Cancel.
        assert_eq!(take(&d, &user(&["Auditor"]), "Cancel"), Access::Denied);
        assert_eq!(
            take(&d, &user(&["Manager", "Auditor"]), "Cancel"),
            Access::All
        );
        // A user with neither.
        assert_eq!(take(&d, &user(&["Other"]), "Send"), Access::Denied);
        assert_eq!(take(&d, &user(&[]), "Send"), Access::Denied);
        // A similar role name is not the role.
        assert_eq!(take(&d, &user(&["clerk"]), "Send"), Access::Denied);
    }

    #[test]
    fn if_owner_limits_the_right_of_a_transition() {
        let d = with_workflow(vec![
            only("Clerk", Right::Write, true),
            only("Manager", Right::Submit, true),
            only("Manager", Right::Cancel, false),
        ]);
        assert_eq!(take(&d, &user(&["Clerk"]), "Send"), Access::OwnerOnly);
        assert_eq!(take(&d, &user(&["Manager"]), "Approve"), Access::OwnerOnly);
        assert_eq!(take(&d, &user(&["Manager"]), "Cancel"), Access::All);
    }

    #[test]
    fn administrator_and_system_pass_the_role_check() {
        // No rules at all, and the roles of the transitions are not theirs.
        let d = with_workflow(vec![]);
        for action in ["Send", "Approve", "Cancel"] {
            assert_eq!(take(&d, &user(&[ADMINISTRATOR]), action), Access::All);
            assert_eq!(take(&d, &Caller::system("cli"), action), Access::All);
            assert_eq!(
                take(&d, &user(&["Clerk", "Manager"]), action),
                Access::Denied
            );
        }
    }

    #[test]
    fn no_workflow_or_a_foreign_transition_is_denied() {
        let plain = doctype(vec![only("Clerk", Right::Write, false)]);
        let foreign = Transition {
            action: "Send".into(),
            from: "Draft".into(),
            to: "Pending".into(),
            roles: vec!["Clerk".into()],
        };
        assert_eq!(
            transition_access(&plain, &Caller::system("cli"), &foreign),
            Access::Denied
        );
        let d = with_workflow(vec![]);
        let unknown_state = Transition {
            to: "Nowhere".into(),
            ..foreign
        };
        assert_eq!(
            transition_access(&d, &Caller::system("cli"), &unknown_state),
            Access::Denied
        );
    }
}

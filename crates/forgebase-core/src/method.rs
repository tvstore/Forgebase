//! App method endpoints (M9.1, ADR 0019): the definition of a method, and the checks that need
//! no database.
//!
//! A method is a function an app compiled into the binary, called over HTTP as
//! `POST /api/v1/method/<app>.<method>`. This module is pure data and rules: it holds the name
//! and who may call it (never the handler, which lives in the `forgebase` crate), so that the
//! per-site registry decides which methods exist on a site, as it does for reports.

use crate::ident::validate_doctype_name;
use crate::permission::check_role_name;

/// Who may call a method. There is no guest access: every call is authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodAccess {
    /// Users that have at least one of these roles (and the Administrator). Never empty in a
    /// valid method.
    Roles(Vec<String>),
    /// Any authenticated user, whatever their roles.
    AnyUser,
}

/// A method: its app, its name and who may call it. Plain data with public fields, like
/// [`ReportDef`](crate::report::ReportDef); check it with [`MethodDef::problems`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDef {
    /// The app that defines it.
    pub app: String,
    /// The name, unique within the app. Same rule as a DocType name.
    pub name: String,
    /// Who may call it.
    pub access: MethodAccess,
}

impl MethodDef {
    /// `<app>.<name>`: how the method is known in the API.
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.app, self.name)
    }

    /// What is wrong with the definition itself, one message per problem.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Err(reason) = validate_doctype_name(&self.name) {
            problems.push(format!("name {:?} {reason}", self.name));
        }
        if let MethodAccess::Roles(roles) = &self.access {
            if roles.is_empty() {
                problems.push(
                    "an empty role list: name the roles that may call it with .roles(&[..]), or use .any_user()"
                        .to_owned(),
                );
            }
            for role in roles {
                if let Err(reason) = check_role_name(role) {
                    problems.push(format!("invalid role {role:?}: {reason}"));
                }
            }
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, access: MethodAccess) -> MethodDef {
        MethodDef {
            app: "sales".to_owned(),
            name: name.to_owned(),
            access,
        }
    }

    #[test]
    fn the_full_name_is_app_dot_name() {
        assert_eq!(def("ping", MethodAccess::AnyUser).full_name(), "sales.ping");
    }

    #[test]
    fn a_valid_definition_has_no_problems() {
        assert!(def("ping", MethodAccess::AnyUser).problems().is_empty());
        let roles = MethodAccess::Roles(vec!["Sales User".to_owned()]);
        assert!(def("order_summary", roles).problems().is_empty());
    }

    #[test]
    fn a_bad_name_an_empty_list_and_a_bad_role_are_each_a_problem() {
        let problems = def("Bad Name", MethodAccess::Roles(vec![])).problems();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].starts_with("name \"Bad Name\""), "{problems:?}");
        assert!(problems[1].contains("empty role list"), "{problems:?}");
        let problems = def("ok", MethodAccess::Roles(vec![" ".to_owned()])).problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("invalid role"), "{problems:?}");
    }
}

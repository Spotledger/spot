//! `with_permissions`: a test user that holds exactly the rights a test names.
//!
//! The harness turns the list into one synthetic role (`testkit-<user>`) and appends its grants
//! to the apps' own permission texts; the snapshot is then rebuilt and swapped in, so every check
//! after that is the engine's own. A right the engine does not know, or a DocType that does not
//! exist, is refused by the snapshot build, exactly as it would be in an app's `grants.toml`.

use crate::error::{Result, TestkitError};

/// One grant: rights on one DocType, optionally only on the user's own documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    doctype: String,
    rights: Vec<String>,
    if_owner: bool,
}

impl Grant {
    /// Grants `rights` (`"read"`, `"write"`, `"create"`, `"delete"`, `"submit"`, `"cancel"`,
    /// `"correct"`, `"report"`, `"select"`) on `doctype`.
    #[must_use]
    pub fn on(doctype: &str, rights: &[&str]) -> Self {
        Self {
            doctype: doctype.to_owned(),
            rights: rights.iter().map(|r| (*r).to_owned()).collect(),
            if_owner: false,
        }
    }

    /// Limits the grant to documents the user owns.
    #[must_use]
    pub fn if_owner(mut self) -> Self {
        self.if_owner = true;
        self
    }
}

/// The synthetic role of `user`.
pub(crate) fn role_of(user: &str) -> String {
    format!("testkit-{user}")
}

/// Whether `text` is safe to put between double quotes in the generated TOML: plain names only.
fn plain(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b' '))
}

/// The grants TOML of `grants` for `user`'s synthetic role.
///
/// # Errors
///
/// `app_testkit.app_invalid` for a name or right that is not a plain name (nothing is generated).
pub(crate) fn toml_for(user: &str, grants: &[Grant]) -> Result<String> {
    let role = role_of(user);
    if !plain(&role) {
        return Err(TestkitError::invalid(
            "the user name cannot make a role",
            vec![
                "user names for with_permissions are letters, digits, `_`, `-`, `.` and spaces"
                    .to_owned(),
            ],
        ));
    }
    let mut out = String::new();
    for grant in grants {
        if !plain(&grant.doctype)
            || grant.rights.is_empty()
            || !grant.rights.iter().all(|r| plain(r))
        {
            return Err(TestkitError::invalid(
                "a grant is not well formed",
                vec!["a grant names a plain DocType and at least one plain right".to_owned()],
            ));
        }
        let rights: Vec<String> = grant.rights.iter().map(|r| format!("\"{r}\"")).collect();
        out.push_str("[[entries]]\n");
        out.push_str(&format!("role = \"{role}\"\n"));
        out.push_str(&format!("doctype = \"{}\"\n", grant.doctype));
        out.push_str(&format!("rights = [{}]\n", rights.join(", ")));
        if grant.if_owner {
            out.push_str("if_owner = true\n");
        }
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn grants_become_toml_entries() {
        let text = toml_for(
            "zed",
            &[Grant::on("ticket", &["read", "create"]).if_owner()],
        )
        .unwrap();
        assert!(text.contains("role = \"testkit-zed\""));
        assert!(text.contains("doctype = \"ticket\""));
        assert!(text.contains("rights = [\"read\", \"create\"]"));
        assert!(text.contains("if_owner = true"));
    }

    #[test]
    fn a_name_that_could_break_the_toml_is_refused() {
        assert!(toml_for("zed", &[Grant::on("tick\"et", &["read"])]).is_err());
        assert!(toml_for("zed", &[Grant::on("ticket", &["read\n"])]).is_err());
        assert!(toml_for("zed", &[Grant::on("ticket", &[])]).is_err());
        assert!(toml_for("z\"ed", &[Grant::on("ticket", &["read"])]).is_err());
    }
}

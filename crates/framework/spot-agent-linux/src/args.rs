//! Reading and re-validating an operation's arguments.
//!
//! The catalogue validates an invocation against the operation's declared schema before a plan
//! is built; a handler then re-validates the values it actually uses, because the schema is
//! deliberately loose (a text parameter is text) and the module knows the narrower shape it
//! needs. [`Args`] reads a typed value out of a [`Validated`] invocation; the `is_*` predicates
//! are the module's own rules. Every refusal names the operation and the parameter and never
//! echoes the value.

use std::collections::BTreeMap;

use spot_mgmt_ops::{AcceptedParam, OpId, ParamValue, Validated};

use crate::error::LinuxError;

/// The longest account or group name the module accepts, per `^[a-z_][a-z0-9_-]{0,31}$`.
const MAX_ACCOUNT_NAME: usize = 32;
/// The longest package name the module accepts.
const MAX_PACKAGE_NAME: usize = 128;
/// The longest unit name the module accepts.
const MAX_UNIT_NAME: usize = 128;

/// A typed, re-validating view of one validated invocation.
#[derive(Clone)]
pub struct Args<'a> {
    op: &'a OpId,
    params: BTreeMap<&'a str, &'a AcceptedParam>,
}

impl std::fmt::Debug for Args<'_> {
    /// Prints the operation id and the parameter names, never a value.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Args")
            .field("op", &self.op.as_str())
            .field("params", &self.params.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl<'a> Args<'a> {
    /// Wraps a validated invocation.
    #[must_use]
    pub fn new(validated: &'a Validated) -> Self {
        let params = validated
            .params
            .iter()
            .map(|param| (param.name.as_str(), param))
            .collect();
        Self {
            op: &validated.op,
            params,
        }
    }

    /// The operation id.
    #[must_use]
    pub fn op(&self) -> &'a OpId {
        self.op
    }

    /// The operation id as text, for error messages.
    fn op_name(&self) -> String {
        self.op.as_str().to_owned()
    }

    /// Builds an invalid-argument error for `param`.
    fn invalid(&self, param: &str, detail: &str) -> LinuxError {
        LinuxError::InvalidArgument {
            op: self.op_name(),
            param: param.to_owned(),
            detail: detail.to_owned(),
        }
    }

    /// The parameter named `name`, or a refusal when the invocation carries no value for it.
    fn param(&self, name: &str) -> Result<&'a AcceptedParam, LinuxError> {
        self.params
            .get(name)
            .copied()
            .ok_or_else(|| self.invalid(name, "the invocation carries no value for this parameter"))
    }

    /// The text value of `name`.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when `name` is absent or is not text.
    pub fn text(&self, name: &str) -> Result<&'a str, LinuxError> {
        match &self.param(name)?.value {
            ParamValue::Text(value) => Ok(value.as_str()),
            _ => Err(self.invalid(name, "expected a text value")),
        }
    }

    /// The boolean value of `name`.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when `name` is absent or is not a boolean.
    pub fn bool(&self, name: &str) -> Result<bool, LinuxError> {
        match &self.param(name)?.value {
            ParamValue::Bool(value) => Ok(*value),
            _ => Err(self.invalid(name, "expected a boolean value")),
        }
    }

    /// The integer value of `name`.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when `name` is absent or is not an integer.
    pub fn int(&self, name: &str) -> Result<i64, LinuxError> {
        match &self.param(name)?.value {
            ParamValue::Int(value) => Ok(*value),
            _ => Err(self.invalid(name, "expected an integer value")),
        }
    }

    /// The duration of `name`, in whole seconds.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when `name` is absent or is not a duration.
    pub fn duration(&self, name: &str) -> Result<u64, LinuxError> {
        match &self.param(name)?.value {
            ParamValue::Duration(value) => Ok(*value),
            _ => Err(self.invalid(name, "expected a duration value")),
        }
    }

    /// The list of text values of `name`.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when `name` is absent, is not a list, or holds a
    /// non-text element.
    pub fn list_text(&self, name: &str) -> Result<Vec<&'a str>, LinuxError> {
        match &self.param(name)?.value {
            ParamValue::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        ParamValue::Text(value) => out.push(value.as_str()),
                        _ => return Err(self.invalid(name, "expected a list of text values")),
                    }
                }
                Ok(out)
            }
            _ => Err(self.invalid(name, "expected a list of text values")),
        }
    }

    /// The text value of `name`, or `None` when the invocation omits it.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when `name` is present and is not text.
    pub fn optional_text(&self, name: &str) -> Result<Option<&'a str>, LinuxError> {
        match self.params.get(name).copied() {
            None => Ok(None),
            Some(param) => match &param.value {
                ParamValue::Text(value) => Ok(Some(value.as_str())),
                _ => Err(self.invalid(name, "expected a text value")),
            },
        }
    }

    /// The boolean value of `name`, defaulting to `false` when the invocation omits it.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when `name` is present and is not a boolean.
    pub fn flag(&self, name: &str) -> Result<bool, LinuxError> {
        match self.params.get(name).copied() {
            None => Ok(false),
            Some(param) => match &param.value {
                ParamValue::Bool(value) => Ok(*value),
                _ => Err(self.invalid(name, "expected a boolean value")),
            },
        }
    }

    /// The integer value of `name`, or `None` when the invocation omits it.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when `name` is present and is not an integer.
    pub fn optional_int(&self, name: &str) -> Result<Option<i64>, LinuxError> {
        match self.params.get(name).copied() {
            None => Ok(None),
            Some(param) => match &param.value {
                ParamValue::Int(value) => Ok(Some(*value)),
                _ => Err(self.invalid(name, "expected an integer value")),
            },
        }
    }

    /// The list of text values of `name`, or an empty vector when the invocation omits it.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when `name` is present and is not a list of text.
    pub fn optional_list_text(&self, name: &str) -> Result<Vec<&'a str>, LinuxError> {
        if self.params.contains_key(name) {
            self.list_text(name)
        } else {
            Ok(Vec::new())
        }
    }

    /// A validated account or group name.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when the value is not `^[a-z_][a-z0-9_-]{0,31}$`.
    pub fn account_name(&self, name: &str) -> Result<&'a str, LinuxError> {
        let value = self.text(name)?;
        if !is_account_name(value) {
            return Err(self.invalid(name, "not a valid account name"));
        }
        Ok(value)
    }

    /// A validated list of package names (each one, in order).
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when the list is empty or holds a name outside the
    /// allow-listed package charset.
    pub fn package_names(&self, name: &str) -> Result<Vec<&'a str>, LinuxError> {
        let values = self.list_text(name)?;
        if values.is_empty() {
            return Err(self.invalid(name, "at least one package name is required"));
        }
        for value in &values {
            if !is_package_name(value) {
                return Err(self.invalid(name, "a package name is outside the allowed charset"));
            }
        }
        Ok(values)
    }

    /// A validated systemd unit name.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when the value is not a unit name of a known type.
    pub fn unit_name(&self, name: &str) -> Result<&'a str, LinuxError> {
        let value = self.text(name)?;
        if !is_unit_name(value) {
            return Err(self.invalid(name, "not a valid systemd unit name"));
        }
        Ok(value)
    }

    /// A validated firewall source (an address or CIDR).
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when the value is not an address or CIDR.
    pub fn source(&self, name: &str) -> Result<&'a str, LinuxError> {
        let value = self.text(name)?;
        if !is_source(value) {
            return Err(self.invalid(name, "not a valid address or CIDR"));
        }
        Ok(value)
    }

    /// A validated firewalld zone name.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when the value is not a zone name.
    pub fn zone(&self, name: &str) -> Result<&'a str, LinuxError> {
        let value = self.text(name)?;
        if !is_zone(value) {
            return Err(self.invalid(name, "not a valid firewalld zone name"));
        }
        Ok(value)
    }

    /// A validated absolute shell path.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when the value is not an absolute, safe path.
    pub fn shell(&self, name: &str) -> Result<&'a str, LinuxError> {
        let value = self.text(name)?;
        if !is_absolute_path(value) {
            return Err(self.invalid(name, "not an absolute path"));
        }
        Ok(value)
    }

    /// A validated absolute home path.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when the value is not an absolute, safe path.
    pub fn home(&self, name: &str) -> Result<&'a str, LinuxError> {
        self.shell(name)
    }

    /// A port number, already range-checked by the schema and re-checked here.
    ///
    /// # Errors
    /// [`LinuxError::InvalidArgument`] when the value is outside `1..=65535`.
    pub fn port(&self, name: &str) -> Result<u16, LinuxError> {
        let value = self.int(name)?;
        u16::try_from(value)
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| self.invalid(name, "a port must be between 1 and 65535"))
    }
}

/// Whether `value` is a valid account or group name: `^[a-z_][a-z0-9_-]{0,31}$`.
#[must_use]
pub fn is_account_name(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first == '_' => {}
        _ => return false,
    }
    value.len() <= MAX_ACCOUNT_NAME
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Whether `value` is a package name in the allow-listed charset: one leading alphanumeric
/// (never a dash, so it can never be mistaken for a flag), then letters, digits, `.`, `_`,
/// `+`, `-` or `:`.
#[must_use]
pub fn is_package_name(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    value.len() <= MAX_PACKAGE_NAME
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-' | ':'))
}

/// Whether `value` is a systemd unit name of a known type: `^[A-Za-z0-9][A-Za-z0-9@._:-]*` with
/// a final `.` segment naming a real unit type. Never a leading dash.
#[must_use]
pub fn is_unit_name(value: &str) -> bool {
    if value.len() > MAX_UNIT_NAME || value.starts_with('-') {
        return false;
    }
    let Some((stem, kind)) = value.rsplit_once('.') else {
        return false;
    };
    if stem.is_empty() || stem.starts_with('-') {
        return false;
    }
    let stem_ok = stem
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | ':' | '-'));
    stem_ok
        && matches!(
            kind,
            "service"
                | "socket"
                | "timer"
                | "target"
                | "mount"
                | "automount"
                | "path"
                | "slice"
                | "scope"
                | "device"
                | "swap"
        )
}

/// Whether `value` is a firewall source: an IPv4/IPv6 address, optionally with a `/prefix`.
#[must_use]
pub fn is_source(value: &str) -> bool {
    if value.is_empty() || value.len() > 64 || value.starts_with('-') {
        return false;
    }
    let (address, prefix) = match value.split_once('/') {
        Some((address, prefix)) => (address, Some(prefix)),
        None => (value, None),
    };
    if address.is_empty()
        || !address
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':')
    {
        return false;
    }
    match prefix {
        None => true,
        Some(prefix) => {
            !prefix.is_empty()
                && prefix.bytes().all(|b| b.is_ascii_digit())
                && prefix.parse::<u8>().ok().is_some_and(|bits| bits <= 128)
        }
    }
}

/// Whether `value` is a firewalld zone name: `^[A-Za-z][A-Za-z0-9_-]{0,31}$`.
#[must_use]
pub fn is_zone(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    value.len() <= 32 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Whether `value` is an absolute, safe filesystem path: it starts with `/`, carries no control
/// character, no whitespace, no `..` segment and no shell metacharacter.
#[must_use]
pub fn is_absolute_path(value: &str) -> bool {
    if !value.starts_with('/') || value.len() > 256 {
        return false;
    }
    let safe = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+'));
    safe && !value.split('/').any(|segment| segment == "..")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_mgmt_ops::validated::AcceptedParam;
    use spot_mgmt_ops::{OpId, ParamValue, Validated};

    use super::*;

    fn validated(params: Vec<AcceptedParam>) -> Validated {
        Validated {
            op: OpId::new("os.package.install").unwrap(),
            params,
        }
    }

    fn param(name: &str, value: ParamValue) -> AcceptedParam {
        AcceptedParam {
            name: name.to_owned(),
            ty: spot_mgmt_ops::ParamType::Bool,
            value,
            from_default: false,
            secret: false,
        }
    }

    #[test]
    fn account_names_follow_the_documented_shape() {
        assert!(is_account_name("alice"));
        assert!(is_account_name("_svc"));
        assert!(is_account_name("a-b_c9"));
        assert!(!is_account_name(""));
        assert!(!is_account_name("Alice"));
        assert!(!is_account_name("9alice"));
        assert!(!is_account_name("-alice"));
        assert!(!is_account_name("alice;rm"));
        assert!(!is_account_name(&"a".repeat(33)));
    }

    #[test]
    fn package_names_reject_flags_and_shell_metacharacters() {
        assert!(is_package_name("nginx"));
        assert!(is_package_name("python3.12"));
        assert!(is_package_name("gcc-c++"));
        assert!(is_package_name("libstdc++"));
        assert!(is_package_name("kernel:5.14"));
        assert!(!is_package_name("-y"));
        assert!(!is_package_name(""));
        assert!(!is_package_name("nginx;rm -rf /"));
        assert!(!is_package_name("a b"));
        assert!(!is_package_name("a/b"));
    }

    #[test]
    fn unit_names_require_a_known_type() {
        assert!(is_unit_name("sshd.service"));
        assert!(is_unit_name("getty@tty1.service"));
        assert!(is_unit_name("var-lib.mount"));
        assert!(!is_unit_name("sshd"));
        assert!(!is_unit_name("-rf.service"));
        assert!(!is_unit_name("sshd.exe"));
        assert!(!is_unit_name("a b.service"));
    }

    #[test]
    fn sources_accept_addresses_and_cidrs() {
        assert!(is_source("10.0.0.0/8"));
        assert!(is_source("192.168.1.5"));
        assert!(is_source("2001:db8::/32"));
        assert!(!is_source(""));
        assert!(!is_source("example.com"));
        assert!(!is_source("10.0.0.0/129"));
        assert!(!is_source("-10.0.0.1"));
    }

    #[test]
    fn absolute_paths_reject_traversal_and_metacharacters() {
        assert!(is_absolute_path("/bin/bash"));
        assert!(is_absolute_path("/home/alice"));
        assert!(!is_absolute_path("bin/bash"));
        assert!(!is_absolute_path("/home/../etc"));
        assert!(!is_absolute_path("/home/a b"));
        assert!(!is_absolute_path("/home/a;rm"));
    }

    #[test]
    fn args_reads_typed_values_and_refuses_the_wrong_shape() {
        let invocation = validated(vec![
            param(
                "packages",
                ParamValue::List(vec![ParamValue::Text("nginx".to_owned())]),
            ),
            param("enabled", ParamValue::Bool(true)),
        ]);
        let args = Args::new(&invocation);
        assert_eq!(args.package_names("packages").unwrap(), vec!["nginx"]);
        assert!(args.bool("enabled").unwrap());
        assert!(args.text("enabled").is_err());
        assert!(args.bool("missing").is_err());
        assert!(!args.flag("missing").unwrap());
    }

    #[test]
    fn args_rejects_a_leading_dash_package() {
        let invocation = validated(vec![param(
            "packages",
            ParamValue::List(vec![ParamValue::Text("-y".to_owned())]),
        )]);
        let args = Args::new(&invocation);
        let error = args.package_names("packages").unwrap_err();
        assert_eq!(error.code(), crate::error::INVALID_ARGUMENT);
    }
}

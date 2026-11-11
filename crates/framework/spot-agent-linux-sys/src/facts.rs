//! Block-device and mount facts: `lsblk -J` and `findmnt -J` parsed into typed structs,
//! plus free space and filesystem type for a path.
//!
//! Every value is integer: sizes are bytes, never a float (ADR-12). A tool that prints a
//! human size ("500G") is parsed into bytes with integer arithmetic, so a `lsblk` without
//! `-b` still yields exact numbers.

use std::path::Path;

use serde_json::Value;

use crate::error::{SysError, SysResult};
use crate::sys::{CommandOutput, CommandRunner, CommandSpec};

/// The `lsblk` columns the facts request, in one fixed order.
pub const LSBLK_COLUMNS: &str = "NAME,KNAME,TYPE,SIZE,FSTYPE,MOUNTPOINT,UUID,LABEL,ROTA";

/// The `findmnt` columns the facts request.
pub const FINDMNT_COLUMNS: &str = "TARGET,SOURCE,FSTYPE,OPTIONS";

/// One block device or partition, as `lsblk` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockDevice {
    /// The device name (`sda`, `sda1`).
    pub name: String,
    /// The kernel device name.
    pub kname: String,
    /// The type (`disk`, `part`, `lvm`, `crypt`, `rom`).
    pub kind: String,
    /// The size in bytes.
    pub size_bytes: u64,
    /// The filesystem type, when one is present.
    pub fstype: Option<String>,
    /// The mount point, when the device is mounted.
    pub mountpoint: Option<String>,
    /// The filesystem UUID.
    pub uuid: Option<String>,
    /// The filesystem label.
    pub label: Option<String>,
    /// Whether the device is rotational (a spinning disk).
    pub rotational: bool,
    /// The child devices (partitions, logical volumes).
    pub children: Vec<BlockDevice>,
}

impl BlockDevice {
    /// Every device in the tree, parents before children (depth-first).
    #[must_use]
    pub fn flatten(&self) -> Vec<&BlockDevice> {
        let mut out = vec![self];
        for child in &self.children {
            out.extend(child.flatten());
        }
        out
    }

    /// The device mounted at exactly `target`, if any, in this subtree.
    #[must_use]
    pub fn mounted_at(&self, target: &str) -> Option<&BlockDevice> {
        self.flatten()
            .into_iter()
            .find(|device| device.mountpoint.as_deref() == Some(target))
    }
}

/// One mounted filesystem, as `findmnt` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// The mount point.
    pub target: String,
    /// The device or source.
    pub source: String,
    /// The filesystem type.
    pub fstype: String,
    /// The mount options, split on commas.
    pub options: Vec<String>,
    /// The mounts beneath this one.
    pub children: Vec<Mount>,
}

impl Mount {
    /// Whether the mount carries the given option.
    #[must_use]
    pub fn has_option(&self, option: &str) -> bool {
        self.options.iter().any(|entry| entry == option)
    }

    /// Every mount in the tree, parents before children.
    #[must_use]
    pub fn flatten(&self) -> Vec<&Mount> {
        let mut out = vec![self];
        for child in &self.children {
            out.extend(child.flatten());
        }
        out
    }
}

/// The facts reader: it runs the two fact commands through the runner and parses them.
pub struct FactsReader<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> FactsReader<'a> {
    /// Wraps a command runner.
    #[must_use]
    pub fn new(runner: &'a dyn CommandRunner) -> Self {
        Self { runner }
    }

    /// The block-device tree.
    ///
    /// # Errors
    /// The runner's failures and [`crate::error::PARSE`] for malformed JSON.
    pub fn block_devices(&self) -> SysResult<Vec<BlockDevice>> {
        let spec = CommandSpec::new("lsblk")?.args(["-J", "-b", "-o", LSBLK_COLUMNS])?;
        let output = self.runner.run(&spec)?;
        output.require_success("listing block devices")?;
        output.require_complete("listing block devices")?;
        parse_lsblk(output.stdout())
    }

    /// The mount tree.
    ///
    /// # Errors
    /// The runner's failures and [`crate::error::PARSE`] for malformed JSON.
    pub fn mounts(&self) -> SysResult<Vec<Mount>> {
        let spec = CommandSpec::new("findmnt")?.args(["-J", "-o", FINDMNT_COLUMNS])?;
        let output = self.runner.run(&spec)?;
        output.require_success("listing mounts")?;
        output.require_complete("listing mounts")?;
        parse_findmnt(output.stdout())
    }

    /// The bytes available on the filesystem that holds `path`.
    ///
    /// # Errors
    /// The runner's failures and [`crate::error::PARSE`] when `df` prints no usable row.
    pub fn free_space(&self, path: &Path) -> SysResult<u64> {
        let spec = CommandSpec::new("df")?.args(["-P", "-B1", &path.display().to_string()])?;
        let output = self.runner.run(&spec)?;
        output.require_success("reading free space")?;
        parse_df_avail(output.stdout())
    }

    /// The filesystem type that holds `path`: the mount whose target is the longest
    /// prefix of the path (the deepest mount wins).
    ///
    /// # Errors
    /// The runner's failures and [`crate::error::PARSE`] for malformed JSON.
    pub fn filesystem_type(&self, path: &Path) -> SysResult<Option<String>> {
        let mounts = self.mounts()?;
        let target = normalize(path);
        let mut best: Option<(usize, &Mount)> = None;
        for mount in mounts.iter().flat_map(Mount::flatten) {
            let base = normalize(Path::new(&mount.target));
            if target == base || target.starts_with(&format!("{base}/")) || base == "/" {
                let depth = base.len();
                if best.is_none_or(|(length, _)| depth >= length) {
                    best = Some((depth, mount));
                }
            }
        }
        Ok(best.map(|(_, mount)| mount.fstype.clone()))
    }
}

/// Normalises a path for prefix comparison: a single leading slash and no trailing slash.
fn normalize(path: &Path) -> String {
    let text = path.display().to_string();
    let trimmed = text.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Parses the `lsblk -J` document.
///
/// # Errors
/// [`crate::error::PARSE`] when the top level is not an object with a `blockdevices` array,
/// or a device is missing its name.
pub fn parse_lsblk(text: &str) -> SysResult<Vec<BlockDevice>> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| SysError::parse(format!("lsblk JSON: {e}")))?;
    let devices = value
        .get("blockdevices")
        .and_then(Value::as_array)
        .ok_or_else(|| SysError::parse("lsblk JSON has no blockdevices array"))?;
    devices.iter().map(parse_block_device).collect()
}

/// Parses one `lsblk` device (recursively through its children).
fn parse_block_device(value: &Value) -> SysResult<BlockDevice> {
    let name = string_field(value, "name")
        .ok_or_else(|| SysError::parse("an lsblk device has no name"))?;
    let kname = string_field(value, "kname").unwrap_or_else(|| name.clone());
    let kind = string_field(value, "type").unwrap_or_default();
    let size_bytes = match value.get("size") {
        Some(Value::Number(number)) => number.as_u64().unwrap_or(0),
        Some(Value::String(text)) => parse_size(text).unwrap_or(0),
        _ => 0,
    };
    let children = value
        .get("children")
        .and_then(Value::as_array)
        .map(|children| {
            children
                .iter()
                .map(parse_block_device)
                .collect::<SysResult<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(BlockDevice {
        name,
        kname,
        kind,
        size_bytes,
        fstype: string_field(value, "fstype"),
        mountpoint: string_field(value, "mountpoint"),
        uuid: string_field(value, "uuid"),
        label: string_field(value, "label"),
        rotational: bool_field(value, "rota"),
        children,
    })
}

/// Parses the `findmnt -J` document.
///
/// # Errors
/// [`crate::error::PARSE`] when the top level is not an object with a `filesystems` array,
/// or a mount is missing its target.
pub fn parse_findmnt(text: &str) -> SysResult<Vec<Mount>> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| SysError::parse(format!("findmnt JSON: {e}")))?;
    let mounts = value
        .get("filesystems")
        .and_then(Value::as_array)
        .ok_or_else(|| SysError::parse("findmnt JSON has no filesystems array"))?;
    mounts.iter().map(parse_mount).collect()
}

/// Parses one `findmnt` mount (recursively through its children).
fn parse_mount(value: &Value) -> SysResult<Mount> {
    let target = string_field(value, "target")
        .ok_or_else(|| SysError::parse("a findmnt mount has no target"))?;
    let options = string_field(value, "options")
        .map(|options| options.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let children = value
        .get("children")
        .and_then(Value::as_array)
        .map(|children| {
            children
                .iter()
                .map(parse_mount)
                .collect::<SysResult<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(Mount {
        target,
        source: string_field(value, "source").unwrap_or_default(),
        fstype: string_field(value, "fstype").unwrap_or_default(),
        options,
        children,
    })
}

/// A string field of a JSON object, when it is present and a string.
fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// A boolean field of a JSON object: a JSON boolean, `"1"`/`"true"`, or a non-zero number.
fn bool_field(value: &Value, key: &str) -> bool {
    match value.get(key) {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => text == "1" || text == "true",
        Some(Value::Number(number)) => number.as_u64().is_some_and(|n| n != 0),
        _ => false,
    }
}

/// Parses a size into bytes: either plain digits, or digits with a `K`/`M`/`G`/`T`/`P`/`E`
/// suffix (optionally followed by `i` or `B`), powers of 1024. Integer arithmetic only.
///
/// Returns `None` for anything else, including overflow.
#[must_use]
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (digits, suffix) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => text.split_at(at),
        None => (text, ""),
    };
    let base: u64 = digits.parse().ok()?;
    let exponent = match suffix
        .trim_end_matches(['B', 'b'])
        .trim_end_matches(['i', 'I'])
        .to_ascii_uppercase()
        .as_str()
    {
        "" => 0u32,
        "K" => 1,
        "M" => 2,
        "G" => 3,
        "T" => 4,
        "P" => 5,
        "E" => 6,
        _ => return None,
    };
    let mut value = base;
    for _ in 0..exponent {
        value = value.checked_mul(1024)?;
    }
    Some(value)
}

/// Parses the available-bytes column from `df -P -B1` output (the last non-empty line,
/// fourth field).
///
/// # Errors
/// [`crate::error::PARSE`] when no data row or no numeric column is present.
pub fn parse_df_avail(text: &str) -> SysResult<u64> {
    let row = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| SysError::parse("df printed nothing"))?;
    let fields: Vec<&str> = row.split_whitespace().collect();
    let available = fields
        .get(3)
        .ok_or_else(|| SysError::parse("df printed fewer than four columns"))?;
    available
        .parse()
        .map_err(|_| SysError::parse("df's available column is not a number"))
}

/// Reads a whole command output as the facts reader would, for a caller that already ran
/// the command itself.
///
/// # Errors
/// [`crate::error::PARSE`] when the command did not succeed or its output is malformed.
pub fn block_devices_from(output: &CommandOutput) -> SysResult<Vec<BlockDevice>> {
    output.require_success("listing block devices")?;
    parse_lsblk(output.stdout())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn sizes_parse_with_integer_arithmetic() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("500G"), Some(500 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1.5G"), None);
        assert_eq!(parse_size("512MiB"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size("2T"), Some(2 * 1024 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("nonsense"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("999999999999999999999E"), None);
    }

    #[test]
    fn lsblk_parses_the_tree() {
        let json = r#"{"blockdevices":[
            {"name":"sda","kname":"sda","type":"disk","size":"500G","fstype":null,"mountpoint":null,"uuid":null,"label":null,"rota":true,
             "children":[{"name":"sda1","kname":"sda1","type":"part","size":"500G","fstype":"xfs","mountpoint":"/","uuid":"abc","label":"root","rota":true}]}
        ]}"#;
        let devices = parse_lsblk(json).unwrap();
        assert_eq!(devices.len(), 1);
        assert!(devices[0].rotational);
        assert_eq!(devices[0].children[0].fstype.as_deref(), Some("xfs"));
        assert_eq!(
            devices[0].mounted_at("/").map(|d| d.name.as_str()),
            Some("sda1")
        );
    }

    #[test]
    fn findmnt_parses_options() {
        let json = r#"{"filesystems":[{"target":"/","source":"/dev/sda1","fstype":"xfs","options":"rw,relatime,seclabel"}]}"#;
        let mounts = parse_findmnt(json).unwrap();
        assert_eq!(mounts[0].fstype, "xfs");
        assert!(mounts[0].has_option("seclabel"));
        assert!(!mounts[0].has_option("ro"));
    }

    #[test]
    fn df_avail_takes_the_last_row() {
        let text = "Filesystem     1B-blocks       Used  Available Use% Mounted on\n/dev/sda1  500000000000 100000000 400000000000  20% /\n";
        assert_eq!(parse_df_avail(text).unwrap(), 400_000_000_000);
        assert!(parse_df_avail("").is_err());
    }
}

//! The Windows Update Agent binding for [`RealWin`](super::RealWin).
//!
//! The agent's COM objects are reached through late-bound `IDispatch` on
//! `Microsoft.Update.Session`, resolved from its ProgID, so this crate hard-codes no type
//! library. `os.update.schedule_reboot` does not call COM here: the privileged wrapper
//! owns the reboot, so this seam validates the instant and hands it on.

use windows::Win32::System::Com::{
    DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET, DISPATCH_PROPERTYPUT, DISPPARAMS,
    EXCEPINFO, IDispatch,
};
use windows::Win32::System::Variant::{VARIANT, VariantClear};
use windows::core::{GUID, PCWSTR, w};

use super::variant::{self, Value};

use super::{RealWin, api, prog_id, wide, with_com};
use crate::api::{Severity, UpdateInfo};
use crate::error::WindowsError;

impl RealWin {
    pub(super) fn search_updates(
        &self,
        include_hidden: bool,
    ) -> Result<Vec<UpdateInfo>, WindowsError> {
        with_com(|| {
            let session = update_session()?;
            let searcher = dispatch_object(&session, "CreateUpdateSearcher")?;
            let criteria = if include_hidden {
                "IsInstalled=0 or IsInstalled=1"
            } else {
                "IsInstalled=0"
            };
            let result = dispatch_call_object(&searcher, "Search", &[criteria.to_owned().into()])?;
            let updates = dispatch_object(&result, "Updates")?;
            let count = dispatch_get(&updates, "Count")?.as_i32();
            let mut out = Vec::new();
            for index in 0..count {
                let update = dispatch_call_object(&updates, "Item", &[index.into()])?;
                out.push(update_info(&update)?);
            }
            Ok(out)
        })
    }

    pub(super) fn download_update(&self, id: &str) -> Result<(), WindowsError> {
        with_com(|| {
            let session = update_session()?;
            let searcher = dispatch_object(&session, "CreateUpdateSearcher")?;
            let result =
                dispatch_call_object(&searcher, "Search", &[format!("UpdateID='{id}'").into()])?;
            let updates = dispatch_object(&result, "Updates")?;
            let downloader = dispatch_object(&session, "CreateUpdateDownloader")?;
            dispatch_put(&downloader, "Updates", Dispatch::Object(updates))?;
            dispatch_call(&downloader, "Download", &[])?;
            Ok(())
        })
    }

    pub(super) fn install_update(&self, id: &str) -> Result<(), WindowsError> {
        with_com(|| {
            let session = update_session()?;
            let searcher = dispatch_object(&session, "CreateUpdateSearcher")?;
            let result =
                dispatch_call_object(&searcher, "Search", &[format!("UpdateID='{id}'").into()])?;
            let updates = dispatch_object(&result, "Updates")?;
            let installer = dispatch_object(&session, "CreateUpdateInstaller")?;
            dispatch_put(&installer, "Updates", Dispatch::Object(updates))?;
            dispatch_call(&installer, "Install", &[])?;
            Ok(())
        })
    }

    pub(super) fn reboot_required(&self) -> Result<bool, WindowsError> {
        with_com(|| {
            let session = update_session()?;
            let system = dispatch_object(&session, "CreateUpdateSystemInfo")?;
            Ok(dispatch_get(&system, "RebootRequired")?.as_bool())
        })
    }

    pub(super) fn schedule_reboot(&self, at_unix_seconds: i64) -> Result<(), WindowsError> {
        if at_unix_seconds < 1 {
            return Err(WindowsError::RebootTimeInvalid {
                at: at_unix_seconds,
            });
        }
        Ok(())
    }

    pub(super) fn scheduled_reboot(&self) -> Result<Option<i64>, WindowsError> {
        Ok(None)
    }
}

/// A `Microsoft.Update.Session` COM object, as `IDispatch` for late binding.
fn update_session() -> Result<IDispatch, WindowsError> {
    let clsid = prog_id(w!("Microsoft.Update.Session"))?;
    // SAFETY: the CLSID came from the registry; CoCreateInstance returns a live interface.
    unsafe {
        api(windows::Win32::System::Com::CoCreateInstance(
            &clsid,
            None,
            windows::Win32::System::Com::CLSCTX_ALL,
        ))
    }
}

/// A late-bound value from the Windows Update Agent's `IDispatch` graph.
#[derive(Clone, Debug)]
enum Dispatch {
    /// A COM object.
    Object(IDispatch),
    /// A 32-bit integer.
    Int(i32),
    /// A boolean.
    Bool(bool),
    /// A string.
    Text(String),
}

impl From<IDispatch> for Dispatch {
    fn from(object: IDispatch) -> Self {
        Self::Object(object)
    }
}

impl From<i32> for Dispatch {
    fn from(value: i32) -> Self {
        Self::Int(value)
    }
}

impl From<String> for Dispatch {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl Dispatch {
    /// The integer value, or zero.
    fn as_i32(&self) -> i32 {
        match self {
            Self::Int(value) => *value,
            _ => 0,
        }
    }

    /// The boolean value, or false.
    fn as_bool(&self) -> bool {
        match self {
            Self::Bool(value) => *value,
            Self::Int(value) => *value != 0,
            _ => false,
        }
    }
}

/// Reads a property off a late-bound object.
fn dispatch_get(object: &IDispatch, name: &str) -> Result<Dispatch, WindowsError> {
    dispatch_invoke(object, name, DISPATCH_PROPERTYGET, &[])
}

/// Reads a property that must be a COM object.
fn dispatch_object(object: &IDispatch, name: &str) -> Result<IDispatch, WindowsError> {
    expect_object(dispatch_get(object, name)?, name)
}

/// Calls a method whose result must be a COM object.
fn dispatch_call_object(
    object: &IDispatch,
    name: &str,
    args: &[Dispatch],
) -> Result<IDispatch, WindowsError> {
    expect_object(dispatch_call(object, name, args)?, name)
}

/// The COM object in `value`, or a typed error naming what was asked for.
fn expect_object(value: Dispatch, name: &str) -> Result<IDispatch, WindowsError> {
    match value {
        Dispatch::Object(object) => Ok(object),
        _ => Err(WindowsError::Api {
            detail: format!("{name} did not return an object"),
        }),
    }
}

/// Writes a property on a late-bound object.
fn dispatch_put(object: &IDispatch, name: &str, value: Dispatch) -> Result<(), WindowsError> {
    let _ = dispatch_invoke(object, name, DISPATCH_PROPERTYPUT, &[value])?;
    Ok(())
}

/// Calls a method on a late-bound object.
fn dispatch_call(
    object: &IDispatch,
    name: &str,
    args: &[Dispatch],
) -> Result<Dispatch, WindowsError> {
    dispatch_invoke(object, name, DISPATCH_METHOD, args)
}

/// The late-binding core: resolve `name` to a DISPID, then invoke it.
fn dispatch_invoke(
    object: &IDispatch,
    name: &str,
    flags: DISPATCH_FLAGS,
    args: &[Dispatch],
) -> Result<Dispatch, WindowsError> {
    let name_w = wide(name);
    let mut dispid = 0i32;
    // SAFETY: the name buffer is valid for the call; `dispid` is an out-pointer.
    let resolved = unsafe {
        object.GetIDsOfNames(
            &GUID::zeroed(),
            &(PCWSTR(name_w.as_ptr())),
            1,
            0,
            &mut dispid,
        )
    };
    api(resolved)?;

    let mut variants: Vec<VARIANT> = args
        .iter()
        .map(|argument| match argument {
            Dispatch::Object(object) => variant::from_object(object.clone()),
            Dispatch::Int(value) => variant::from_i32(*value),
            Dispatch::Bool(value) => variant::from_bool(*value),
            Dispatch::Text(value) => variant::from_text(value),
        })
        .collect();
    variants.reverse();
    // A property put names its one argument `DISPID_PROPERTYPUT` (-3); a plain call has
    // no named arguments.
    let mut put_dispid = -3i32;
    let named = flags == DISPATCH_PROPERTYPUT;
    let params = DISPPARAMS {
        rgvarg: variants.as_mut_ptr(),
        rgdispidNamedArgs: if named {
            &raw mut put_dispid
        } else {
            std::ptr::null_mut()
        },
        cArgs: u32::try_from(variants.len()).unwrap_or(u32::MAX),
        cNamedArgs: u32::from(named),
    };
    let mut result = VARIANT::default();
    let mut exception = EXCEPINFO::default();
    let mut argument_error = 0u32;
    // SAFETY: `params`, `result` and `exception` are valid for the duration of the call.
    let invoked = unsafe {
        object.Invoke(
            dispid,
            &GUID::zeroed(),
            0,
            flags,
            &raw const params,
            Some(&raw mut result),
            Some(&raw mut exception),
            Some(&raw mut argument_error),
        )
    };
    // SAFETY: every variant in the argument array was created above and is owned here.
    unsafe {
        for variant in variants.iter_mut() {
            let _ = VariantClear(variant);
        }
    }
    let value = dispatch_value(&result);
    // SAFETY: `result` was filled by Invoke (or is still empty); the value above holds its
    // own copy of any string or object reference.
    unsafe {
        let _ = VariantClear(&mut result);
    }
    api(invoked)?;
    Ok(value)
}

/// Converts a COM result `VARIANT` into a [`Dispatch`].
fn dispatch_value(value: &VARIANT) -> Dispatch {
    match variant::read(value) {
        Value::Object(object) => Dispatch::Object(object),
        Value::Text(text) => Dispatch::Text(text),
        Value::Bool(flag) => Dispatch::Bool(flag),
        Value::Int(number) => Dispatch::Int(number),
        Value::Other => Dispatch::Int(0),
    }
}

/// Reads one offered update into an [`UpdateInfo`].
fn update_info(object: &IDispatch) -> Result<UpdateInfo, WindowsError> {
    let identity = dispatch_object(object, "Identity")?;
    let update_id = dispatch_get(&identity, "UpdateID")?;
    let title = dispatch_get(object, "Title")?;
    let severity = dispatch_get(object, "MsrcSeverity")?;
    Ok(UpdateInfo {
        id: dispatch_text(&update_id),
        kb: None,
        title: dispatch_text(&title),
        severity: Severity::parse(&dispatch_text(&severity).to_lowercase())
            .unwrap_or(Severity::Unspecified),
        reboot_required: dispatch_get(object, "RebootRequired")
            .map(|value| value.as_bool())
            .unwrap_or(false),
        downloaded: dispatch_get(object, "IsDownloaded")
            .map(|value| value.as_bool())
            .unwrap_or(false),
        installed: dispatch_get(object, "IsInstalled")
            .map(|value| value.as_bool())
            .unwrap_or(false),
    })
}

/// The text of a [`Dispatch`], or the empty string.
fn dispatch_text(value: &Dispatch) -> String {
    match value {
        Dispatch::Text(text) => text.clone(),
        _ => String::new(),
    }
}

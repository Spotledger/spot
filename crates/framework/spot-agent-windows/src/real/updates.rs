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
use windows::core::{BSTR, GUID, Interface, PCWSTR, w};

use super::{RealWin, api, prog_id, wide, with_com};
use crate::api::{Severity, UpdateInfo, WinApi};
use crate::error::WindowsError;

impl WinApi for RealWin {
    fn search_updates(&self, include_hidden: bool) -> Result<Vec<UpdateInfo>, WindowsError> {
        with_com(|| {
            let session = update_session()?;
            let searcher = dispatch_get(&session, "CreateUpdateSearcher")?;
            let criteria = if include_hidden {
                "IsInstalled=0 or IsInstalled=1"
            } else {
                "IsInstalled=0"
            };
            let result = dispatch_call(&searcher, "Search", &[criteria.to_owned().into()])?;
            let updates = dispatch_get(&result, "Updates")?;
            let count = dispatch_get(&updates, "Count")?.as_i32();
            let mut out = Vec::new();
            for index in 0..count {
                let update = dispatch_call(&updates, "Item", &[index.into()])?;
                out.push(update_info(&update)?);
            }
            Ok(out)
        })
    }

    fn download_update(&self, id: &str) -> Result<(), WindowsError> {
        with_com(|| {
            let session = update_session()?;
            let searcher = dispatch_get(&session, "CreateUpdateSearcher")?;
            let result = dispatch_call(&searcher, "Search", &[format!("UpdateID='{id}'")])?;
            let updates = dispatch_get(&result, "Updates")?;
            let downloader = dispatch_get(&session, "CreateUpdateDownloader")?;
            dispatch_put(&downloader, "Updates", &updates)?;
            dispatch_call(&downloader, "Download", &[])?;
            Ok(())
        })
    }

    fn install_update(&self, id: &str) -> Result<(), WindowsError> {
        with_com(|| {
            let session = update_session()?;
            let searcher = dispatch_get(&session, "CreateUpdateSearcher")?;
            let result = dispatch_call(&searcher, "Search", &[format!("UpdateID='{id}'")])?;
            let updates = dispatch_get(&result, "Updates")?;
            let installer = dispatch_get(&session, "CreateUpdateInstaller")?;
            dispatch_put(&installer, "Updates", &updates)?;
            dispatch_call(&installer, "Install", &[])?;
            Ok(())
        })
    }

    fn reboot_required(&self) -> Result<bool, WindowsError> {
        with_com(|| {
            let session = update_session()?;
            let system = dispatch_get(&session, "CreateUpdateSystemInfo")?;
            Ok(dispatch_get(&system, "RebootRequired")?.as_bool())
        })
    }

    fn schedule_reboot(&self, at_unix_seconds: i64) -> Result<(), WindowsError> {
        if at_unix_seconds < 1 {
            return Err(WindowsError::RebootTimeInvalid {
                at: at_unix_seconds,
            });
        }
        Ok(())
    }

    fn scheduled_reboot(&self) -> Result<Option<i64>, WindowsError> {
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

/// Writes a property on a late-bound object.
fn dispatch_put(object: &IDispatch, name: &str, value: &Dispatch) -> Result<(), WindowsError> {
    let _ = dispatch_invoke(object, name, DISPATCH_PROPERTYPUT, &[value.clone()])?;
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
            Dispatch::Object(object) => VARIANT::from(object.clone()),
            Dispatch::Int(value) => VARIANT::from(*value),
            Dispatch::Bool(value) => VARIANT::from(*value),
            Dispatch::Text(value) => VARIANT::from(BSTR::from(value.as_str())),
        })
        .collect();
    variants.reverse();
    let mut params = DISPPARAMS {
        rgvarg: variants.as_mut_ptr(),
        rgdispidNamedArgs: std::ptr::null_mut(),
        cArgs: variants.len() as u32,
        cNamedArgs: 0,
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
            &mut params,
            Some(&mut result),
            Some(&mut exception),
            Some(&mut argument_error),
        )
    };
    // SAFETY: every variant in the argument array was created above and is owned here.
    unsafe {
        for variant in variants.iter_mut() {
            let _ = VariantClear(variant);
        }
    }
    api(invoked)?;
    Ok(dispatch_value(&result))
}

/// Converts a COM result `VARIANT` into a [`Dispatch`].
fn dispatch_value(value: &VARIANT) -> Dispatch {
    if let Ok(object) = Interface::cast::<IDispatch>(value) {
        return Dispatch::Object(object);
    }
    if let Ok(text) = BSTR::try_from(value) {
        return Dispatch::Text(text.to_string());
    }
    Dispatch::Int(0)
}

/// Reads one offered update into an [`UpdateInfo`].
fn update_info(update: &Dispatch) -> Result<UpdateInfo, WindowsError> {
    let Dispatch::Object(object) = update else {
        return Err(WindowsError::Api {
            detail: "the update is not an object".to_owned(),
        });
    };
    let update_id = match dispatch_get(object, "Identity")? {
        Dispatch::Object(identity) => dispatch_get(&identity, "UpdateID")?,
        _ => {
            return Err(WindowsError::Api {
                detail: "the update identity is missing".to_owned(),
            });
        }
    };
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

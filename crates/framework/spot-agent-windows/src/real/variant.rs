//! Raw `VARIANT` construction and reading for the late-bound COM calls (the firewall
//! collection index and the Windows Update Agent's `IDispatch` graph).
//!
//! The `windows` crate exposes `VARIANT` as the bare C union with no conversions, so the
//! few shapes this crate needs (a 32-bit integer, a boolean, a string and an object) are
//! built here. Building is safe (writing a union field is); every variant built here is
//! released with `VariantClear`, which frees the `BSTR` or drops the `IDispatch` it owns.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Com::IDispatch;
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_BSTR, VT_DISPATCH, VT_I4,
    VT_UI4,
};
use windows::core::BSTR;

/// A value read out of a result `VARIANT`.
pub(super) enum Value {
    /// A 32-bit integer.
    Int(i32),
    /// A boolean.
    Bool(bool),
    /// A string.
    Text(String),
    /// A COM object.
    Object(IDispatch),
    /// Empty, null or a type this crate does not read.
    Other,
}

fn build(vt: VARENUM, payload: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: payload,
            }),
        },
    }
}

/// A `VT_I4` variant.
pub(super) fn from_i32(value: i32) -> VARIANT {
    build(VT_I4, VARIANT_0_0_0 { lVal: value })
}

/// A `VT_BOOL` variant.
pub(super) fn from_bool(value: bool) -> VARIANT {
    build(
        VT_BOOL,
        VARIANT_0_0_0 {
            boolVal: VARIANT_BOOL::from(value),
        },
    )
}

/// A `VT_BSTR` variant that owns a copy of `text`.
pub(super) fn from_text(text: &str) -> VARIANT {
    build(
        VT_BSTR,
        VARIANT_0_0_0 {
            bstrVal: ManuallyDrop::new(BSTR::from(text)),
        },
    )
}

/// A `VT_DISPATCH` variant that owns one reference to `object`.
pub(super) fn from_object(object: IDispatch) -> VARIANT {
    build(
        VT_DISPATCH,
        VARIANT_0_0_0 {
            pdispVal: ManuallyDrop::new(Some(object)),
        },
    )
}

/// Reads a result variant. Strings and objects are copied or referenced again, so the
/// caller still clears the variant it passed in.
pub(super) fn read(variant: &VARIANT) -> Value {
    // SAFETY: the `vt` tag names the union member that the callee wrote, and each arm
    // reads only that member; the borrowed object or string is cloned, not moved.
    unsafe {
        let inner = &*variant.Anonymous.Anonymous;
        match inner.vt {
            VT_I4 => Value::Int(inner.Anonymous.lVal),
            VT_UI4 => Value::Int(i32::try_from(inner.Anonymous.ulVal).unwrap_or(i32::MAX)),
            VT_BOOL => Value::Bool(inner.Anonymous.boolVal.as_bool()),
            VT_BSTR => Value::Text(String::from_utf16_lossy(&inner.Anonymous.bstrVal)),
            VT_DISPATCH => match &*inner.Anonymous.pdispVal {
                Some(object) => Value::Object(object.clone()),
                None => Value::Other,
            },
            _ => Value::Other,
        }
    }
}

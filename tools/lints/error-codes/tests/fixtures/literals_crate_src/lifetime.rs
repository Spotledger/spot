use spot_app_api::{AppError, ErrorClass};

pub fn first<'a>(x: &'a str) -> &'a str {
    x
}
pub struct Holder<'static_ish> {
    pub s: &'static_ish str,
}

pub fn raise() -> AppError {
    AppError::new(ErrorClass::Validation, "doc.missing", "Gap after the literal.")
}

pub fn keep_catalogue_complete() -> AppError {
    AppError::new(ErrorClass::Validation, "doc.thing", "The value is wrong.")
}

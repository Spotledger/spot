use spot_app_api::{AppError, ErrorClass};

pub fn first() -> String {
    format!("a\r\nb")
}

pub fn raise() -> AppError {
    AppError::new(ErrorClass::Validation, "doc.missing", "Gap after the literal.")
}

pub fn keep_catalogue_complete() -> AppError {
    AppError::new(ErrorClass::Validation, "doc.thing", "The value is wrong.")
}

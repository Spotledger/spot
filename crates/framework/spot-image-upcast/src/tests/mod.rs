//! Tests of the image upcaster.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod fixture_tests;
mod image_tests;
mod patch_tests;
mod property_tests;
mod registry_tests;
mod steps_tests;

use serde_json::Value;

use crate::{Body, Image};

/// An image of `entity` written under `schema`, with `body` as its body.
pub(crate) fn img(entity: &str, schema: u64, body: Value) -> Image {
    Image::from_parts(entity, schema, body).expect("a JSON object body")
}

/// An empty body.
pub(crate) fn empty() -> Body {
    Body::new()
}

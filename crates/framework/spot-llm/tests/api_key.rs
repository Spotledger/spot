//! The API key: redacted by `Debug`/`Display`, buildable from every source the brief names.

use spot_llm::ApiKey;
use spot_secrets::SecretString;

#[test]
fn never_prints_itself() {
    let key = ApiKey::new("sk-quiet-horizon-42");
    assert_eq!(format!("{key:?}"), "***");
    assert_eq!(format!("{key}"), "***");
    assert_eq!(format!("{key:?}"), format!("{:?}", key.clone()));
}

#[test]
fn builds_from_a_string_and_a_secret() {
    let from_string: ApiKey = "sk-string".to_owned().into();
    let from_str: ApiKey = "sk-str".into();
    let secret = SecretString::new("sk-secret");
    let from_secret: ApiKey = (&secret).into();
    let built = ApiKey::from_secret(&secret);

    assert_eq!(from_string.expose(), "sk-string");
    assert_eq!(from_str.expose(), "sk-str");
    assert_eq!(from_secret.expose(), "sk-secret");
    assert_eq!(built.expose(), "sk-secret");
    // The secret stays redacted both ways.
    assert_eq!(format!("{secret:?}"), "[redacted]");
    assert_eq!(format!("{from_secret:?}"), "***");
}

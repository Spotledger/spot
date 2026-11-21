//! find_by_secret and the blind index.

use crate::support::*;

async fn find_by_secret_finds_misses_and_never_matches_across_fields<
    B: StorageBackend + ChangeFeed,
>(
    r: &Rig<B>,
) {
    let access = r.access();
    let (a, va) = r.account("a").await;
    let (b, vb) = r.account("b").await;
    r.engine
        .set_secret(&access, "account", a, "token", s(TOKEN), va)
        .await
        .unwrap();
    r.engine
        .set_secret(&access, "account", b, "api_key", s(TOKEN), vb)
        .await
        .unwrap();
    let find = |field: &'static str, value: &'static str| {
        r.engine
            .find_by_secret(&access, "account", field, value.as_bytes())
    };
    assert_eq!(find("token", TOKEN).await.unwrap(), Some(a), "found");
    assert_eq!(
        find("api_key", TOKEN).await.unwrap(),
        Some(b),
        "the same text in another field"
    );
    assert_eq!(
        find("token", "tok_live_other").await.unwrap(),
        None,
        "a miss"
    );
    assert_eq!(
        find("token", "TOK_LIVE_8F3A9C1E5D7B2046").await.unwrap(),
        None,
        "exact, not folded"
    );
    assert_eq!(
        find("token", "tok_live_8f3a9c1e5d7b204").await.unwrap(),
        None,
        "no prefix match"
    );
    // The wrong tenant never matches.
    let other = r.access_at("evil.test");
    assert_eq!(
        r.engine
            .find_by_secret(&other, "account", "token", TOKEN.as_bytes())
            .await
            .unwrap(),
        None
    );
    // A field that is not a lookup field is refused, not answered with "not found".
    assert_eq!(
        find("password", PASSWORD).await.unwrap_err().code,
        "secretfield.not_lookup"
    );
    // A name that is not a secret field, and a DocType that does not exist, fail closed.
    assert_eq!(
        find("label", "ada").await.unwrap_err().code,
        "secretfield.unknown_field"
    );
    assert_eq!(
        find("nope", "x").await.unwrap_err().code,
        "secretfield.unknown_field"
    );
    let err = r
        .engine
        .find_by_secret(&access, "ghost", "token", b"x")
        .await
        .unwrap_err();
    assert_eq!(err.code, "meta.unknown_doctype");
    // A child DocType holds no secrets.
    let err = r
        .engine
        .find_by_secret(&access, "crate_line", "what", b"x")
        .await
        .unwrap_err();
    assert_eq!(err.code, "secretfield.unsupported_doctype");
    // An empty value is refused, not looked up.
    assert_eq!(
        r.engine
            .find_by_secret(&access, "account", "token", b"")
            .await
            .unwrap_err()
            .code,
        "secretfield.empty_value"
    );
    // A record whose secret is unset never matches anything: add one with no token.
    r.account("nobody").await;
    assert_eq!(find("token", TOKEN).await.unwrap(), Some(a));
}

async fn a_lookup_field_that_is_not_unique_refuses_an_ambiguous_match<
    B: StorageBackend + ChangeFeed,
>(
    r: &Rig<B>,
) {
    let access = r.access();
    let (a, va) = r.account("a").await;
    let (b, vb) = r.account("b").await;
    r.engine
        .set_secret(&access, "account", a, "api_key", s(TOKEN), va)
        .await
        .unwrap();
    r.engine
        .set_secret(&access, "account", b, "api_key", s(TOKEN), vb)
        .await
        .unwrap();
    let err = r
        .engine
        .find_by_secret(&access, "account", "api_key", TOKEN.as_bytes())
        .await
        .unwrap_err();
    assert_eq!(err.code, "secretfield.ambiguous");
}

async fn a_unique_lookup_secret_is_unique_and_an_unset_one_does_not_skip_the_check<
    B: StorageBackend + ChangeFeed,
>(
    r: &Rig<B>,
) {
    let access = r.access();
    let (a, va) = r.account("a").await;
    let (b, vb) = r.account("b").await;
    let (c, _vc) = r.account("c").await;
    r.engine
        .set_secret(&access, "account", a, "token", s(TOKEN), va)
        .await
        .unwrap();
    // A second record with the same token is refused, with no detail of what it collided with.
    let err = r
        .engine
        .set_secret(&access, "account", b, "token", s(TOKEN), vb)
        .await
        .unwrap_err();
    assert_eq!(err.code, "secretfield.duplicate");
    assert_eq!(err.class, ErrorClass::Duplicate);
    let shown = format!("{err:?}");
    assert_absent(
        shown.as_bytes(),
        &forms_of_plain(TOKEN),
        "the duplicate error",
    );
    assert!(r.sealed(b, "token").await.is_none() && r.bidx(b, "token").await.is_none());
    // Records that never had a token are all fine (unset is not a value) ...
    assert!(r.sealed(c, "token").await.is_none());
    // ... but an empty text is not "no value": it is refused outright, so a unique lookup field
    // can never hold a value that skips the uniqueness check.
    let err = r
        .engine
        .set_secret(&access, "account", b, "token", s(""), vb)
        .await
        .unwrap_err();
    assert_eq!(err.code, "secretfield.empty_value");
    assert_eq!(
        r.engine
            .find_by_secret(&access, "account", "token", TOKEN.as_bytes())
            .await
            .unwrap(),
        Some(a)
    );
}

cases!(
    find_by_secret_finds_misses_and_never_matches_across_fields,
    a_lookup_field_that_is_not_unique_refuses_an_ambiguous_match,
    a_unique_lookup_secret_is_unique_and_an_unset_one_does_not_skip_the_check,
);

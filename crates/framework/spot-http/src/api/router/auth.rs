//! Login, logout and session handlers for `/api/v1/auth/*`: the handlers marshal between
//! the contract shapes and the `AuthService`; the token rules themselves live in
//! WP-PLT-02.

use crate::api::registry::Code;
use crate::api::service::{ApiError, LoginBody, RequestCtx, Session, SessionInfo, UserInfo};
use crate::request::RequestId;
use crate::site::SiteContext;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde_json::Value as Json;

use super::middleware::header_credential;
use super::{
    Failure, HandlerResult, SharedState, build_ctx, envelope, fail, json_response, language_of,
    take_body, validated_key,
};
pub(super) async fn auth_login(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> HandlerResult {
    let body = take_body(request, true, &site.request_id).await?;
    let ctx = RequestCtx {
        site: site.site.clone(),
        credential: header_credential(&headers),
        idempotency_key: None,
        language: language_of(&headers),
        request_id: site.request_id.clone(),
        peer: site.peer,
    };
    let json = body.unwrap_or(Json::Null);
    let login = decode_login(&json, &site.request_id)?;
    let session = state
        .auth
        .login(&ctx, &login)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    Ok(json_response(
        StatusCode::OK,
        envelope(session_json(&session), &[]),
    ))
}

fn decode_login(body: &Json, request_id: &RequestId) -> Result<LoginBody, Failure> {
    let Json::Object(map) = body else {
        return Err(fail(
            ApiError::new(Code::ApiInvalidRequest, "The login body must be an object."),
            request_id,
        ));
    };
    for key in map.keys() {
        if !["user", "password", "totp", "recovery_code"].contains(&key.as_str()) {
            return Err(fail(
                ApiError::new(
                    Code::ApiInvalidRequest,
                    format!("`{key}` is not a login field."),
                ),
                request_id,
            ));
        }
    }
    let take_str =
        |k: &str| -> Option<String> { map.get(k).and_then(Json::as_str).map(str::to_owned) };
    let Some(user) = take_str("user") else {
        return Err(fail(
            ApiError::new(Code::ApiInvalidRequest, "`user` is required."),
            request_id,
        ));
    };
    let Some(password) = take_str("password") else {
        return Err(fail(
            ApiError::new(Code::ApiInvalidRequest, "`password` is required."),
            request_id,
        ));
    };
    if user.is_empty()
        || user.chars().count() > 320
        || password.is_empty()
        || password.chars().count() > 1024
    {
        return Err(fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                "The login fields are out of range.",
            ),
            request_id,
        ));
    }
    let totp = take_str("totp");
    if let Some(code) = &totp {
        let len_ok = code.len() == 6 || code.len() == 8;
        if !len_ok || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(fail(
                ApiError::new(Code::ApiInvalidRequest, "`totp` must be 6 to 8 digits."),
                request_id,
            ));
        }
    }
    let recovery_code = take_str("recovery_code");
    if recovery_code.as_deref().is_some_and(|c| c.len() > 32) {
        return Err(fail(
            ApiError::new(Code::ApiInvalidRequest, "`recovery_code` is too long."),
            request_id,
        ));
    }
    Ok(LoginBody {
        user,
        password,
        totp,
        recovery_code,
    })
}

fn user_json(user: &UserInfo) -> Json {
    let mut map = serde_json::Map::new();
    map.insert("id".to_owned(), Json::String(user.id.to_string()));
    map.insert("email".to_owned(), Json::String(user.email.clone()));
    map.insert("full_name".to_owned(), Json::String(user.full_name.clone()));
    Json::Object(map)
}

fn session_json(session: &Session) -> Json {
    let mut map = serde_json::Map::new();
    map.insert("token".to_owned(), Json::String(session.token.clone()));
    map.insert(
        "expires_at".to_owned(),
        Json::String(session.expires_at.clone()),
    );
    map.insert("user".to_owned(), user_json(&session.user));
    Json::Object(map)
}

fn session_info_json(info: &SessionInfo) -> Json {
    let mut map = serde_json::Map::new();
    map.insert(
        "expires_at".to_owned(),
        Json::String(info.expires_at.clone()),
    );
    map.insert("user".to_owned(), user_json(&info.user));
    map.insert("site".to_owned(), Json::String(info.site.clone()));
    Json::Object(map)
}

pub(super) async fn auth_logout(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> HandlerResult {
    let ctx = build_ctx(&site, &headers, validated_key(&request));
    state
        .auth
        .logout(&ctx)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(super) async fn auth_session(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
) -> HandlerResult {
    let ctx = build_ctx(&site, &headers, None);
    let info = state
        .auth
        .session(&ctx)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    Ok(json_response(
        StatusCode::OK,
        envelope(session_info_json(&info), &[]),
    ))
}

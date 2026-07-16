//! RPC payload types with JSON/CBOR dual format support.
//!
//! This module provides lightweight RPC request/response types and
//! format negotiation based on HTTP headers:
//! - `Content-Type: application/cbor` for CBOR request bodies
//! - `Content-Type: application/json` (default) for JSON request bodies
//! - `Accept: application/cbor` for CBOR responses
//! - `Accept: application/json` (default) for JSON responses
//!
//! Content negotiation applies to success bodies only: error response
//! bodies are always JSON regardless of the `Accept` header.

use axum::{
    Json,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use core::fmt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

/// Content format for request/response payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    Json,
    Cbor,
    Markdown(bool), // 是否明确为 markdown，默认为 false
}

/// Request/response format pair derived from HTTP content negotiation headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadFormat {
    request: ContentType,
    response: ContentType,
}

impl PayloadFormat {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            request: ContentType::from_header(headers),
            response: ContentType::from_accept(headers),
        }
    }

    pub fn request_type(&self) -> ContentType {
        self.request
    }

    pub fn response_type(&self) -> ContentType {
        self.response
    }

    pub fn parse_body<T>(&self, body: &[u8]) -> Result<StringOr<T>, RpcError>
    where
        T: DeserializeOwned,
    {
        self.request.parse_body(body)
    }

    pub fn response<T: Serialize>(&self, data: T) -> AppResponse<T> {
        self.response.response(data)
    }
}

/// A helper type that can represent either a raw string or a parsed value.
#[derive(Debug)]
pub enum StringOr<T> {
    String(String),
    Value(T),
}

impl<T> StringOr<T> {
    /// Get the parsed value, or return an error if it's a raw string.
    pub fn value(self) -> Result<T, String> {
        match self {
            StringOr::String(s) => Err(s),
            StringOr::Value(v) => Ok(v),
        }
    }
}

impl<T> fmt::Display for StringOr<T>
where
    T: Serialize + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StringOr::String(s) => write!(f, "{}", s),
            StringOr::Value(v) => match serde_json::to_string_pretty(v) {
                Ok(s) => write!(f, "{}", s),
                Err(_) => write!(f, "{:?}", v),
            },
        }
    }
}

impl ContentType {
    /// Detect request content type from Content-Type header.
    ///
    /// Note: this is deliberate substring keyword matching, not full RFC
    /// 7231 negotiation. Precedence is fixed — `cbor` > `json` > `markdown`
    /// — regardless of the order media types are listed in or their
    /// q-values. Kept for wire compatibility; clients that want markdown
    /// must not also list `application/json` in the same header.
    pub fn from_header(headers: &HeaderMap) -> Self {
        match headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
        {
            Some(ct) => {
                if ct.contains("application/cbor") {
                    ContentType::Cbor
                } else if ct.contains("application/json") {
                    ContentType::Json
                } else if ct.contains("text/markdown") {
                    ContentType::Markdown(true)
                } else {
                    ContentType::Markdown(false)
                }
            }
            None => ContentType::Json,
        }
    }

    /// Detect preferred response format from Accept header.
    ///
    /// Same fixed keyword precedence as [`ContentType::from_header`]:
    /// `cbor` > `json` > `markdown`, ignoring listing order and q-values.
    pub fn from_accept(headers: &HeaderMap) -> Self {
        headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .map(|accept| {
                if accept.contains("application/cbor") {
                    ContentType::Cbor
                } else if accept.contains("application/json") {
                    ContentType::Json
                } else if accept.contains("text/markdown") {
                    ContentType::Markdown(true)
                } else {
                    ContentType::Json
                }
            })
            .unwrap_or(ContentType::Json)
    }

    /// Get the corresponding HTTP Content-Type header value.
    pub fn header_value(&self) -> HeaderValue {
        match self {
            ContentType::Json => HeaderValue::from_static("application/json"),
            ContentType::Cbor => HeaderValue::from_static("application/cbor"),
            ContentType::Markdown(_) => HeaderValue::from_static("text/markdown; charset=utf-8"),
        }
    }

    /// Parse the request body according to the content type.
    pub fn parse_body<T>(&self, body: &[u8]) -> Result<StringOr<T>, RpcError>
    where
        T: DeserializeOwned,
    {
        match self {
            ContentType::Json => serde_json::from_slice(body)
                .map(StringOr::Value)
                .map_err(|e| RpcError::new(format!("parse JSON error: {e}"))),
            ContentType::Cbor => cbor2::from_slice(body)
                .map(StringOr::Value)
                .map_err(|e| RpcError::new(format!("parse CBOR error: {e}"))),
            ContentType::Markdown(_) => {
                serde_json::from_slice(body)
                    .map(StringOr::Value)
                    .or_else(|_| {
                        let text = std::str::from_utf8(body)
                            .map_err(|e| RpcError::new(format!("parse Markdown error: {e}")))?;
                        Ok(StringOr::String(text.to_string()))
                    })
            }
        }
    }

    /// Create a response with the given data and this content type.
    pub fn response<T: Serialize>(&self, data: T) -> AppResponse<T> {
        AppResponse::new(data, *self)
    }
}

/// Extracts the preferred response format from the `Accept` header.
///
/// Defaults to JSON if no supported `Accept` value is present.
pub struct Accept(pub PayloadFormat, pub bool);

fn prefers_chinese(accept_language: &str) -> bool {
    let lang = accept_language.to_lowercase();
    let zh_pos = lang.find("zh");
    let en_pos = lang.find("en");

    match (zh_pos, en_pos) {
        (Some(zh), Some(en)) => zh < en,
        (Some(_), None) => true,
        _ => false,
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Accept {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let is_cn = parts
            .headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok())
            .map(prefers_chinese)
            .unwrap_or(false);
        Ok(Accept(PayloadFormat::from_headers(&parts.headers), is_cn))
    }
}

// ─── RPC Types ────────────────────────────────────────────────────────────────

/// RPC request object.
#[allow(unused)]
#[derive(Debug, Deserialize)]
pub struct RpcRequest<T> {
    pub method: String,
    pub params: Option<T>,
}

/// RPC response object.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RpcResponse<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<T>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl<T> RpcResponse<T> {
    /// Create a successful RPC response.
    pub fn success(result: T) -> Self {
        Self {
            result: Some(result),
            error: None,
            next_cursor: None,
        }
    }

    /// Create an error RPC response.
    #[allow(unused)]
    pub fn error(error: RpcError) -> Self {
        Self {
            result: None,
            error: Some(error),
            next_cursor: None,
        }
    }
}

/// RPC error object.
#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// Create a new RPC error with the given code and message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            data: None,
        }
    }

    pub fn into_response(self, code: Option<StatusCode>) -> Response {
        (
            code.unwrap_or(StatusCode::OK),
            Json(RpcResponse::<()>::error(self)),
        )
            .into_response()
    }
}

/// Extract the bearer token from the `Authorization` header.
///
/// Strips exactly one leading `Bearer ` prefix: `trim_start_matches` would
/// strip a repeated prefix out of the credential itself. Returns an empty
/// string when the header is absent. Shared by the HTTP handlers and the
/// MCP channel so both extract credentials identically.
pub fn extract_bearer_token(headers: &HeaderMap) -> String {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .map(|s| {
            let s = s.trim();
            s.strip_prefix("Bearer ").unwrap_or(s)
        })
        .unwrap_or("")
        .to_string()
}

/// Extract the shard id from the `Shard-Id`/`X-Shard` header.
///
/// Lenient: an absent or unparseable header falls back to shard 0. Shared
/// by the HTTP handlers and the MCP channel.
pub fn extract_shard_id(headers: &HeaderMap) -> u32 {
    headers
        .get("Shard-Id")
        .or_else(|| headers.get("X-Shard"))
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// Extracts a bearer token from the `Authorization` header and sharding id from the `X-Shard` header.
#[derive(Debug)]
pub struct HeaderVals(pub String, pub u32);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for HeaderVals {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(HeaderVals(
            extract_bearer_token(&parts.headers),
            extract_shard_id(&parts.headers),
        ))
    }
}

// Drop-in replacements for the raw axum `Query`/`Path`/`Bytes` extractors.
// The raw extractors reject with plain-text bodies (e.g. `?limit=abc`, a
// non-numeric path segment, or a body over the size limit), which would
// break the "error bodies are always JSON" contract before the handler even
// runs. Handlers must use these wrappers, which re-emit the rejection
// through the `AppError` JSON envelope with the rejection's own status.

/// `axum::extract::Query` with JSON-enveloped rejections.
pub struct AppQuery<T>(pub T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> axum::extract::FromRequestParts<S>
    for AppQuery<T>
{
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(query) => Ok(AppQuery(query.0)),
            Err(rejection) => Err(AppError::with_status(
                rejection.status(),
                rejection.body_text(),
            )),
        }
    }
}

/// `axum::extract::Path` with JSON-enveloped rejections.
pub struct AppPath<T>(pub T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned + Send> axum::extract::FromRequestParts<S>
    for AppPath<T>
{
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(path) => Ok(AppPath(path.0)),
            Err(rejection) => Err(AppError::with_status(
                rejection.status(),
                rejection.body_text(),
            )),
        }
    }
}

/// `axum::body::Bytes` with JSON-enveloped rejections (covers the body size
/// limit's 413).
pub struct AppBytes(pub axum::body::Bytes);

impl<S: Send + Sync> axum::extract::FromRequest<S> for AppBytes {
    type Rejection = AppError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::body::Bytes::from_request(req, state).await {
            Ok(bytes) => Ok(AppBytes(bytes)),
            Err(rejection) => Err(AppError::with_status(
                rejection.status(),
                rejection.body_text(),
            )),
        }
    }
}

// ─── App Error ────────────────────────────────────────────────────────────────

/// A typed error that converts to an HTTP response via `IntoResponse`.
#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub message: String,
    pub data: Option<Value>,
}

impl AppError {
    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "authentication failed".into(),
            data: None,
        }
    }

    pub fn bad_request(e: impl std::fmt::Debug) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: format!("{e:?}"),
            data: None,
        }
    }

    pub fn with_status(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            data: None,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Error bodies are always JSON, regardless of the request's Accept
        // header; only success bodies participate in content negotiation.
        let mut err = RpcError::new(self.message);
        err.data = self.data;
        err.into_response(Some(self.status))
    }
}

// ─── Response Encoding ────────────────────────────────────────────────────────

/// A response type that supports both JSON and CBOR serialization.
///
/// The format is determined by the `content_type` field, which should
/// be set from the `Accept` header via the [`Accept`] extractor.
pub struct AppResponse<T: Serialize> {
    pub data: T,
    pub content_type: ContentType,
}

impl<T: Serialize> AppResponse<T> {
    pub fn new(data: T, ct: ContentType) -> Self {
        Self {
            data,
            content_type: ct,
        }
    }
}

impl<T: Serialize> IntoResponse for AppResponse<T> {
    fn into_response(self) -> Response {
        match self.content_type {
            ContentType::Json => match serde_json::to_vec(&self.data) {
                Ok(bytes) => (
                    [(header::CONTENT_TYPE, self.content_type.header_value())],
                    bytes,
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("JSON serialization error: {e}"),
                )
                    .into_response(),
            },
            ContentType::Cbor => {
                let mut buf = Vec::new();
                match cbor2::to_writer(&self.data, &mut buf) {
                    Ok(()) => (
                        [(header::CONTENT_TYPE, self.content_type.header_value())],
                        buf,
                    )
                        .into_response(),
                    Err(e) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("CBOR serialization error: {e}"),
                    )
                        .into_response(),
                }
            }
            ContentType::Markdown(_) => match serde_json::to_value(&self.data) {
                Ok(val) => {
                    let text = match val {
                        Value::String(s) => s,
                        other => format!("{:#}", other),
                    };
                    (
                        [(header::CONTENT_TYPE, self.content_type.header_value())],
                        text,
                    )
                        .into_response()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Markdown serialization error: {e}"),
                )
                    .into_response(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Accept, AppError, AppQuery, ContentType, HeaderVals, PayloadFormat, StringOr,
        prefers_chinese,
    };
    use axum::{
        body::to_bytes,
        extract::FromRequestParts,
        http::{HeaderMap, Request, StatusCode, header},
        response::IntoResponse,
    };
    use serde::{Deserialize, Serialize, Serializer, ser::Error as _};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct DemoPayload {
        name: String,
        count: u32,
    }

    #[derive(Debug)]
    struct BadSerialize;

    impl Serialize for BadSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(S::Error::custom("bad serialize"))
        }
    }

    fn demo_payload() -> DemoPayload {
        DemoPayload {
            name: "alice".to_string(),
            count: 7,
        }
    }

    #[test]
    fn content_type_from_header_prefers_content_type() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "application/cbor".parse().unwrap());
        headers.insert(header::ACCEPT, "application/json".parse().unwrap());

        assert_eq!(ContentType::from_header(&headers), ContentType::Cbor);
    }

    #[test]
    fn payload_format_splits_request_and_response_types() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        headers.insert(header::ACCEPT, "application/cbor".parse().unwrap());

        let format = PayloadFormat::from_headers(&headers);
        assert_eq!(format.request_type(), ContentType::Json);
        assert_eq!(format.response_type(), ContentType::Cbor);
    }

    #[test]
    fn content_type_from_accept_and_default() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "application/json".parse().unwrap());
        assert_eq!(ContentType::from_accept(&headers), ContentType::Json);

        let headers = HeaderMap::new();
        assert_eq!(ContentType::from_header(&headers), ContentType::Json);
        assert_eq!(ContentType::from_accept(&headers), ContentType::Json);

        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
        headers.insert(header::ACCEPT, "application/xml".parse().unwrap());
        assert_eq!(
            ContentType::from_header(&headers),
            ContentType::Markdown(false)
        );
        assert_eq!(ContentType::from_accept(&headers), ContentType::Json);
    }

    #[test]
    fn parse_body_json_and_cbor_success() {
        let expected = demo_payload();
        let json_body = serde_json::to_vec(&expected).unwrap();

        let parsed_json = ContentType::Json
            .parse_body::<DemoPayload>(&json_body)
            .unwrap();
        assert_eq!(parsed_json.value().unwrap(), expected);

        let mut cbor_body = Vec::new();
        cbor2::to_writer(&demo_payload(), &mut cbor_body).unwrap();
        let parsed_cbor = ContentType::Cbor
            .parse_body::<DemoPayload>(&cbor_body)
            .unwrap();
        assert_eq!(parsed_cbor.value().unwrap(), demo_payload());
    }

    #[test]
    fn parse_body_markdown_handles_json_and_plain_text() {
        let expected = demo_payload();
        let json_body = serde_json::to_vec(&expected).unwrap();

        let parsed_from_json = ContentType::Markdown(true)
            .parse_body::<DemoPayload>(&json_body)
            .unwrap();
        assert_eq!(parsed_from_json.value().unwrap(), expected);

        let plain_text = b"# hello markdown";
        let parsed_text = ContentType::Markdown(false)
            .parse_body::<DemoPayload>(plain_text)
            .unwrap();
        match parsed_text {
            StringOr::String(s) => assert_eq!(s, "# hello markdown"),
            StringOr::Value(_) => panic!("expected raw markdown string"),
        }
    }

    #[test]
    fn parse_body_markdown_rejects_invalid_utf8() {
        let invalid = [0xff, 0xfe, 0xfd];
        let err = ContentType::Markdown(false)
            .parse_body::<DemoPayload>(&invalid)
            .unwrap_err();
        assert!(err.message.contains("parse Markdown error"));
    }

    #[test]
    fn parse_body_json_and_cbor_report_decode_errors() {
        let json_err = ContentType::Json
            .parse_body::<DemoPayload>(b"{bad json")
            .unwrap_err();
        assert!(json_err.message.contains("parse JSON error"));

        let cbor_err = ContentType::Cbor
            .parse_body::<DemoPayload>(b"not cbor")
            .unwrap_err();
        assert!(cbor_err.message.contains("parse CBOR error"));
    }

    #[tokio::test]
    async fn app_response_json_and_cbor_have_expected_headers_and_body() {
        let payload = demo_payload();

        let json_res = ContentType::Json.response(payload.clone()).into_response();
        assert_eq!(json_res.status(), StatusCode::OK);
        assert_eq!(
            json_res.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let json_bytes = to_bytes(json_res.into_body(), usize::MAX).await.unwrap();
        let json_parsed: DemoPayload = serde_json::from_slice(&json_bytes).unwrap();
        assert_eq!(json_parsed, payload);

        let cbor_res = ContentType::Cbor.response(payload.clone()).into_response();
        assert_eq!(cbor_res.status(), StatusCode::OK);
        assert_eq!(
            cbor_res.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/cbor"
        );
        let cbor_bytes = to_bytes(cbor_res.into_body(), usize::MAX).await.unwrap();
        let cbor_parsed: DemoPayload = cbor2::from_slice(cbor_bytes.as_ref()).unwrap();
        assert_eq!(cbor_parsed, payload);
    }

    #[tokio::test]
    async fn app_response_markdown_string_and_object() {
        let md_text_res = ContentType::Markdown(true)
            .response("# title".to_string())
            .into_response();
        assert_eq!(md_text_res.status(), StatusCode::OK);
        assert_eq!(
            md_text_res.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/markdown; charset=utf-8"
        );
        let text_bytes = to_bytes(md_text_res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(std::str::from_utf8(&text_bytes).unwrap(), "# title");

        let md_obj_res = ContentType::Markdown(false)
            .response(demo_payload())
            .into_response();
        let obj_bytes = to_bytes(md_obj_res.into_body(), usize::MAX).await.unwrap();
        let obj_text = std::str::from_utf8(&obj_bytes).unwrap();
        assert!(obj_text.contains("\"name\": \"alice\""));
        assert!(obj_text.contains("\"count\": 7"));
    }

    #[tokio::test]
    async fn app_response_reports_serialization_errors_for_all_formats() {
        let json = ContentType::Json.response(BadSerialize).into_response();
        assert_eq!(json.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(json.into_body(), usize::MAX).await.unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("JSON serialization error")
        );

        let cbor = ContentType::Cbor.response(BadSerialize).into_response();
        assert_eq!(cbor.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(cbor.into_body(), usize::MAX).await.unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("CBOR serialization error")
        );

        let markdown = ContentType::Markdown(true)
            .response(BadSerialize)
            .into_response();
        assert_eq!(markdown.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(markdown.into_body(), usize::MAX).await.unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("Markdown serialization error")
        );

        assert_eq!(StringOr::Value(BadSerialize).to_string(), "BadSerialize");
    }

    #[tokio::test]
    async fn accept_and_header_vals_extractors_work() {
        let req = Request::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/cbor")
            .header(header::ACCEPT_LANGUAGE, "zh-CN,en;q=0.8")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();

        let accept = Accept::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(accept.0.request_type(), ContentType::Json);
        assert_eq!(accept.0.response_type(), ContentType::Cbor);
        assert!(accept.1);

        let HeaderVals(bearer, sharding) = HeaderVals::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert_eq!(bearer, "secret-token");
        assert_eq!(sharding, 0);

        let req = Request::builder()
            .header(header::CONTENT_TYPE, "application/cbor")
            .header(header::ACCEPT, "text/markdown")
            .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header(header::AUTHORIZATION, "another-token")
            .header("shard-id", "42")
            .body(())
            .unwrap();

        let (mut parts, _) = req.into_parts();

        let accept = Accept::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(accept.0.request_type(), ContentType::Cbor);
        assert_eq!(accept.0.response_type(), ContentType::Markdown(true));
        assert!(!accept.1);

        let HeaderVals(bearer, sharding) = HeaderVals::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert_eq!(bearer, "another-token");
        assert_eq!(sharding, 42);
    }

    #[tokio::test]
    async fn app_error_into_response_contains_message() {
        let res = AppError::unauthorized().into_response();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v.pointer("/error/message").and_then(|v| v.as_str()),
            Some("authentication failed")
        );
    }

    #[test]
    fn prefers_chinese_when_zh_before_en() {
        assert!(prefers_chinese("zh-CN,zh;q=0.9,en;q=0.8"));
        assert!(prefers_chinese("zh,en"));
    }

    #[test]
    fn prefers_english_when_en_before_zh() {
        assert!(!prefers_chinese("en-US,en;q=0.9,zh;q=0.8"));
        assert!(!prefers_chinese("en,zh"));
    }

    #[test]
    fn handles_single_language_or_empty() {
        assert!(prefers_chinese("zh-TW"));
        assert!(!prefers_chinese("en-US"));
        assert!(!prefers_chinese(""));
    }

    #[test]
    fn handles_case_insensitive_values() {
        assert!(prefers_chinese("ZH-CN,en"));
        assert!(!prefers_chinese("EN,zh"));
    }

    #[test]
    fn string_or_display_and_value_cover_raw_and_structured_inputs() {
        let raw: StringOr<DemoPayload> = StringOr::String("# raw".to_string());
        assert_eq!(raw.to_string(), "# raw");
        assert_eq!(raw.value().unwrap_err(), "# raw");

        let structured = StringOr::Value(demo_payload());
        let text = structured.to_string();
        assert!(text.contains("\"name\": \"alice\""));
        assert!(text.contains("\"count\": 7"));
    }

    #[tokio::test]
    async fn header_vals_accepts_x_shard_and_defaults_invalid_values() {
        let req = Request::builder()
            .header(header::AUTHORIZATION, "Bearer token")
            .header("X-Shard", "9")
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();

        let HeaderVals(token, sharding) = HeaderVals::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert_eq!(token, "token");
        assert_eq!(sharding, 9);

        // A present-but-unparseable shard header falls back to shard 0,
        // matching the lenient MCP channel.
        let req = Request::builder()
            .header("Shard-Id", "not-a-number")
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let HeaderVals(_, sharding) = HeaderVals::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert_eq!(sharding, 0);

        // Absent header defaults to shard 0.
        let req = Request::builder().body(()).unwrap();
        let (mut parts, _) = req.into_parts();
        let HeaderVals(token, sharding) = HeaderVals::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert_eq!(token, "");
        assert_eq!(sharding, 0);
    }

    #[tokio::test]
    async fn app_query_rejection_is_json_enveloped() {
        #[derive(Debug, Deserialize)]
        struct DemoQuery {
            #[allow(dead_code)]
            limit: Option<usize>,
        }

        let req = Request::builder().uri("/x?limit=abc").body(()).unwrap();
        let (mut parts, _) = req.into_parts();
        let Err(err) = AppQuery::<DemoQuery>::from_request_parts(&mut parts, &()).await else {
            panic!("expected the query rejection");
        };
        assert_eq!(err.status, StatusCode::BAD_REQUEST);

        // The raw axum extractor would reject with a plain-text body; the
        // wrapper must uphold the "error bodies are always JSON" contract.
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["error"]["message"].as_str().is_some());
    }

    #[tokio::test]
    async fn header_vals_strips_bearer_prefix_once() {
        for (input, expected) in [
            ("Bearer secret", "secret"),
            // Only one prefix is stripped; the rest is the credential.
            ("Bearer Bearer secret", "Bearer secret"),
            ("secret", "secret"),
        ] {
            let req = Request::builder()
                .header(header::AUTHORIZATION, input)
                .body(())
                .unwrap();
            let (mut parts, _) = req.into_parts();
            let HeaderVals(token, _) = HeaderVals::from_request_parts(&mut parts, &())
                .await
                .unwrap();
            assert_eq!(token, expected, "input: {input:?}");
        }
    }
}

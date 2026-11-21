// The legacy error vocabulary: NamedError `{name,data}` and Effect-tagged `{_tag,...}` bodies.
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub body: Value,
}

const REASON_LIMIT: usize = 1024;

fn truncate(reason: &str) -> String {
    // Lengths are UTF-16 units, as in the original.
    let units: Vec<u16> = reason.encode_utf16().collect();
    if units.len() <= REASON_LIMIT {
        return reason.to_string();
    }
    format!("{}\u{2026} ({} more chars)", String::from_utf16_lossy(&units[..REASON_LIMIT]), units.len() - REASON_LIMIT)
}

impl ApiError {
    pub fn new(status: StatusCode, body: Value) -> Self {
        Self { status, body }
    }

    // Schema rejection. `kind` is one of Params | Headers | Query | Body | Payload.
    pub fn schema(kind: &str, reason: impl AsRef<str>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, json!({"name": "BadRequest", "data": {"message": truncate(reason.as_ref()), "kind": kind}}))
    }

    // Handler-raised `HttpApiError.BadRequest`.
    pub fn bad_request() -> Self {
        Self::new(StatusCode::BAD_REQUEST, json!({"_tag": "BadRequest"}))
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, json!({"name": "NotFoundError", "data": {"message": message.into()}}))
    }

    // `{_tag, <field>, message}` 404s such as PermissionNotFoundError{requestID}.
    pub fn tagged_not_found(tag: &str, field: &str, value: &str, message: String) -> Self {
        let mut body = serde_json::Map::new();
        body.insert("_tag".into(), tag.into());
        body.insert(field.into(), value.into());
        body.insert("message".into(), message.into());
        Self::new(StatusCode::NOT_FOUND, Value::Object(body))
    }

    pub fn busy(session_id: &str) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            json!({"_tag": "SessionBusyError", "sessionID": session_id, "message": format!("Session is busy: {session_id}")}),
        )
    }

    pub fn unknown(status: StatusCode, message: impl Into<String>) -> Self {
        let reference = opencode_storage::id::ascending(opencode_storage::id::Prefix::Event);
        let short: String = reference.chars().rev().take(8).collect();
        Self::new(status, json!({"name": "UnknownError", "data": {"message": message.into(), "ref": format!("err_{short}")}}))
    }

    pub fn internal(detail: impl std::fmt::Display) -> Self {
        eprintln!("opencode-server: {detail}");
        Self::unknown(StatusCode::INTERNAL_SERVER_ERROR, "Unexpected server error. Check server logs for details.")
    }

    // v2 `TaggedError`: `{_tag, ...fields}`.
    pub fn tagged(status: StatusCode, tag: &str, fields: Value) -> Self {
        let mut body = serde_json::Map::new();
        body.insert("_tag".into(), tag.into());
        if let Value::Object(fields) = fields {
            body.extend(fields);
        }
        Self::new(status, Value::Object(body))
    }

    // v2 schema rejection; the truncation suffix is ASCII there.
    pub fn invalid_request(kind: &str, reason: impl AsRef<str>) -> Self {
        let units: Vec<u16> = reason.as_ref().encode_utf16().collect();
        let message = if units.len() <= REASON_LIMIT {
            reason.as_ref().to_string()
        } else {
            format!("{}... ({} more chars)", String::from_utf16_lossy(&units[..REASON_LIMIT]), units.len() - REASON_LIMIT)
        };
        Self::tagged(StatusCode::BAD_REQUEST, "InvalidRequestError", json!({"message": message, "kind": kind}))
    }

    pub fn v2_not_found(tag: &str, field: &str, value: &str, message: String) -> Self {
        let mut fields = serde_json::Map::new();
        fields.insert(field.into(), value.into());
        fields.insert("message".into(), message.into());
        Self::tagged(StatusCode::NOT_FOUND, tag, Value::Object(fields))
    }

    pub fn v2_unknown(detail: impl std::fmt::Display) -> Self {
        let reference = uuid::Uuid::new_v4().to_string();
        let reference = format!("err_{}", &reference[..8]);
        tracing::error!(reference = %reference, "{detail}");
        let fields = json!({"message": "Unexpected server error. Check server logs for details.", "ref": reference});
        Self::tagged(StatusCode::INTERNAL_SERVER_ERROR, "UnknownError", fields)
    }

    // Converts a legacy-shaped failure raised by shared code into the v2 vocabulary.
    pub fn into_v2(self) -> Self {
        if self.body.get("_tag").is_some() {
            return self;
        }
        let message = self.body.pointer("/data/message").and_then(Value::as_str).unwrap_or("Request failed").to_string();
        match self.status {
            StatusCode::BAD_REQUEST => {
                let kind = self.body.pointer("/data/kind").and_then(Value::as_str).unwrap_or("Payload").to_string();
                Self::tagged(self.status, "InvalidRequestError", json!({"message": message, "kind": kind}))
            }
            StatusCode::NOT_IMPLEMENTED => Self::tagged(self.status, "UnknownError", json!({"message": message})),
            _ => {
                let mut fields = json!({"message": message});
                if let Some(reference) = self.body.pointer("/data/ref").filter(|r| r.is_string()) {
                    fields["ref"] = reference.clone();
                }
                Self::tagged(self.status, "UnknownError", fields)
            }
        }
    }

    pub fn not_implemented(what: &str) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, json!({"name": "UnknownError", "data": {"message": format!("Not implemented: {what}")}}))
    }

    pub fn config(error: &opencode_core::config::ConfigError) -> Self {
        use opencode_core::config::ConfigError;
        let (name, path, message) = match error {
            ConfigError::Json { path, message } => ("ConfigJsonError", path, message),
            ConfigError::Invalid { path, message } => ("ConfigInvalidError", path, message),
        };
        Self::new(StatusCode::BAD_REQUEST, json!({"name": name, "data": {"path": path, "message": message}}))
    }
}

impl From<opencode_storage::StoreError> for ApiError {
    fn from(error: opencode_storage::StoreError) -> Self {
        Self::internal(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, [(header::CONTENT_TYPE, "application/json")], self.body.to_string()).into_response()
    }
}

pub type ApiResult<T = Response> = Result<T, ApiError>;

pub fn json(value: impl serde::Serialize) -> Response {
    match serde_json::to_string(&value) {
        Ok(body) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

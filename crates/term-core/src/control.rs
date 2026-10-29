use serde_json::{Map, Value, json};
use std::fmt;

pub const MAX_REQUEST_LINE_BYTES: usize = 1024 * 1024;
pub const PROTOCOL_VERSION: u64 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlDirection {
    Right,
    Down,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextScope {
    Screen,
    LastBlock,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlCommand {
    List,
    NewTab,
    Split {
        pane: Option<u64>,
        direction: ControlDirection,
    },
    Focus {
        pane: u64,
    },
    Close {
        pane: u64,
    },
    SendText {
        pane: Option<u64>,
        text: String,
    },
    GetText {
        pane: Option<u64>,
        scope: TextScope,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlRequest {
    pub id: u64,
    pub command: ControlCommand,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlErrorCode {
    MalformedJson,
    UnknownVersion,
    UnknownCommand,
    MissingArguments,
    InvalidArguments,
    LineTooLarge,
    Timeout,
    Unavailable,
    NotFound,
    OperationRefused,
    Internal,
}

impl ControlErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MalformedJson => "MALFORMED_JSON",
            Self::UnknownVersion => "UNKNOWN_VERSION",
            Self::UnknownCommand => "UNKNOWN_COMMAND",
            Self::MissingArguments => "MISSING_ARGUMENTS",
            Self::InvalidArguments => "INVALID_ARGUMENTS",
            Self::LineTooLarge => "LINE_TOO_LARGE",
            Self::Timeout => "TIMEOUT",
            Self::Unavailable => "UNAVAILABLE",
            Self::NotFound => "NOT_FOUND",
            Self::OperationRefused => "OPERATION_REFUSED",
            Self::Internal => "INTERNAL",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlError {
    pub id: Option<u64>,
    pub code: ControlErrorCode,
    pub message: String,
}

impl ControlError {
    pub fn new(code: ControlErrorCode, message: impl Into<String>) -> Self {
        Self {
            id: None,
            code,
            message: message.into(),
        }
    }

    pub fn for_request(
        id: Option<u64>,
        code: ControlErrorCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id,
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ControlError {}

#[derive(Clone, Debug, PartialEq)]
pub enum ControlOutcome {
    Success(Value),
    Failure(ControlError),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ControlResponse {
    pub id: Option<u64>,
    pub outcome: ControlOutcome,
}

fn error_from_value(id: Option<u64>, code: &str, message: &str) -> ControlError {
    let code = match code {
        "MALFORMED_JSON" => ControlErrorCode::MalformedJson,
        "UNKNOWN_VERSION" => ControlErrorCode::UnknownVersion,
        "UNKNOWN_COMMAND" => ControlErrorCode::UnknownCommand,
        "MISSING_ARGUMENTS" => ControlErrorCode::MissingArguments,
        "INVALID_ARGUMENTS" => ControlErrorCode::InvalidArguments,
        "LINE_TOO_LARGE" => ControlErrorCode::LineTooLarge,
        "TIMEOUT" => ControlErrorCode::Timeout,
        "UNAVAILABLE" => ControlErrorCode::Unavailable,
        "NOT_FOUND" => ControlErrorCode::NotFound,
        "OPERATION_REFUSED" => ControlErrorCode::OperationRefused,
        _ => ControlErrorCode::Internal,
    };
    ControlError::for_request(id, code, message)
}

fn raw_id(value: &Value) -> Option<u64> {
    value.get("id").and_then(Value::as_u64)
}

fn required_u64(object: &Map<String, Value>, key: &str) -> Result<u64, ControlErrorCode> {
    let Some(value) = object.get(key) else {
        return Err(ControlErrorCode::MissingArguments);
    };
    value.as_u64().ok_or(ControlErrorCode::InvalidArguments)
}

fn optional_u64(object: &Map<String, Value>, key: &str) -> Result<Option<u64>, ControlErrorCode> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or(ControlErrorCode::InvalidArguments),
    }
}

fn allowed_fields(object: &Map<String, Value>, fields: &[&str]) -> bool {
    object.keys().all(|key| fields.contains(&key.as_str()))
}

fn command_from_object(object: &Map<String, Value>) -> Result<ControlCommand, ControlErrorCode> {
    let command = object.get("cmd").and_then(Value::as_str).ok_or_else(|| {
        if object.contains_key("cmd") {
            ControlErrorCode::InvalidArguments
        } else {
            ControlErrorCode::MissingArguments
        }
    })?;
    let base = ["v", "id", "cmd"];
    match command {
        "list" if allowed_fields(object, &base) => Ok(ControlCommand::List),
        "new-tab" if allowed_fields(object, &base) => Ok(ControlCommand::NewTab),
        "split" => {
            if !allowed_fields(object, &["v", "id", "cmd", "pane", "direction"]) {
                return Err(ControlErrorCode::InvalidArguments);
            }
            let direction = match object.get("direction").and_then(Value::as_str) {
                Some("right") => ControlDirection::Right,
                Some("down") => ControlDirection::Down,
                Some(_) => return Err(ControlErrorCode::InvalidArguments),
                None => return Err(ControlErrorCode::MissingArguments),
            };
            Ok(ControlCommand::Split {
                pane: optional_u64(object, "pane")?,
                direction,
            })
        }
        "focus" | "close" => {
            if !allowed_fields(object, &["v", "id", "cmd", "pane"]) {
                return Err(ControlErrorCode::InvalidArguments);
            }
            let pane = required_u64(object, "pane")?;
            if command == "focus" {
                Ok(ControlCommand::Focus { pane })
            } else {
                Ok(ControlCommand::Close { pane })
            }
        }
        "send-text" => {
            if !allowed_fields(object, &["v", "id", "cmd", "pane", "text"]) {
                return Err(ControlErrorCode::InvalidArguments);
            }
            let text = object.get("text").and_then(Value::as_str).ok_or_else(|| {
                if object.contains_key("text") {
                    ControlErrorCode::InvalidArguments
                } else {
                    ControlErrorCode::MissingArguments
                }
            })?;
            Ok(ControlCommand::SendText {
                pane: optional_u64(object, "pane")?,
                text: text.to_owned(),
            })
        }
        "get-text" => {
            if !allowed_fields(object, &["v", "id", "cmd", "pane", "scope"]) {
                return Err(ControlErrorCode::InvalidArguments);
            }
            let scope = match object.get("scope") {
                None => TextScope::Screen,
                Some(Value::String(scope)) if scope == "screen" => TextScope::Screen,
                Some(Value::String(scope)) if scope == "last-block" => TextScope::LastBlock,
                Some(_) => return Err(ControlErrorCode::InvalidArguments),
            };
            Ok(ControlCommand::GetText {
                pane: optional_u64(object, "pane")?,
                scope,
            })
        }
        "list" | "new-tab" => Err(ControlErrorCode::InvalidArguments),
        _ => Err(ControlErrorCode::UnknownCommand),
    }
}

pub fn parse_request(line: &[u8]) -> Result<ControlRequest, ControlError> {
    if line.len() > MAX_REQUEST_LINE_BYTES {
        return Err(ControlError::new(
            ControlErrorCode::LineTooLarge,
            "request line exceeds 1 MiB",
        ));
    }
    let content = line.strip_suffix(b"\n").unwrap_or(line);
    let content = content.strip_suffix(b"\r").unwrap_or(content);
    let value: Value = serde_json::from_slice(content).map_err(|_| {
        ControlError::new(ControlErrorCode::MalformedJson, "malformed JSON request")
    })?;
    let Some(object) = value.as_object() else {
        return Err(ControlError::new(
            ControlErrorCode::InvalidArguments,
            "request must be a JSON object",
        ));
    };
    let id = raw_id(&value);
    let version = object
        .get("v")
        .ok_or_else(|| {
            ControlError::for_request(
                id,
                ControlErrorCode::MissingArguments,
                "missing protocol version",
            )
        })?
        .as_u64()
        .ok_or_else(|| {
            ControlError::for_request(
                id,
                ControlErrorCode::InvalidArguments,
                "protocol version must be an unsigned integer",
            )
        })?;
    if version != PROTOCOL_VERSION {
        return Err(ControlError::for_request(
            id,
            ControlErrorCode::UnknownVersion,
            "unsupported protocol version",
        ));
    }
    let id = object
        .get("id")
        .ok_or_else(|| {
            ControlError::for_request(
                None,
                ControlErrorCode::MissingArguments,
                "missing request id",
            )
        })?
        .as_u64()
        .ok_or_else(|| {
            ControlError::for_request(
                None,
                ControlErrorCode::InvalidArguments,
                "request id must be an unsigned integer",
            )
        })?;
    let command = command_from_object(object)
        .map_err(|code| ControlError::for_request(Some(id), code, "invalid command arguments"))?;
    Ok(ControlRequest { id, command })
}

pub fn serialize_request(request: &ControlRequest) -> Result<Vec<u8>, serde_json::Error> {
    let mut object = Map::from_iter([
        ("v".to_owned(), json!(PROTOCOL_VERSION)),
        ("id".to_owned(), json!(request.id)),
    ]);
    let (name, args) = match &request.command {
        ControlCommand::List => ("list", Vec::new()),
        ControlCommand::NewTab => ("new-tab", Vec::new()),
        ControlCommand::Split { pane, direction } => {
            let mut args = Vec::new();
            if let Some(pane) = pane {
                args.push(("pane", json!(pane)));
            }
            args.push((
                "direction",
                json!(match direction {
                    ControlDirection::Right => "right",
                    ControlDirection::Down => "down",
                }),
            ));
            ("split", args)
        }
        ControlCommand::Focus { pane } => ("focus", vec![("pane", json!(pane))]),
        ControlCommand::Close { pane } => ("close", vec![("pane", json!(pane))]),
        ControlCommand::SendText { pane, text } => {
            let mut args = Vec::new();
            if let Some(pane) = pane {
                args.push(("pane", json!(pane)));
            }
            args.push(("text", json!(text)));
            ("send-text", args)
        }
        ControlCommand::GetText { pane, scope } => {
            let mut args = Vec::new();
            if let Some(pane) = pane {
                args.push(("pane", json!(pane)));
            }
            args.push((
                "scope",
                json!(match scope {
                    TextScope::Screen => "screen",
                    TextScope::LastBlock => "last-block",
                }),
            ));
            ("get-text", args)
        }
    };
    object.insert("cmd".to_owned(), json!(name));
    for (key, value) in args {
        object.insert(key.to_owned(), value);
    }
    let mut bytes = serde_json::to_vec(&Value::Object(object))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn serialize_response(response: &ControlResponse) -> Result<Vec<u8>, serde_json::Error> {
    let value = match &response.outcome {
        ControlOutcome::Success(result) => {
            json!({ "v": PROTOCOL_VERSION, "id": response.id, "ok": true, "result": result })
        }
        ControlOutcome::Failure(error) => {
            json!({ "v": PROTOCOL_VERSION, "id": response.id.or(error.id), "ok": false, "error": { "code": error.code.as_str(), "message": error.message } })
        }
    };
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn parse_response(line: &[u8]) -> Result<ControlResponse, ControlError> {
    let content = line.strip_suffix(b"\n").unwrap_or(line);
    let content = content.strip_suffix(b"\r").unwrap_or(content);
    let value: Value = serde_json::from_slice(content).map_err(|_| {
        ControlError::new(ControlErrorCode::MalformedJson, "malformed JSON response")
    })?;
    let object = value.as_object().ok_or_else(|| {
        ControlError::new(
            ControlErrorCode::InvalidArguments,
            "response must be a JSON object",
        )
    })?;
    let id = object.get("id").and_then(Value::as_u64);
    if object.get("v").and_then(Value::as_u64) != Some(PROTOCOL_VERSION) {
        return Err(ControlError::for_request(
            id,
            ControlErrorCode::UnknownVersion,
            "unsupported response version",
        ));
    }
    match object.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(ControlResponse {
            id,
            outcome: ControlOutcome::Success(object.get("result").cloned().unwrap_or(Value::Null)),
        }),
        Some(false) => {
            let error = object
                .get("error")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    ControlError::for_request(
                        id,
                        ControlErrorCode::InvalidArguments,
                        "response error is missing",
                    )
                })?;
            let code = error.get("code").and_then(Value::as_str).ok_or_else(|| {
                ControlError::for_request(
                    id,
                    ControlErrorCode::InvalidArguments,
                    "response error code is missing",
                )
            })?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ControlError::for_request(
                        id,
                        ControlErrorCode::InvalidArguments,
                        "response error message is missing",
                    )
                })?;
            Ok(ControlResponse {
                id,
                outcome: ControlOutcome::Failure(error_from_value(id, code, message)),
            })
        }
        None => Err(ControlError::for_request(
            id,
            ControlErrorCode::InvalidArguments,
            "response ok flag must be a boolean",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ControlCommand, ControlDirection, ControlErrorCode, MAX_REQUEST_LINE_BYTES, TextScope,
        parse_request,
    };

    fn parse(source: &str) -> Result<super::ControlRequest, super::ControlError> {
        parse_request(source.as_bytes())
    }

    #[test]
    fn parses_every_v1_command_and_its_optional_defaults() {
        let cases = [
            (r#"{"v":1,"id":1,"cmd":"list"}"#, ControlCommand::List),
            (r#"{"v":1,"id":2,"cmd":"new-tab"}"#, ControlCommand::NewTab),
            (
                r#"{"v":1,"id":3,"cmd":"split","direction":"right"}"#,
                ControlCommand::Split {
                    pane: None,
                    direction: ControlDirection::Right,
                },
            ),
            (
                r#"{"v":1,"id":4,"cmd":"focus","pane":7}"#,
                ControlCommand::Focus { pane: 7 },
            ),
            (
                r#"{"v":1,"id":5,"cmd":"close","pane":7}"#,
                ControlCommand::Close { pane: 7 },
            ),
            (
                r#"{"v":1,"id":6,"cmd":"send-text","text":"echo hi\n"}"#,
                ControlCommand::SendText {
                    pane: None,
                    text: "echo hi\n".into(),
                },
            ),
            (
                r#"{"v":1,"id":7,"cmd":"get-text"}"#,
                ControlCommand::GetText {
                    pane: None,
                    scope: TextScope::Screen,
                },
            ),
        ];

        for (source, expected) in cases {
            assert_eq!(parse(source).unwrap().command, expected, "{source}");
        }
    }

    #[test]
    fn distinguishes_protocol_error_classes() {
        let cases = [
            ("{", ControlErrorCode::MalformedJson),
            (
                r#"{"v":2,"id":1,"cmd":"list"}"#,
                ControlErrorCode::UnknownVersion,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"unknown"}"#,
                ControlErrorCode::UnknownCommand,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"focus"}"#,
                ControlErrorCode::MissingArguments,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"focus","pane":"bad"}"#,
                ControlErrorCode::InvalidArguments,
            ),
        ];

        for (source, code) in cases {
            assert_eq!(parse(source).unwrap_err().code, code, "{source}");
        }
    }

    #[test]
    fn validates_each_command_argument_shape() {
        let cases = [
            (
                r#"{"v":1,"id":1,"cmd":"split"}"#,
                ControlErrorCode::MissingArguments,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"split","direction":"left"}"#,
                ControlErrorCode::InvalidArguments,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"send-text"}"#,
                ControlErrorCode::MissingArguments,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"send-text","text":1}"#,
                ControlErrorCode::InvalidArguments,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"focus"}"#,
                ControlErrorCode::MissingArguments,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"focus","pane":-1}"#,
                ControlErrorCode::InvalidArguments,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"close"}"#,
                ControlErrorCode::MissingArguments,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"close","pane":"one"}"#,
                ControlErrorCode::InvalidArguments,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"new-tab","extra":true}"#,
                ControlErrorCode::InvalidArguments,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"get-text","scope":null}"#,
                ControlErrorCode::InvalidArguments,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"get-text","scope":"all"}"#,
                ControlErrorCode::InvalidArguments,
            ),
            (
                r#"{"v":1,"id":1,"cmd":"list","extra":true}"#,
                ControlErrorCode::InvalidArguments,
            ),
        ];

        for (source, code) in cases {
            assert_eq!(parse(source).unwrap_err().code, code, "{source}");
        }
    }

    #[test]
    fn serializer_round_trips_requests_and_response_errors() {
        let request = super::ControlRequest {
            id: 23,
            command: ControlCommand::GetText {
                pane: Some(9),
                scope: TextScope::LastBlock,
            },
        };
        let bytes = super::serialize_request(&request).unwrap();
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert_eq!(parse_request(&bytes).unwrap(), request);
        let response = super::ControlResponse {
            id: Some(23),
            outcome: super::ControlOutcome::Failure(super::ControlError::for_request(
                Some(23),
                ControlErrorCode::NotFound,
                "missing pane",
            )),
        };
        let bytes = super::serialize_response(&response).unwrap();
        assert_eq!(super::parse_response(&bytes).unwrap(), response);
    }

    #[test]
    fn rejects_a_request_line_above_one_mebibyte() {
        let mut request = format!(
            r#"{{"v":1,"id":1,"cmd":"list","pad":"{}"}}"#,
            "x".repeat(MAX_REQUEST_LINE_BYTES)
        );
        assert!(request.len() > MAX_REQUEST_LINE_BYTES);
        assert_eq!(
            parse_request(request.as_bytes()).unwrap_err().code,
            ControlErrorCode::LineTooLarge
        );
        request.clear();
    }
}

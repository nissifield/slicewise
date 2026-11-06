//! Socket protocol types and framing (design §6.2): one JSON object per line,
//! 64 KiB limit, closed enums, `deny_unknown_fields`, `v` must be 1.

use std::fmt;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::config::{Class, valid_unit_name};

pub const MAX_MESSAGE: usize = 64 * 1024;
pub const JOBS_HINT_MAX: u32 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Admit {
        v: u32,
        unit: String,
        class: Class,
        mem_hint: Option<u64>,
        jobs_hint: Option<u32>,
        queue: bool,
    },
    Status {
        v: u32,
    },
    Explain {
        v: u32,
        target: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Op {
    Admit,
    Status,
    Explain,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequest {
    v: u32,
    op: Op,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    class: Option<Class>,
    #[serde(default)]
    mem_hint: Option<u64>,
    #[serde(default)]
    jobs_hint: Option<u32>,
    #[serde(default)]
    queue: Option<bool>,
    #[serde(default)]
    target: Option<String>,
}

fn parse_request(bytes: &[u8]) -> Result<Request, ProtoError> {
    let raw: RawRequest =
        serde_json::from_slice(bytes).map_err(|e| ProtoError::BadRequest(e.to_string()))?;
    if raw.v != 1 {
        return Err(ProtoError::UnsupportedVersion(raw.v));
    }
    match raw.op {
        Op::Admit => {
            if raw.target.is_some() {
                return Err(unexpected("target", "admit"));
            }
            let unit = raw
                .unit
                .ok_or_else(|| ProtoError::BadRequest("missing field `unit`".into()))?;
            let class = raw
                .class
                .ok_or_else(|| ProtoError::BadRequest("missing field `class`".into()))?;
            if !valid_unit_name(&unit) {
                return Err(ProtoError::BadRequest(format!(
                    "unit = {unit:?}: invalid systemd unit name"
                )));
            }
            if let Some(jobs) = raw.jobs_hint
                && jobs > JOBS_HINT_MAX
            {
                return Err(ProtoError::BadRequest(format!(
                    "jobs_hint = {jobs}: must be <= {JOBS_HINT_MAX}"
                )));
            }
            Ok(Request::Admit {
                v: raw.v,
                unit,
                class,
                mem_hint: raw.mem_hint,
                jobs_hint: raw.jobs_hint,
                queue: raw.queue.unwrap_or(true),
            })
        }
        Op::Status => {
            for (present, field) in [
                (raw.unit.is_some(), "unit"),
                (raw.class.is_some(), "class"),
                (raw.mem_hint.is_some(), "mem_hint"),
                (raw.jobs_hint.is_some(), "jobs_hint"),
                (raw.queue.is_some(), "queue"),
                (raw.target.is_some(), "target"),
            ] {
                if present {
                    return Err(unexpected(field, "status"));
                }
            }
            Ok(Request::Status { v: raw.v })
        }
        Op::Explain => {
            for (present, field) in [
                (raw.unit.is_some(), "unit"),
                (raw.class.is_some(), "class"),
                (raw.mem_hint.is_some(), "mem_hint"),
                (raw.jobs_hint.is_some(), "jobs_hint"),
                (raw.queue.is_some(), "queue"),
            ] {
                if present {
                    return Err(unexpected(field, "explain"));
                }
            }
            let target = raw
                .target
                .ok_or_else(|| ProtoError::BadRequest("missing field `target`".into()))?;
            Ok(Request::Explain { v: raw.v, target })
        }
    }
}

fn unexpected(field: &str, op: &str) -> ProtoError {
    ProtoError::BadRequest(format!("unexpected field `{field}` for op {op}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionReply {
    Admit,
    Queued,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    UnsupportedVersion,
    TooLarge,
    Timeout,
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub v: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<DecisionReply>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jobs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Response {
    fn new(ok: bool) -> Self {
        Self {
            v: 1,
            ok,
            decision: None,
            position: None,
            jobs: None,
            reason: None,
            error: None,
            detail: None,
        }
    }

    pub fn admit(jobs: u32, reason: impl Into<String>) -> Self {
        Self {
            decision: Some(DecisionReply::Admit),
            jobs: Some(jobs),
            reason: Some(reason.into()),
            ..Self::new(true)
        }
    }

    pub fn queued(position: usize, reason: impl Into<String>) -> Self {
        Self {
            decision: Some(DecisionReply::Queued),
            position: Some(position),
            reason: Some(reason.into()),
            ..Self::new(true)
        }
    }

    pub fn reject(reason: impl Into<String>) -> Self {
        Self {
            decision: Some(DecisionReply::Reject),
            reason: Some(reason.into()),
            ..Self::new(true)
        }
    }

    pub fn error(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            error: Some(code),
            detail: Some(detail.into()),
            ..Self::new(false)
        }
    }
}

#[derive(Debug)]
pub enum ProtoError {
    Io(io::Error),
    BadRequest(String),
    UnsupportedVersion(u32),
    TooLarge,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Io(error) => write!(f, "i/o error: {error}"),
            ProtoError::BadRequest(detail) => write!(f, "bad_request: {detail}"),
            ProtoError::UnsupportedVersion(version) => {
                write!(f, "unsupported_version: v = {version}")
            }
            ProtoError::TooLarge => write!(f, "too_large: limit is {MAX_MESSAGE} bytes"),
        }
    }
}

impl std::error::Error for ProtoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProtoError::Io(error) => Some(error),
            ProtoError::BadRequest(_)
            | ProtoError::UnsupportedVersion(_)
            | ProtoError::TooLarge => None,
        }
    }
}

impl From<io::Error> for ProtoError {
    fn from(error: io::Error) -> Self {
        ProtoError::Io(error)
    }
}

/// Read one newline-terminated request; EOF terminates a partial final line.
pub fn read_one<R: Read>(reader: &mut R) -> Result<Request, ProtoError> {
    let mut buffer = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match reader.read(&mut byte)? {
            0 => break,
            _ if byte[0] == b'\n' => break,
            _ => {
                if buffer.len() >= MAX_MESSAGE {
                    return Err(ProtoError::TooLarge);
                }
                buffer.push(byte[0]);
            }
        }
    }
    if buffer.is_empty() {
        return Err(ProtoError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed",
        )));
    }
    parse_request(&buffer)
}

/// Write one response followed by a newline, enforcing the same size limit.
pub fn write_one<W: Write>(writer: &mut W, response: &Response) -> Result<(), ProtoError> {
    let mut bytes = serde_json::to_vec(response)
        .map_err(|e| ProtoError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
    if bytes.len() > MAX_MESSAGE {
        return Err(ProtoError::TooLarge);
    }
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn parses_the_design_admit_example() {
        let request = parse_request(
            br#"{"v":1,"op":"admit","unit":"slicewise-batch-4711-1.service","class":"batch","mem_hint":3221225472,"jobs_hint":8,"queue":true}"#,
        )
        .unwrap();
        assert_eq!(
            request,
            Request::Admit {
                v: 1,
                unit: "slicewise-batch-4711-1.service".into(),
                class: Class::Batch,
                mem_hint: Some(3_221_225_472),
                jobs_hint: Some(8),
                queue: true,
            }
        );
    }

    #[test]
    fn queue_defaults_to_true_and_status_and_explain_parse() {
        let request =
            parse_request(br#"{"v":1,"op":"admit","unit":"a.service","class":"normal"}"#).unwrap();
        let Request::Admit { queue, .. } = request else {
            panic!("admit");
        };
        assert!(queue);
        assert_eq!(
            parse_request(br#"{"v":1,"op":"status"}"#).unwrap(),
            Request::Status { v: 1 }
        );
        assert_eq!(
            parse_request(br#"{"v":1,"op":"explain","target":"batch"}"#).unwrap(),
            Request::Explain {
                v: 1,
                target: "batch".into()
            }
        );
    }

    #[test]
    fn unknown_fields_and_variants_are_rejected() {
        let error = parse_request(br#"{"v":1,"op":"status","mystery":1}"#).unwrap_err();
        assert!(matches!(error, ProtoError::BadRequest(_)));
        assert!(error.to_string().contains("unknown field `mystery`"));

        let error = parse_request(br#"{"v":1,"op":"dance"}"#).unwrap_err();
        assert!(error.to_string().contains("unknown variant `dance`"));

        let error = parse_request(br#"{"v":1,"op":"admit","unit":"a.service","class":"urgent"}"#)
            .unwrap_err();
        assert!(error.to_string().contains("unknown variant `urgent`"));
    }

    #[test]
    fn bad_version_is_typed() {
        let error = parse_request(br#"{"v":2,"op":"status"}"#).unwrap_err();
        assert!(matches!(error, ProtoError::UnsupportedVersion(2)));
    }

    #[test]
    fn field_validation_is_strict() {
        let error = parse_request(br#"{"v":1,"op":"status","unit":"a.service"}"#).unwrap_err();
        assert!(error.to_string().contains("unexpected field `unit`"));

        let error = parse_request(br#"{"v":1,"op":"admit","unit":"bad name","class":"batch"}"#)
            .unwrap_err();
        assert!(error.to_string().contains("invalid systemd unit name"));

        let error = parse_request(
            br#"{"v":1,"op":"admit","unit":"a.service","class":"batch","jobs_hint":4097}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("must be <= 4096"));
    }

    #[test]
    fn read_one_stops_at_newline_and_enforces_the_size_limit() {
        let mut cursor = Cursor::new(b"{\"v\":1,\"op\":\"status\"}\n".to_vec());
        assert_eq!(read_one(&mut cursor).unwrap(), Request::Status { v: 1 });

        let mut oversized = vec![b'x'; MAX_MESSAGE + 1];
        oversized.push(b'\n');
        let error = read_one(&mut Cursor::new(oversized)).unwrap_err();
        assert!(matches!(error, ProtoError::TooLarge));

        let error = read_one(&mut Cursor::new(Vec::new())).unwrap_err();
        assert!(matches!(error, ProtoError::Io(_)));
    }

    #[test]
    fn response_round_trips_and_is_newline_terminated() {
        let response = Response::admit(3, "headroom 4.4G >= 3.0G + 0.6G");
        let mut bytes = Vec::new();
        write_one(&mut bytes, &response).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let decoded: Response = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(decoded, response);
        let value: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(value["v"], 1);
        assert_eq!(value["ok"], true);
        assert_eq!(value["decision"], "admit");
        assert_eq!(value["jobs"], 3);

        let response = Response::error(ErrorCode::BadRequest, "class: unknown variant");
        let mut bytes = Vec::new();
        write_one(&mut bytes, &response).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"], "bad_request");
        assert!(value.get("decision").is_none());
    }
}

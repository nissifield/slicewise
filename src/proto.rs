//! Socket protocol types and framing (design §6.2): one JSON object per line,
//! 64 KiB limit, closed enums, `deny_unknown_fields`, `v` must be 1.

use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{Class, valid_unit_name};

pub const MAX_MESSAGE: usize = 64 * 1024;
pub const JOBS_HINT_MAX: u32 = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
pub enum Request {
    Admit {
        v: u32,
        unit: String,
        class: Class,
        #[serde(default)]
        mem_hint: Option<u64>,
        #[serde(default)]
        jobs_hint: Option<u32>,
        #[serde(default = "default_queue")]
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

fn default_queue() -> bool {
    true
}

type ProtoResult<T> = Result<T, (ErrorCode, String)>;

impl Request {
    fn validate(&self, mem_total: u64) -> ProtoResult<()> {
        match self {
            Request::Admit {
                v,
                unit,
                mem_hint,
                jobs_hint,
                ..
            } => {
                check_version(*v)?;
                if !valid_unit_name(unit) {
                    return Err((
                        ErrorCode::BadRequest,
                        format!("unit = {unit:?}: invalid systemd unit name"),
                    ));
                }
                if let Some(hint) = mem_hint
                    && *hint > mem_total
                {
                    return Err((
                        ErrorCode::BadRequest,
                        format!("mem_hint = {hint}: must be <= MemTotal {mem_total}"),
                    ));
                }
                if let Some(jobs) = jobs_hint
                    && *jobs > JOBS_HINT_MAX
                {
                    return Err((
                        ErrorCode::BadRequest,
                        format!("jobs_hint = {jobs}: must be <= {JOBS_HINT_MAX}"),
                    ));
                }
            }
            Request::Status { v } => check_version(*v)?,
            Request::Explain { v, target } => {
                check_version(*v)?;
                if !valid_unit_name(target)
                    && !matches!(target.as_str(), "interactive" | "normal" | "batch")
                {
                    return Err((
                        ErrorCode::BadRequest,
                        format!("target = {target:?}: invalid unit name or class"),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn check_version(v: u32) -> ProtoResult<()> {
    if v == 1 {
        Ok(())
    } else {
        Err((ErrorCode::UnsupportedVersion, format!("v = {v}")))
    }
}

fn parse_request(bytes: &[u8], mem_total: u64) -> ProtoResult<Request> {
    let request: Request =
        serde_json::from_slice(bytes).map_err(|e| (ErrorCode::BadRequest, e.to_string()))?;
    request.validate(mem_total)?;
    Ok(request)
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub status: Option<Status>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explain: Option<Explain>,
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
            status: None,
            explain: None,
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

    pub fn status(status: Status) -> Self {
        Self {
            status: Some(status),
            ..Self::new(true)
        }
    }

    pub fn explain(explain: Explain) -> Self {
        Self {
            explain: Some(explain),
            ..Self::new(true)
        }
    }
}

/// `status` payload (design §8): the pool, one row per class, protected PSI and the queue.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub cap: u64,
    pub working_set: u64,
    pub headroom: u64,
    pub margin: u64,
    pub classes: Vec<ClassRow>,
    pub protected: Vec<ProtectedRow>,
    pub queue: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClassRow {
    pub class: Class,
    pub memory: u64,
    pub memory_high: Option<u64>,
    pub cpu_quota: Option<u64>,
    pub cpu_psi: f32,
    pub memory_psi: f32,
    pub pids: u64,
    pub brake_secs: Option<u64>,
    pub clamp_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProtectedRow {
    pub unit: String,
    pub cpu_psi: f32,
}

/// `explain` payload (design §8): ring-buffer events for a unit or class, plus now.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Explain {
    pub target: String,
    pub class: Option<Class>,
    pub slice: Option<String>,
    pub events: Vec<Event>,
    pub current: Option<ClassRow>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub age_secs: u64,
    pub event: String,
    pub unit: Option<String>,
    pub reason: String,
}

/// `$XDG_RUNTIME_DIR/<NAME>/<NAME>.sock` (design §6.2); there is no `/tmp` fallback.
pub fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty())?;
    let name = env!("CARGO_PKG_NAME");
    Some(
        PathBuf::from(runtime)
            .join(name)
            .join(format!("{name}.sock")),
    )
}

/// Read one newline-terminated request; EOF terminates a partial final line.
pub fn read_one<R: BufRead>(reader: &mut R, mem_total: u64) -> ProtoResult<Request> {
    let mut buffer = Vec::new();
    let mut limited = Read::take(&mut *reader, (MAX_MESSAGE + 1) as u64);
    limited.read_until(b'\n', &mut buffer).map_err(|e| {
        let code = if matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            ErrorCode::Timeout
        } else {
            ErrorCode::BadRequest
        };
        (code, format!("read: {e}"))
    })?;
    if buffer.last() == Some(&b'\n') {
        buffer.pop();
    }
    if buffer.len() > MAX_MESSAGE {
        return Err((ErrorCode::TooLarge, format!("limit is {MAX_MESSAGE} bytes")));
    }
    if buffer.is_empty() {
        return Err((ErrorCode::BadRequest, "connection closed".to_string()));
    }
    parse_request(&buffer, mem_total)
}

/// Write one response followed by a newline, enforcing the same size limit.
pub fn write_one<W: Write>(writer: &mut W, response: &Response) -> io::Result<()> {
    let mut bytes =
        serde_json::to_vec(response).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if bytes.len() > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response too large",
        ));
    }
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    struct WouldBlock;
    impl Read for WouldBlock {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }

    #[test]
    fn read_timeout_maps_to_timeout() {
        let mut reader = io::BufReader::new(WouldBlock);
        let (code, _) = read_one(&mut reader, u64::MAX).unwrap_err();
        assert!(matches!(code, ErrorCode::Timeout));
    }

    use super::*;

    const MEM: u64 = 64 << 30;

    fn bad(bytes: &[u8]) -> (ErrorCode, String) {
        parse_request(bytes, MEM).unwrap_err()
    }

    #[test]
    fn parses_the_design_admit_example() {
        let request = parse_request(
            br#"{"v":1,"op":"admit","unit":"slicewise-batch-4711-1.service","class":"batch","mem_hint":3221225472,"jobs_hint":8,"queue":true}"#,
            MEM,
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
        let request = parse_request(
            br#"{"v":1,"op":"admit","unit":"a.service","class":"normal"}"#,
            MEM,
        )
        .unwrap();
        let Request::Admit { queue, .. } = request else {
            panic!("admit");
        };
        assert!(queue);
        assert_eq!(
            parse_request(br#"{"v":1,"op":"status"}"#, MEM).unwrap(),
            Request::Status { v: 1 }
        );
        assert_eq!(
            parse_request(br#"{"v":1,"op":"explain","target":"batch"}"#, MEM).unwrap(),
            Request::Explain {
                v: 1,
                target: "batch".into()
            }
        );
    }

    #[test]
    fn mem_hint_must_fit_mem_total() {
        let (code, detail) = parse_request(
            br#"{"v":1,"op":"admit","unit":"a.service","class":"batch","mem_hint":18446744073709551615}"#,
            1 << 30,
        )
        .unwrap_err();
        assert_eq!(code, ErrorCode::BadRequest);
        assert_eq!(
            detail,
            "mem_hint = 18446744073709551615: must be <= MemTotal 1073741824"
        );
    }

    #[test]
    fn explain_target_is_a_unit_or_class() {
        for target in ["interactive", "normal", "batch", "a.service"] {
            let request = format!(r#"{{"v":1,"op":"explain","target":"{target}"}}"#);
            assert!(parse_request(request.as_bytes(), MEM).is_ok(), "{target}");
        }
        let (code, detail) =
            parse_request(br#"{"v":1,"op":"explain","target":"not a unit"}"#, MEM).unwrap_err();
        assert_eq!(code, ErrorCode::BadRequest);
        assert!(detail.contains("invalid unit name or class"), "{detail}");
    }

    #[test]
    fn unknown_fields_and_variants_are_rejected() {
        let (code, detail) = bad(br#"{"v":1,"op":"status","mystery":1}"#);
        assert_eq!(code, ErrorCode::BadRequest);
        assert!(detail.contains("unknown field `mystery`"));

        let (_, detail) =
            bad(br#"{"v":1,"op":"admit","unit":"a.service","class":"batch","target":"x"}"#);
        assert!(detail.contains("unknown field `target`"));

        let (_, detail) = bad(br#"{"v":1,"op":"dance"}"#);
        assert!(detail.contains("unknown variant `dance`"));

        let (_, detail) = bad(br#"{"v":1,"op":"admit","unit":"a.service","class":"urgent"}"#);
        assert!(detail.contains("unknown variant `urgent`"));
    }

    #[test]
    fn bad_version_is_typed() {
        let (code, detail) = bad(br#"{"v":2,"op":"status"}"#);
        assert_eq!(code, ErrorCode::UnsupportedVersion);
        assert_eq!(detail, "v = 2");
    }

    #[test]
    fn field_validation_is_strict() {
        let (_, detail) = bad(br#"{"v":1,"op":"status","unit":"a.service"}"#);
        assert!(detail.contains("unknown field `unit`"));

        let (_, detail) = bad(br#"{"v":1,"op":"admit","unit":"bad name","class":"batch"}"#);
        assert!(detail.contains("invalid systemd unit name"));

        let (_, detail) =
            bad(br#"{"v":1,"op":"admit","unit":"a.service","class":"batch","jobs_hint":4097}"#);
        assert!(detail.contains("must be <= 4096"));
    }

    #[test]
    fn read_one_stops_at_newline_and_enforces_the_size_limit() {
        let mut cursor = Cursor::new(b"{\"v\":1,\"op\":\"status\"}\n".to_vec());
        assert_eq!(
            read_one(&mut cursor, MEM).unwrap(),
            Request::Status { v: 1 }
        );

        let mut oversized = vec![b'x'; MAX_MESSAGE + 1];
        oversized.push(b'\n');
        let (code, _) = read_one(&mut Cursor::new(oversized), MEM).unwrap_err();
        assert_eq!(code, ErrorCode::TooLarge);

        let (code, detail) = read_one(&mut Cursor::new(Vec::new()), MEM).unwrap_err();
        assert_eq!(code, ErrorCode::BadRequest);
        assert_eq!(detail, "connection closed");
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

    #[test]
    fn response_tolerates_unknown_fields() {
        let response: Response =
            serde_json::from_slice(br#"{"v":1,"ok":true,"future":1}"#).unwrap();
        assert_eq!(response, Response::new(true));
    }

    #[test]
    fn socket_round_trip_over_a_unix_stream_pair() {
        use std::io::Write as _;
        use std::os::unix::net::UnixStream;
        use std::thread;

        let (mut client, server) = UnixStream::pair().unwrap();
        let handle = thread::spawn(move || {
            let mut reader = io::BufReader::new(server.try_clone().unwrap());
            let request = read_one(&mut reader, MEM).unwrap();
            let Request::Admit { unit, .. } = request else {
                panic!("expected admit");
            };
            let mut writer = server;
            write_one(&mut writer, &Response::admit(3, format!("admitted {unit}"))).unwrap();
        });
        client
            .write_all(b"{\"v\":1,\"op\":\"admit\",\"unit\":\"a.service\",\"class\":\"batch\"}\n")
            .unwrap();
        let mut reader = io::BufReader::new(client);
        let mut line = String::new();
        io::BufRead::read_line(&mut reader, &mut line).unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(response.decision, Some(DecisionReply::Admit));
        assert_eq!(response.jobs, Some(3));
        assert_eq!(response.reason.as_deref(), Some("admitted a.service"));
        handle.join().unwrap();
    }
}

//! CAD bridge wire protocol (`docs/occt_bridge_protocol.v1.json`).
//!
//! Length-prefixed JSON over stdio. This module is the Studio-side framing
//! client and the stub server loop used by `reyn-cad-bridge`. No OpenCASCADE
//! code lives here — the stub returns a fixed triangle mesh so IPC, cancel,
//! timeout, and oversize fail-closed behavior can be proven first.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const PROTOCOL_SCHEMA: &str = "com.reyn.cad-bridge.protocol/1";
pub const PROTOCOL_DOC: &str = "docs/occt_bridge_protocol.v1.json";
pub const MAX_REQUEST_BYTES: usize = 67_108_864;
pub const MAX_RESPONSE_BYTES: usize = 268_435_456;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
pub const STUB_BRIDGE_VERSION: &str = "stub-0.1.0";
pub const STUB_OCCT_VERSION: &str = "none";
/// Path marker that makes the stub sleep in 50 ms slices (cancel / timeout tests).
pub const STUB_SLOW_MARKER: &str = "__slow__";
/// Path marker that makes the stub treat the file as an assembly.
pub const STUB_ASSEMBLY_MARKER: &str = "assembly";
/// Stub occurrence paths returned by `list_occurrences` for assembly markers.
pub const STUB_OCCURRENCE_PATHS: [&str; 2] = ["/Product/Part-1", "/Product/Part-2"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    Io(String),
    Oversize {
        side: &'static str,
        len: u32,
        max: usize,
    },
    Json(String),
    Protocol(String),
    Timeout,
    Cancelled,
    BridgeExit {
        status: Option<i32>,
        stderr: String,
    },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "cad-bridge io: {message}"),
            Self::Oversize { side, len, max } => {
                write!(f, "cad-bridge {side} oversize: {len} > {max}")
            }
            Self::Json(message) => write!(f, "cad-bridge json: {message}"),
            Self::Protocol(message) => write!(f, "cad-bridge protocol: {message}"),
            Self::Timeout => write!(f, "cad-bridge timeout"),
            Self::Cancelled => write!(f, "cad-bridge cancelled"),
            Self::BridgeExit { status, stderr } => write!(
                f,
                "cad-bridge exited (status={status:?}): {}",
                stderr.trim()
            ),
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

fn is_eof(error: &FrameError) -> bool {
    match error {
        FrameError::Io(message) => {
            let lower = message.to_ascii_lowercase();
            lower.contains("unexpected eof")
                || lower.contains("unexpected end of file")
                || lower.contains("failed to fill whole buffer")
                || lower.contains("broken pipe")
        }
        _ => false,
    }
}

/// Write one length-prefixed JSON frame (`u32` LE length + UTF-8 JSON body).
pub fn write_frame<W: Write>(writer: &mut W, value: &Value) -> Result<(), FrameError> {
    let body = serde_json::to_vec(value).map_err(|error| FrameError::Json(error.to_string()))?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(FrameError::Oversize {
            side: "encode",
            len: body.len() as u32,
            max: MAX_RESPONSE_BYTES,
        });
    }
    let len = body.len() as u32;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Read one length-prefixed JSON frame, rejecting payloads above `max_bytes`.
pub fn read_frame<R: Read>(reader: &mut R, max_bytes: usize) -> Result<Value, FrameError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len as usize > max_bytes {
        return Err(FrameError::Oversize {
            side: "decode",
            len,
            max: max_bytes,
        });
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|error| FrameError::Json(error.to_string()))
}

fn require_schema(value: &Value) -> Result<(), FrameError> {
    match value.get("schema").and_then(Value::as_str) {
        Some(PROTOCOL_SCHEMA) => Ok(()),
        Some(other) => Err(FrameError::Protocol(format!("unsupported schema {other}"))),
        None => Err(FrameError::Protocol("missing schema".into())),
    }
}

fn require_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, FrameError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| FrameError::Protocol(format!("missing string field {key}")))
}

fn error_response(op: &str, request_id: &str, code: &str, message: impl Into<String>) -> Value {
    serde_json::json!({
        "schema": PROTOCOL_SCHEMA,
        "ok": false,
        "op": op,
        "request_id": request_id,
        "code": code,
        "message": message.into(),
    })
}

fn hello_ok(request_id: &str) -> Value {
    serde_json::json!({
        "schema": PROTOCOL_SCHEMA,
        "ok": true,
        "op": "hello",
        "request_id": request_id,
        "bridge_version": STUB_BRIDGE_VERSION,
        "occt_version": STUB_OCCT_VERSION,
    })
}

/// Fixed right-triangle fixture used by the stub tessellator (no STEP parsing).
pub fn stub_fixture_mesh() -> (Vec<f32>, Vec<u32>) {
    let positions = vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
    let indices = vec![0, 1, 2];
    (positions, indices)
}

fn encode_f32_le_b64(values: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(values.len() * 4);
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn encode_u32_le_b64(values: &[u32]) -> String {
    let mut bytes = Vec::with_capacity(values.len() * 4);
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn decode_f32_le_b64(encoded: &str) -> Result<Vec<f32>, FrameError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| FrameError::Protocol(format!("positions b64: {error}")))?;
    if bytes.len() % 4 != 0 {
        return Err(FrameError::Protocol(
            "positions b64 length is not a multiple of 4".into(),
        ));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("4 bytes")))
        .collect())
}

pub fn decode_u32_le_b64(encoded: &str) -> Result<Vec<u32>, FrameError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| FrameError::Protocol(format!("indices b64: {error}")))?;
    if bytes.len() % 4 != 0 {
        return Err(FrameError::Protocol(
            "indices b64 length is not a multiple of 4".into(),
        ));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("4 bytes")))
        .collect())
}

fn tessellation_param_sha256(
    path: &str,
    chord_tolerance: f64,
    max_triangles: u64,
    max_shells: u64,
    occurrence_path: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    hasher.update(b"\0");
    hasher.update(chord_tolerance.to_bits().to_le_bytes());
    hasher.update(max_triangles.to_le_bytes());
    hasher.update(max_shells.to_le_bytes());
    hasher.update(b"\0");
    hasher.update(occurrence_path.unwrap_or("").as_bytes());
    hasher.update(b"\0stub-fixture-v1");
    format!("{:x}", hasher.finalize())
}

fn identity_matrix_4x4() -> [f64; 16] {
    [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

fn stub_occurrence_transform(occurrence_path: &str) -> [f64; 16] {
    let mut transform = identity_matrix_4x4();
    if occurrence_path == STUB_OCCURRENCE_PATHS[1] {
        // Part-2 is translated so hosts can prove occurrence transforms are not dropped.
        transform[12] = 1.0;
        transform[13] = 2.0;
        transform[14] = 3.0;
    }
    transform
}

fn stub_occurrences(path: &str) -> Value {
    let path_lower = path.to_ascii_lowercase();
    if path_lower.contains(STUB_ASSEMBLY_MARKER) {
        let occurrences = STUB_OCCURRENCE_PATHS
            .iter()
            .map(|occurrence_path| {
                serde_json::json!({
                    "path": occurrence_path,
                    "name": occurrence_path.rsplit('/').next().unwrap_or(occurrence_path),
                    "transform_4x4": stub_occurrence_transform(occurrence_path),
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "assembly": true,
            "occurrences": occurrences,
        })
    } else {
        serde_json::json!({
            "assembly": false,
            "occurrences": [{
                "path": "/",
                "name": "part",
                "transform_4x4": identity_matrix_4x4(),
            }],
        })
    }
}

fn mesh_ok(
    request_id: &str,
    path: &str,
    chord_tolerance: f64,
    max_triangles: u64,
    max_shells: u64,
    occurrence_path: Option<&str>,
) -> Value {
    let (positions, indices) = stub_fixture_mesh();
    let triangle_count = indices.len() / 3;
    // One stable bridge face id per triangle so remap tests can exercise identity.
    let triangle_face_ids = (0..triangle_count)
        .map(|index| (index as u32) + 1)
        .collect::<Vec<_>>();
    let mut body = serde_json::json!({
        "schema": PROTOCOL_SCHEMA,
        "ok": true,
        "op": "tessellate_step",
        "request_id": request_id,
        "length_unit": "metre",
        "shell_count": 1,
        "triangle_count": triangle_count,
        "positions_f32le_b64": encode_f32_le_b64(&positions),
        "indices_u32le_b64": encode_u32_le_b64(&indices),
        "face_identity_kind": "bridge_face_id",
        "triangle_face_ids_u32le_b64": encode_u32_le_b64(&triangle_face_ids),
        "tessellation_param_sha256": tessellation_param_sha256(
            path,
            chord_tolerance,
            max_triangles,
            max_shells,
            occurrence_path,
        ),
        "warnings": [
            "stub bridge: fixed fixture mesh; OCCT tessellation not linked"
        ],
    });
    if let Some(occurrence) = occurrence_path {
        body["occurrence_path"] = Value::String(occurrence.to_owned());
        body["occurrence_transform_4x4"] = serde_json::json!(stub_occurrence_transform(occurrence));
    }
    body
}

fn is_cancelled(cancel_targets: &Mutex<HashSet<String>>, request_id: &str) -> bool {
    cancel_targets
        .lock()
        .map(|set| set.contains(request_id))
        .unwrap_or(false)
}

/// Handle one work request (not `cancel` — those are acked on the reader path).
pub fn handle_stub_work(
    request: &Value,
    cancel_targets: &Mutex<HashSet<String>>,
) -> Result<Value, FrameError> {
    require_schema(request)?;
    let op = require_str(request, "op")?;
    let request_id = require_str(request, "request_id")?;

    match op {
        "hello" => Ok(hello_ok(request_id)),
        "list_occurrences" => {
            let path = require_str(request, "path")?;
            let listing = stub_occurrences(path);
            Ok(serde_json::json!({
                "schema": PROTOCOL_SCHEMA,
                "ok": true,
                "op": "list_occurrences",
                "request_id": request_id,
                "assembly": listing["assembly"],
                "occurrences": listing["occurrences"],
            }))
        }
        "tessellate_step" => {
            let path = require_str(request, "path")?;
            let chord_tolerance = request
                .get("chord_tolerance")
                .and_then(Value::as_f64)
                .ok_or_else(|| FrameError::Protocol("missing chord_tolerance".into()))?;
            let max_triangles = request
                .get("max_triangles")
                .and_then(Value::as_u64)
                .ok_or_else(|| FrameError::Protocol("missing max_triangles".into()))?;
            let max_shells = request
                .get("max_shells")
                .and_then(Value::as_u64)
                .ok_or_else(|| FrameError::Protocol("missing max_shells".into()))?;
            let occurrence_path = request.get("occurrence_path").and_then(Value::as_str);

            let path_lower = path.to_ascii_lowercase();
            if path_lower.contains(STUB_ASSEMBLY_MARKER) && occurrence_path.is_none() {
                return Ok(error_response(
                    "tessellate_step",
                    request_id,
                    "occurrence_required",
                    "assemblies require an explicit occurrence_path (stub fail-closed)",
                ));
            }
            if let Some(occurrence) = occurrence_path {
                if path_lower.contains(STUB_ASSEMBLY_MARKER)
                    && !STUB_OCCURRENCE_PATHS.contains(&occurrence)
                {
                    return Ok(error_response(
                        "tessellate_step",
                        request_id,
                        "occurrence_not_found",
                        format!("occurrence_path {occurrence} was not found in the stub assembly"),
                    ));
                }
            }

            if path_lower.contains(STUB_SLOW_MARKER) {
                for _ in 0..40 {
                    if is_cancelled(cancel_targets, request_id) {
                        return Ok(error_response(
                            "tessellate_step",
                            request_id,
                            "cancelled",
                            "tessellation cancelled",
                        ));
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }

            if is_cancelled(cancel_targets, request_id) {
                return Ok(error_response(
                    "tessellate_step",
                    request_id,
                    "cancelled",
                    "tessellation cancelled",
                ));
            }

            Ok(mesh_ok(
                request_id,
                path,
                chord_tolerance,
                max_triangles,
                max_shells,
                occurrence_path,
            ))
        }
        other => Ok(error_response(
            other,
            request_id,
            "internal",
            format!("unknown op {other}"),
        )),
    }
}

/// In-process helper for unit tests that mirror cancel via an `AtomicBool`.
pub fn handle_stub_request(request: &Value, cancel: &AtomicBool) -> Result<Value, FrameError> {
    let targets = Mutex::new(HashSet::new());
    if request.get("op").and_then(Value::as_str) == Some("tessellate_step") {
        let path = request
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if path.contains(STUB_SLOW_MARKER) {
            let request_id = require_str(request, "request_id")?.to_owned();
            return thread::scope(|scope| {
                scope.spawn(|| {
                    for _ in 0..80 {
                        if cancel.load(Ordering::SeqCst) {
                            targets.lock().expect("lock").insert(request_id.clone());
                            return;
                        }
                        thread::sleep(Duration::from_millis(25));
                    }
                });
                handle_stub_work(request, &targets)
            });
        }
    }
    if cancel.load(Ordering::SeqCst) {
        if let Some(id) = request.get("request_id").and_then(Value::as_str) {
            targets.lock().expect("lock").insert(id.to_owned());
        }
    }
    handle_stub_work(request, &targets)
}

/// Stdio request loop for the `reyn-cad-bridge` stub binary.
///
/// A reader thread accepts frames continuously so `cancel` can interrupt an
/// in-flight slow `tessellate_step`. Cancel acks are written immediately;
/// work responses are written by the main loop.
pub fn run_stub_stdio() -> Result<(), FrameError> {
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let cancel_targets = Arc::new(Mutex::new(HashSet::<String>::new()));
    let (work_tx, work_rx) = mpsc::channel::<Value>();

    let stdout_reader = Arc::clone(&stdout);
    let cancel_reader = Arc::clone(&cancel_targets);
    let reader = thread::spawn(move || -> Result<(), FrameError> {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        loop {
            let request = match read_frame(&mut input, MAX_REQUEST_BYTES) {
                Ok(value) => value,
                Err(error) if is_eof(&error) => return Ok(()),
                Err(error @ FrameError::Oversize { .. }) => {
                    let response =
                        error_response("unknown", "unparsed", "memory_limit", error.to_string());
                    let mut out = stdout_reader.lock().expect("stdout lock");
                    let _ = write_frame(&mut *out, &response);
                    return Err(error);
                }
                Err(error) => return Err(error),
            };

            if request.get("op").and_then(Value::as_str) == Some("cancel") {
                require_schema(&request)?;
                let request_id = require_str(&request, "request_id")?;
                let target = require_str(&request, "target_request_id")?;
                cancel_reader
                    .lock()
                    .expect("cancel lock")
                    .insert(target.to_owned());
                let response = serde_json::json!({
                    "schema": PROTOCOL_SCHEMA,
                    "ok": true,
                    "op": "cancel",
                    "request_id": request_id,
                    "target_request_id": target,
                    "accepted": true,
                });
                let mut out = stdout_reader.lock().expect("stdout lock");
                write_frame(&mut *out, &response)?;
                continue;
            }

            if work_tx.send(request).is_err() {
                return Ok(());
            }
        }
    });

    while let Ok(request) = work_rx.recv() {
        let response = handle_stub_work(&request, &cancel_targets)?;
        let mut out = stdout.lock().expect("stdout lock");
        write_frame(&mut *out, &response)?;
    }

    match reader.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(FrameError::Io("bridge reader thread panicked".into())),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HelloOk {
    pub request_id: String,
    pub bridge_version: String,
    pub occt_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MeshOk {
    pub request_id: String,
    pub length_unit: String,
    pub shell_count: u64,
    pub triangle_count: u64,
    pub positions: Vec<f32>,
    pub indices: Vec<u32>,
    pub tessellation_param_sha256: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeError {
    pub request_id: String,
    pub op: String,
    pub code: String,
    pub message: String,
}

/// Host-side client that owns a `reyn-cad-bridge` child process.
pub struct CadBridgeClient {
    child: Arc<Mutex<Child>>,
    stdin: Option<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl CadBridgeClient {
    pub fn spawn(bridge_bin: impl AsRef<Path>) -> Result<Self, FrameError> {
        let mut child = Command::new(bridge_bin.as_ref())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| FrameError::Io(error.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| FrameError::Io("bridge stdin missing".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| FrameError::Io("bridge stdout missing".into()))?;
        Ok(Self {
            child: Arc::new(Mutex::new(child)),
            stdin: Some(stdin),
            stdout: std::io::BufReader::new(stdout),
        })
    }

    fn stdin_mut(&mut self) -> Result<&mut std::process::ChildStdin, FrameError> {
        self.stdin
            .as_mut()
            .ok_or_else(|| FrameError::Io("bridge stdin already closed".into()))
    }

    pub fn spawn_default() -> Result<Self, FrameError> {
        let path = default_bridge_bin()
            .ok_or_else(|| FrameError::Io("reyn-cad-bridge binary path unknown".into()))?;
        Self::spawn(path)
    }

    fn exchange(&mut self, request: &Value, timeout: Duration) -> Result<Value, FrameError> {
        write_frame(self.stdin_mut()?, request)?;
        self.read_response(timeout)
    }

    fn read_response(&mut self, timeout: Duration) -> Result<Value, FrameError> {
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let child = Arc::clone(&self.child);
        let watchdog = thread::spawn(move || match done_rx.recv_timeout(timeout) {
            Ok(()) => false,
            Err(_) => {
                if let Ok(mut child) = child.lock() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                true
            }
        });

        let result = read_frame(&mut self.stdout, MAX_RESPONSE_BYTES);
        let _ = done_tx.send(());
        let timed_out = watchdog.join().unwrap_or(false);
        if timed_out {
            return Err(FrameError::Timeout);
        }
        result
    }

    pub fn hello(&mut self, request_id: &str) -> Result<HelloOk, FrameError> {
        let request = serde_json::json!({
            "schema": PROTOCOL_SCHEMA,
            "op": "hello",
            "request_id": request_id,
        });
        let response = self.exchange(&request, DEFAULT_TIMEOUT)?;
        parse_hello_ok(&response)
    }

    pub fn tessellate_step(
        &mut self,
        request_id: &str,
        path: &str,
        chord_tolerance: f64,
        max_triangles: u64,
        max_shells: u64,
        occurrence_path: Option<&str>,
        timeout: Duration,
    ) -> Result<Result<MeshOk, BridgeError>, FrameError> {
        let mut request = serde_json::json!({
            "schema": PROTOCOL_SCHEMA,
            "op": "tessellate_step",
            "request_id": request_id,
            "path": path,
            "chord_tolerance": chord_tolerance,
            "max_triangles": max_triangles,
            "max_shells": max_shells,
        });
        if let Some(occurrence) = occurrence_path {
            request["occurrence_path"] = Value::String(occurrence.to_owned());
        }
        let response = self.exchange(&request, timeout)?;
        parse_tessellate_response(&response)
    }

    pub fn cancel(&mut self, request_id: &str, target_request_id: &str) -> Result<(), FrameError> {
        let request = serde_json::json!({
            "schema": PROTOCOL_SCHEMA,
            "op": "cancel",
            "request_id": request_id,
            "target_request_id": target_request_id,
        });
        write_frame(self.stdin_mut()?, &request)?;
        // Cancel ack and cancelled tessellate may arrive in either order.
        for _ in 0..3 {
            let response = self.read_response(Duration::from_secs(5))?;
            require_schema(&response)?;
            if response.get("op").and_then(Value::as_str) == Some("cancel")
                && response.get("ok").and_then(Value::as_bool) == Some(true)
            {
                return Ok(());
            }
            if response.get("op").and_then(Value::as_str) == Some("tessellate_step")
                && response.get("code").and_then(Value::as_str) == Some("cancelled")
            {
                continue;
            }
            return Err(FrameError::Protocol(format!(
                "unexpected frame while waiting for cancel ack: {response}"
            )));
        }
        Err(FrameError::Protocol(
            "cancel ack not received after tessellate frames".into(),
        ))
    }

    pub fn write_raw(&mut self, request: &Value) -> Result<(), FrameError> {
        write_frame(self.stdin_mut()?, request)
    }

    pub fn read_raw(&mut self, timeout: Duration) -> Result<Value, FrameError> {
        self.read_response(timeout)
    }

    pub fn shutdown(mut self) -> Result<(), FrameError> {
        self.stdin.take();
        let mut child = self
            .child
            .lock()
            .map_err(|_| FrameError::Io("child lock poisoned".into()))?;
        match child.wait() {
            Ok(status) if status.success() || status.code() == Some(0) => Ok(()),
            Ok(status) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                Err(FrameError::BridgeExit {
                    status: status.code(),
                    stderr,
                })
            }
            Err(error) => Err(FrameError::Io(error.to_string())),
        }
    }
}

impl Drop for CadBridgeClient {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
    }
}

pub fn default_bridge_bin() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_reyn-cad-bridge") {
        return Some(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("REYN_CAD_BRIDGE") {
        return Some(PathBuf::from(path));
    }
    None
}

fn parse_hello_ok(response: &Value) -> Result<HelloOk, FrameError> {
    require_schema(response)?;
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(FrameError::Protocol(format!("hello failed: {response}")));
    }
    Ok(HelloOk {
        request_id: require_str(response, "request_id")?.to_owned(),
        bridge_version: require_str(response, "bridge_version")?.to_owned(),
        occt_version: require_str(response, "occt_version")?.to_owned(),
    })
}

fn parse_tessellate_response(response: &Value) -> Result<Result<MeshOk, BridgeError>, FrameError> {
    require_schema(response)?;
    let request_id = require_str(response, "request_id")?.to_owned();
    let op = require_str(response, "op")?.to_owned();
    if response.get("ok").and_then(Value::as_bool) == Some(false) {
        return Ok(Err(BridgeError {
            request_id,
            op,
            code: require_str(response, "code")?.to_owned(),
            message: require_str(response, "message")?.to_owned(),
        }));
    }
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(FrameError::Protocol(
            "tessellate response missing ok".into(),
        ));
    }
    let positions = decode_f32_le_b64(require_str(response, "positions_f32le_b64")?)?;
    let indices = decode_u32_le_b64(require_str(response, "indices_u32le_b64")?)?;
    let warnings = response
        .get("warnings")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok(Ok(MeshOk {
        request_id,
        length_unit: require_str(response, "length_unit")?.to_owned(),
        shell_count: response
            .get("shell_count")
            .and_then(Value::as_u64)
            .ok_or_else(|| FrameError::Protocol("missing shell_count".into()))?,
        triangle_count: response
            .get("triangle_count")
            .and_then(Value::as_u64)
            .ok_or_else(|| FrameError::Protocol("missing triangle_count".into()))?,
        positions,
        indices,
        tessellation_param_sha256: require_str(response, "tessellation_param_sha256")?.to_owned(),
        warnings,
    }))
}

/// In-process stub roundtrip (no child process).
pub fn in_process_stub_roundtrip(request: &Value) -> Result<Value, FrameError> {
    let cancel = AtomicBool::new(false);
    handle_stub_request(request, &cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::time::Instant;

    #[test]
    fn protocol_constants_match_frozen_json_doc() {
        let doc = include_str!("../docs/occt_bridge_protocol.v1.json");
        let value: Value = serde_json::from_str(doc).expect("protocol json");
        assert_eq!(value["schema"], PROTOCOL_SCHEMA);
        assert_eq!(value["max_request_bytes"], MAX_REQUEST_BYTES as u64);
        assert_eq!(value["max_response_bytes"], MAX_RESPONSE_BYTES as u64);
        assert_eq!(value["byte_order"], "little_endian_u32_payload_length");
        assert_eq!(value["transport"], "length_prefixed_json_stdio");
        assert_eq!(value["requests"]["hello"]["op"], "hello");
        assert_eq!(
            value["requests"]["tessellate_step"]["op"],
            "tessellate_step"
        );
        assert_eq!(value["requests"]["cancel"]["op"], "cancel");
    }

    #[test]
    fn frame_roundtrip_preserves_json() {
        let payload =
            serde_json::json!({"schema": PROTOCOL_SCHEMA, "op": "hello", "request_id": "r1"});
        let mut buf = Vec::new();
        write_frame(&mut buf, &payload).unwrap();
        let expected_len = serde_json::to_vec(&payload).unwrap().len() as u32;
        assert_eq!(&buf[..4], &expected_len.to_le_bytes());
        let decoded = read_frame(&mut Cursor::new(buf), MAX_REQUEST_BYTES).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn oversize_length_prefix_fails_closed() {
        let mut buf = Vec::new();
        let too_big = (MAX_REQUEST_BYTES as u32).saturating_add(1);
        buf.extend_from_slice(&too_big.to_le_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        let err = read_frame(&mut Cursor::new(buf), MAX_REQUEST_BYTES).unwrap_err();
        match err {
            FrameError::Oversize { len, max, .. } => {
                assert_eq!(len, too_big);
                assert_eq!(max, MAX_REQUEST_BYTES);
            }
            other => panic!("expected oversize, got {other}"),
        }
    }

    #[test]
    fn stub_hello_and_fixture_mesh() {
        let hello = in_process_stub_roundtrip(&serde_json::json!({
            "schema": PROTOCOL_SCHEMA,
            "op": "hello",
            "request_id": "h1",
        }))
        .unwrap();
        assert_eq!(hello["bridge_version"], STUB_BRIDGE_VERSION);
        assert_eq!(hello["occt_version"], STUB_OCCT_VERSION);

        let mesh = in_process_stub_roundtrip(&serde_json::json!({
            "schema": PROTOCOL_SCHEMA,
            "op": "tessellate_step",
            "request_id": "t1",
            "path": "/tmp/part.step",
            "chord_tolerance": 0.001,
            "max_triangles": 10_000,
            "max_shells": 16,
        }))
        .unwrap();
        assert_eq!(mesh["ok"], true);
        assert_eq!(mesh["triangle_count"], 1);
        assert_eq!(mesh["length_unit"], "metre");
        let positions = decode_f32_le_b64(mesh["positions_f32le_b64"].as_str().unwrap()).unwrap();
        let indices = decode_u32_le_b64(mesh["indices_u32le_b64"].as_str().unwrap()).unwrap();
        assert_eq!(positions, stub_fixture_mesh().0);
        assert_eq!(indices, stub_fixture_mesh().1);
    }

    #[test]
    fn stub_rejects_assembly_without_occurrence() {
        let response = in_process_stub_roundtrip(&serde_json::json!({
            "schema": PROTOCOL_SCHEMA,
            "op": "tessellate_step",
            "request_id": "a1",
            "path": "/tmp/widget_assembly.step",
            "chord_tolerance": 0.001,
            "max_triangles": 10_000,
            "max_shells": 16,
        }))
        .unwrap();
        assert_eq!(response["ok"], false);
        assert_eq!(response["code"], "occurrence_required");
    }

    #[test]
    fn stub_lists_assembly_occurrences_and_returns_transform() {
        let listing = in_process_stub_roundtrip(&serde_json::json!({
            "schema": PROTOCOL_SCHEMA,
            "op": "list_occurrences",
            "request_id": "l1",
            "path": "/tmp/widget_assembly.step",
        }))
        .unwrap();
        assert_eq!(listing["ok"], true);
        assert_eq!(listing["assembly"], true);
        assert_eq!(listing["occurrences"].as_array().unwrap().len(), 2);

        let mesh = in_process_stub_roundtrip(&serde_json::json!({
            "schema": PROTOCOL_SCHEMA,
            "op": "tessellate_step",
            "request_id": "a2",
            "path": "/tmp/widget_assembly.step",
            "chord_tolerance": 0.001,
            "max_triangles": 10_000,
            "max_shells": 16,
            "occurrence_path": STUB_OCCURRENCE_PATHS[1],
        }))
        .unwrap();
        assert_eq!(mesh["ok"], true);
        assert_eq!(mesh["occurrence_path"], STUB_OCCURRENCE_PATHS[1]);
        assert_eq!(mesh["face_identity_kind"], "bridge_face_id");
        let transform = mesh["occurrence_transform_4x4"]
            .as_array()
            .expect("transform");
        assert_eq!(transform[12], 1.0);
        assert_eq!(transform[13], 2.0);
        assert_eq!(transform[14], 3.0);
        let face_ids =
            decode_u32_le_b64(mesh["triangle_face_ids_u32le_b64"].as_str().unwrap()).unwrap();
        assert_eq!(face_ids, vec![1]);
    }

    #[test]
    fn stub_rejects_unknown_occurrence_path() {
        let response = in_process_stub_roundtrip(&serde_json::json!({
            "schema": PROTOCOL_SCHEMA,
            "op": "tessellate_step",
            "request_id": "a3",
            "path": "/tmp/widget_assembly.step",
            "chord_tolerance": 0.001,
            "max_triangles": 10_000,
            "max_shells": 16,
            "occurrence_path": "/Product/Missing",
        }))
        .unwrap();
        assert_eq!(response["ok"], false);
        assert_eq!(response["code"], "occurrence_not_found");
    }

    #[test]
    fn stub_unknown_op_fails_closed() {
        let response = in_process_stub_roundtrip(&serde_json::json!({
            "schema": PROTOCOL_SCHEMA,
            "op": "heal_magic",
            "request_id": "u1",
        }))
        .unwrap();
        assert_eq!(response["ok"], false);
        assert_eq!(response["code"], "internal");
    }

    #[test]
    fn stub_cancel_flag_interrupts_slow_tessellate() {
        let cancel = AtomicBool::new(false);
        let request = serde_json::json!({
            "schema": PROTOCOL_SCHEMA,
            "op": "tessellate_step",
            "request_id": "slow",
            "path": format!("/tmp/{STUB_SLOW_MARKER}.step"),
            "chord_tolerance": 0.001,
            "max_triangles": 10_000,
            "max_shells": 16,
        });
        let started = Instant::now();
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(80));
                cancel.store(true, Ordering::SeqCst);
            });
            let response = handle_stub_request(&request, &cancel).unwrap();
            assert_eq!(response["ok"], false);
            assert_eq!(response["code"], "cancelled");
        });
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancel should interrupt before the full slow sleep"
        );
    }
}

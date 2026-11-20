//! HTTP transport for MCP Streamable HTTP, in two eras on one endpoint: the
//! stateless revision 2026-07-28, and 2025-06-18 with `initialize` and sessions.
//!
//! Owns listener lifecycle and every transport-level decision — status codes,
//! headers, Origin validation, bearer auth, protocol-version negotiation, and
//! session routing. JSON-RPC method handling lives in `mcp`.

use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value as Json;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::config::{self, RequestConfig};
use crate::executor::Loopback;
use crate::{mcp, status};

/// Largest request body accepted, so a client cannot stream an unbounded body
/// into memory.
const MAX_BODY_BYTES: u64 = 1 << 20;

/// How long the worker will wait for a request body before abandoning the read.
/// tiny_http hands over a request with headers parsed but the body unread, and
/// that read happens on the worker thread; on loopback a 1 MiB body arrives in
/// milliseconds, so this only ever fires on a client that has stopped sending.
const BODY_READ_TIMEOUT_S: u64 = 30;

/// The listener while the server is enabled. Taken and dropped on stop.
static SERVER: Mutex<Option<Server>> = Mutex::new(None);

/// Bind the HTTP listener on 127.0.0.1. Any previous listener is dropped first.
///
/// Returns whether the listener bound. A false return means nothing is serving
/// — the caller reflects that in `enabled` rather than reporting a server that
/// isn't there. TLS belongs in a reverse proxy in front of it.
pub fn start(port: i64) -> bool {
    stop();
    // port 0 asks the OS to assign one; a negative port cannot occur (the sys
    // var min is 0), so an unconditional bind is correct.
    match Server::http(format!("127.0.0.1:{port}")) {
        Ok(s) => {
            status::set_http_port(bound_port(&s));
            *SERVER.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
            true
        }
        Err(e) => {
            log(&format!("failed to bind HTTP port {port}: {e}"));
            false
        }
    }
}

/// The port a bound listener actually got. With `port = 0` the OS assigns one,
/// so this is the only way to learn where the server is listening.
fn bound_port(server: &Server) -> i64 {
    server
        .server_addr()
        .to_ip()
        .map_or(0, |addr| i64::from(addr.port()))
}

/// Drop the listener and forget every session.
pub fn stop() {
    SERVER.lock().unwrap_or_else(|e| e.into_inner()).take();
    status::set_http_port(0);
    mcp::clear_sessions();
}

/// Drain and handle every pending request. Called from the
/// worker's periodic wakeup, so it must never block or panic out.
///
/// Requests are collected under the SERVER lock but handled after it is
/// released. Disable runs `stop()` on this same worker thread after the poll
/// loop returns, and the server's `worker_stop()` blocks the thread that ran
/// `SET GLOBAL ... = OFF` in `thread.join()` until it does — so anything that
/// blocks here (a long tool call, or a stalled body read) delays disable and
/// shutdown by exactly that long. The body read is bounded for this reason (see
/// `read_body`); holding the lock only to collect keeps it off the critical path.
pub fn poll() {
    let requests: Vec<Request> = {
        let guard = SERVER.lock().unwrap_or_else(|e| e.into_inner());
        let Some(server) = guard.as_ref() else {
            return;
        };
        let mut pending = Vec::new();
        while let Ok(Some(request)) = server.try_recv() {
            pending.push(request);
        }
        pending
    };
    for request in requests {
        // A panic while handling one request must not take down the worker.
        let _ = catch_unwind(AssertUnwindSafe(|| handle_request(request)));
    }
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    // `HeaderField::equiv` only accepts a &'static str, so compare by hand to
    // allow a runtime header name.
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn ctype_json() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("static header is valid")
}

fn respond_json(request: Request, status: u16, body: &Json) {
    let response = Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(ctype_json());
    let _ = request.respond(response);
}

fn respond_empty(request: Request, status: u16) {
    let _ = request.respond(Response::empty(status));
}

/// True unless the Origin header names a non-local host. Absent Origin is
/// allowed (non-browser MCP clients omit it); present-but-remote is rejected to
/// block DNS-rebinding, as the transport spec requires.
fn origin_ok(req: &Request) -> bool {
    match header(req, "Origin") {
        None => true,
        Some(origin) => {
            // authority = everything after scheme, before any path.
            let authority = origin
                .split_once("://")
                .map_or(origin, |(_, rest)| rest)
                .split('/')
                .next()
                .unwrap_or("");
            // Drop any `userinfo@` prefix: `http://127.0.0.1:9999@evil.com` has
            // authority `127.0.0.1:9999@evil.com`, whose real host is `evil.com`.
            // A browser never sends this (RFC 6454 Origin has no userinfo), but a
            // `:port` split on the raw authority would read the host as local.
            let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            // Strip a bracketed IPv6 host first (`[::1]` or `[::1]:port`), then
            // a trailing `:port` on a bare host. Without pulling the brackets
            // off first, the `:port` split would cut inside the address.
            let host = if let Some(rest) = authority.strip_prefix('[') {
                rest.split(']').next().unwrap_or("")
            } else {
                authority.rsplit_once(':').map_or(authority, |(h, _)| h)
            };
            host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
        }
    }
}

/// Compare two secrets without returning early on the first differing byte.
/// The lengths are still distinguishable, which is inherent to comparing
/// without hashing and is not what an attacker is fishing for here.
fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn auth_ok(req: &Request, require_auth: bool, token: &str) -> bool {
    if !require_auth {
        return true;
    }
    // Requiring auth against an unset token would let `Authorization: Bearer `
    // through, which is the opposite of what the setting asks for.
    if token.is_empty() {
        return false;
    }
    let Some(value) = header(req, "Authorization") else {
        return false;
    };
    let Some((scheme, credential)) = value.split_once(' ') else {
        return false;
    };
    // RFC 7235 defines the scheme as case-insensitive; the credential is not.
    scheme.eq_ignore_ascii_case("Bearer") && secret_eq(credential, token)
}

/// The `MCP-Protocol-Version` header when it names a version this server does
/// not speak. An absent header means 2025-03-26 (spec), which it does.
fn unsupported_protocol(req: &Request) -> Option<String> {
    header(req, "MCP-Protocol-Version")
        .filter(|v| !mcp::SUPPORTED_VERSIONS.contains(v))
        .map(str::to_owned)
}

/// Undo the `=?base64?...?=` form a client must use for an `Mcp-Name` value
/// that is not plain ASCII. None when the encoded form does not decode.
fn decode_header_value(value: &str) -> Option<String> {
    use base64::Engine as _;
    match value.strip_prefix("=?base64?").and_then(|v| v.strip_suffix("?=")) {
        Some(encoded) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok()),
        None => Some(value.to_owned()),
    }
}

fn handle_request(request: Request) {
    let path_ok = matches!(
        request.url().split('?').next(),
        Some("/mcp") | Some("/mcp/") | Some("/")
    );
    if !path_ok {
        respond_empty(request, 404);
        return;
    }

    // Origin first: it defends every method.
    if !origin_ok(&request) {
        respond_empty(request, 403);
        return;
    }

    match request.method() {
        Method::Get => {
            // No SSE stream offered at this endpoint.
            respond_empty(request, 405);
        }
        Method::Delete => {
            // DELETE needs only the auth settings, not the full request config.
            let (require_auth, token) = config::auth_settings();
            if !auth_ok(&request, require_auth, &token) {
                respond_empty(request, 401);
                return;
            }
            match header(&request, "Mcp-Session-Id") {
                Some(id) if mcp::terminate_session(id) => respond_empty(request, 200),
                Some(_) => respond_empty(request, 404),
                None => respond_empty(request, 400),
            }
        }
        Method::Post => handle_post(request),
        _ => respond_empty(request, 405),
    }
}

fn handle_post(request: Request) {
    let cfg = RequestConfig::read();
    if !auth_ok(&request, cfg.require_auth, &cfg.bearer_token) {
        respond_empty(request, 401);
        return;
    }
    if let Some(version) = unsupported_protocol(&request) {
        respond_json(request, 400, &mcp::unsupported_version(&Json::Null, &version));
        return;
    }

    // Read the (bounded) body without letting a slow client wedge the worker.
    let (request, body) = match read_body(request) {
        BodyRead::Ok(request, body) => (request, body),
        BodyRead::ReadError(request) => {
            respond_json(request, 400, &mcp::error(&Json::Null, -32700, "could not read request body"));
            return;
        }
        // The reader stalled and was abandoned to its helper thread, which owns
        // the request and drops it (closing the socket) once the read finally
        // ends. There is nothing to respond on here; the worker moves on.
        BodyRead::TimedOut => return,
    };
    if body.len() as u64 > MAX_BODY_BYTES {
        respond_empty(request, 413);
        return;
    }
    let msg: Json = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            respond_json(request, 400, &mcp::error(&Json::Null, -32700, &format!("parse error: {e}")));
            return;
        }
    };

    // Revision 2025-06-18 removed JSON-RPC batching. An array body would
    // otherwise fall through to the notification path below — no top-level id,
    // so 202 and no body — leaving a batching client waiting for responses that
    // are never coming.
    if msg.is_array() {
        respond_json(
            request,
            400,
            &mcp::error(
                &Json::Null,
                -32600,
                "JSON-RPC batching was removed in MCP 2025-06-18; send one request per POST",
            ),
        );
        return;
    }
    if !msg.is_object() {
        respond_json(
            request,
            400,
            &mcp::error(&Json::Null, -32600, "request must be a JSON object"),
        );
        return;
    }
    if msg.get("jsonrpc").and_then(Json::as_str) != Some(mcp::JSONRPC_VERSION) {
        let id = msg.get("id").cloned().unwrap_or(Json::Null);
        respond_json(
            request,
            400,
            &mcp::error(&id, -32600, "jsonrpc must be \"2.0\""),
        );
        return;
    }

    let method = msg.get("method").and_then(Json::as_str);
    let id = msg.get("id").cloned();
    let params = msg.get("params").unwrap_or(&Json::Null);

    // A message with no id is a notification or response: acknowledge with 202.
    let Some(id) = id else {
        respond_empty(request, 202);
        return;
    };
    let Some(method) = method else {
        respond_json(request, 400, &mcp::error(&id, -32600, "not a request"));
        return;
    };

    // A stateless client names its version in `_meta`; `initialize` always
    // selects the session era, as the spec requires of a server that speaks both.
    let meta = params.get("_meta");
    let stateless = method != "initialize"
        && (meta.and_then(|m| m.get(mcp::META_PROTOCOL_VERSION)).is_some()
            || header(&request, "MCP-Protocol-Version") == Some(mcp::STATELESS_VERSION));
    if stateless {
        handle_stateless(request, method, params, meta, &id, &cfg);
        return;
    }

    if method == "initialize" {
        let session = mcp::new_session();
        let session_header = Header::from_bytes(&b"Mcp-Session-Id"[..], session.as_bytes())
            .expect("session id is ASCII hex");
        let response = Response::from_string(mcp::initialize_result(&id).to_string())
            .with_status_code(200)
            .with_header(ctype_json())
            .with_header(session_header);
        let _ = request.respond(response);
        return;
    }

    // Every other request must carry a live session id.
    match header(&request, "Mcp-Session-Id") {
        None => {
            respond_json(request, 400, &mcp::error(&id, -32600, "missing Mcp-Session-Id header"));
            return;
        }
        Some(sid) if !mcp::session_exists(sid) => {
            respond_empty(request, 404);
            return;
        }
        Some(_) => {}
    }

    let exec = Loopback::new(&cfg.db_url, &cfg.schema, !cfg.allowed_tables.is_empty());
    let response = mcp::dispatch(method, params, &id, &cfg, &exec);
    respond_json(request, 200, &response);
}

/// Serve one request of the stateless revision. Each request stands alone: an
/// `Mcp-Session-Id` header is ignored, and none is issued.
///
/// The routing headers must agree with the body, so that a proxy routing or
/// rate-limiting on the header cannot be steered by a body that says otherwise.
fn handle_stateless(request: Request, method: &str, params: &Json, meta: Option<&Json>, id: &Json, cfg: &RequestConfig) {
    let version = meta.and_then(|m| m.get(mcp::META_PROTOCOL_VERSION)).and_then(Json::as_str);
    let has_caps = meta
        .and_then(|m| m.get(mcp::META_CLIENT_CAPABILITIES))
        .is_some_and(Json::is_object);
    let (Some(version), true) = (version, has_caps) else {
        let msg = format!(
            "params._meta must carry {} and {}",
            mcp::META_PROTOCOL_VERSION,
            mcp::META_CLIENT_CAPABILITIES
        );
        respond_json(request, 400, &mcp::error(id, -32602, &msg));
        return;
    };
    if version != mcp::STATELESS_VERSION {
        respond_json(request, 400, &mcp::unsupported_version(id, version));
        return;
    }

    // Mcp-Name mirrors the tool name or resource URI, and only on the methods
    // that have one. When the body has no value the header must be absent too.
    let body_name = match method {
        "tools/call" => params.get("name"),
        "resources/read" => params.get("uri"),
        _ => None,
    };
    let name_ok = match (header(&request, "Mcp-Name"), body_name.and_then(Json::as_str)) {
        (None, None) => true,
        (Some(h), Some(body)) => decode_header_value(h).as_deref() == Some(body),
        _ => false,
    };
    let mismatch = if header(&request, "MCP-Protocol-Version") != Some(version) {
        Some("MCP-Protocol-Version header does not match params._meta")
    } else if header(&request, "Mcp-Method") != Some(method) {
        Some("Mcp-Method header does not match the request method")
    } else if !name_ok {
        Some("Mcp-Name header does not match the request body")
    } else {
        None
    };
    if let Some(msg) = mismatch {
        respond_json(request, 400, &mcp::error(id, -32020, &format!("Header mismatch: {msg}")));
        return;
    }

    let exec = Loopback::new(&cfg.db_url, &cfg.schema, !cfg.allowed_tables.is_empty());
    let response = mcp::dispatch_stateless(method, params, id, cfg, &exec);
    // This revision answers an unknown method with 404, not 200.
    let status = if response["error"]["code"] == -32601 { 404 } else { 200 };
    respond_json(request, status, &response);
}

/// The outcome of a bounded body read.
enum BodyRead {
    /// The body was read; the request is returned to respond on.
    Ok(Request, String),
    /// The read failed (client hung up mid-body, encoding error); the request
    /// is returned so a 400 can still be sent.
    ReadError(Request),
    /// The read did not finish within `BODY_READ_TIMEOUT_S`. The request has
    /// been left with the helper thread and cannot be responded on here.
    TimedOut,
}

/// Read the request body on a helper thread so a client that sends a short body
/// and then stalls cannot block the worker's poll loop — which would in turn
/// hang disable and shutdown (see `poll`). The read is bounded to `MAX_BODY_BYTES
/// + 1` for the size check, and the worker waits at most `BODY_READ_TIMEOUT_S`.
fn read_body(request: Request) -> BodyRead {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut request = request;
        let mut body = String::new();
        let ok = request
            .as_reader()
            .take(MAX_BODY_BYTES + 1)
            .read_to_string(&mut body)
            .is_ok();
        // The receiver is gone on timeout; the send simply fails and the request
        // drops here, closing the socket.
        let _ = tx.send((request, body, ok));
    });
    match rx.recv_timeout(Duration::from_secs(BODY_READ_TIMEOUT_S)) {
        Ok((request, body, true)) => BodyRead::Ok(request, body),
        Ok((request, _, false)) => BodyRead::ReadError(request),
        Err(_) => BodyRead::TimedOut,
    }
}

pub(crate) fn log(msg: &str) {
    // Not `eprintln!`, which panics when stderr is closed, as it is on a
    // daemonized server whose error log is stderr.
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "[vsql_mcp] {msg}");
}

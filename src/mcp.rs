//! MCP JSON-RPC layer: protocol constants, session store, and method dispatch.
//!
//! Transport concerns (status codes, headers, auth, Origin) live in `httpd`.
//! This module owns what a valid, authenticated JSON-RPC request means once the
//! body is parsed.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use serde_json::{json, Value as Json};

use crate::config::RequestConfig;
use crate::executor::Loopback;
use crate::{resources, status, tools};

/// Protocol version this server advertises in an `initialize` response.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The stateless revision. A request in it carries its protocol version and
/// client capabilities in `params._meta`, and there is no session.
pub const STATELESS_VERSION: &str = "2026-07-28";

/// The only JSON-RPC version MCP uses. Required on every request.
pub const JSONRPC_VERSION: &str = "2.0";

/// Every version this server speaks, newest first. `2025-03-26` is the value the
/// spec says to assume when the header is absent, so we accept it too.
pub const SUPPORTED_VERSIONS: &[&str] = &[STATELESS_VERSION, PROTOCOL_VERSION, "2025-03-26"];

pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// Upper bound on live sessions, so a client that keeps calling `initialize`
/// without ever deleting cannot grow the map without limit before the TTL
/// evicts. Past the cap the oldest session is dropped to make room.
const MAX_SESSIONS: usize = 4096;

/// Each session is the id and the last time it was seen. The instant is
/// refreshed on use so the TTL is genuinely idle-based (see `session_exists`).
static SESSIONS: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// One process-lifetime seed. Hashing the monotonic counter through it gives
/// ids that are unpredictable to outsiders and distinct per counter value.
static ID_SEED: LazyLock<RandomState> = LazyLock::new(RandomState::new);

fn lock_sessions() -> std::sync::MutexGuard<'static, HashMap<String, Instant>> {
    SESSIONS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Mint a new session id and record it, evicting any expired sessions first.
pub fn new_session() -> String {
    let n = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let a = seeded_hash(n);
    let b = seeded_hash(n ^ 0x9e37_79b9_7f4a_7c15);
    let id = format!("{a:016x}{b:016x}");
    let ttl = crate::config::session_ttl();
    let mut map = lock_sessions();
    map.retain(|_, seen| seen.elapsed() < ttl);
    // If eviction did not get the map back under the cap (every session still
    // live), drop the least-recently-seen one so an initialize flood is bounded.
    if map.len() >= MAX_SESSIONS {
        if let Some(oldest) = map.iter().min_by_key(|(_, seen)| **seen).map(|(k, _)| k.clone()) {
            map.remove(&oldest);
        }
    }
    map.insert(id.clone(), Instant::now());
    status::set_sessions_active(map.len() as i64);
    id
}

/// Whether a session is live, refreshing its last-seen time when it is. The TTL
/// is idle-based — a session in active use never expires under a client — so the
/// check is also the touch that keeps it alive.
pub fn session_exists(id: &str) -> bool {
    let ttl = crate::config::session_ttl();
    let mut map = lock_sessions();
    match map.get_mut(id) {
        Some(seen) if seen.elapsed() < ttl => {
            *seen = Instant::now();
            true
        }
        _ => false,
    }
}

/// Remove a session. Returns true if it existed.
pub fn terminate_session(id: &str) -> bool {
    let mut map = lock_sessions();
    let existed = map.remove(id).is_some();
    status::set_sessions_active(map.len() as i64);
    existed
}

/// Clear every session (called when the worker stops).
pub fn clear_sessions() {
    let mut map = lock_sessions();
    map.clear();
    status::set_sessions_active(0);
}

fn seeded_hash(input: u64) -> u64 {
    let mut h = ID_SEED.build_hasher();
    h.write_u64(input);
    h.finish()
}

pub fn error(id: &Json, code: i64, message: &str) -> Json {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": { "code": code, "message": message }
    })
}

pub fn result(id: &Json, value: Json) -> Json {
    json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "result": value })
}

/// The `-32022` error a stateless client reads to pick a version and retry.
pub fn unsupported_version(id: &Json, requested: &str) -> Json {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {
            "code": -32022,
            "message": "Unsupported protocol version",
            "data": { "supported": SUPPORTED_VERSIONS, "requested": requested }
        }
    })
}

fn capabilities() -> Json {
    json!({ "tools": {}, "resources": {} })
}

fn server_info() -> Json {
    json!({ "name": "vsql_mcp", "version": env!("CARGO_PKG_VERSION") })
}

/// The `initialize` result: advertised protocol version, capabilities, and
/// server identity.
pub fn initialize_result(id: &Json) -> Json {
    result(
        id,
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": capabilities(),
            "serverInfo": server_info()
        }),
    )
}

/// Route a request in the stateless revision and add the result fields it
/// requires. `ttlMs` is 0 and the scope private because every list and read
/// reflects live database state as the `db_url` account sees it.
pub fn dispatch_stateless(method: &str, params: &Json, id: &Json, cfg: &RequestConfig, exec: &Loopback) -> Json {
    let mut response = match method {
        "server/discover" => result(
            id,
            json!({ "supportedVersions": SUPPORTED_VERSIONS, "capabilities": capabilities() }),
        ),
        // This revision removed ping.
        "ping" => error(id, -32601, &format!("method not found: {method}")),
        _ => dispatch(method, params, id, cfg, exec),
    };
    if let Some(res) = response.get_mut("result").and_then(Json::as_object_mut) {
        res.insert("resultType".into(), json!("complete"));
        res.insert("_meta".into(), json!({ META_SERVER_INFO: server_info() }));
        if matches!(method, "server/discover" | "tools/list" | "resources/list" | "resources/read") {
            res.insert("ttlMs".into(), json!(0));
            res.insert("cacheScope".into(), json!("private"));
        }
    }
    response
}

/// Route a non-initialize JSON-RPC request to its handler and return a full
/// JSON-RPC response object.
pub fn dispatch(method: &str, params: &Json, id: &Json, cfg: &RequestConfig, exec: &Loopback) -> Json {
    match method {
        "ping" => result(id, json!({})),
        "tools/list" => result(id, tools::list(cfg, exec)),
        "tools/call" => tools::call(params, id, cfg, exec),
        "resources/list" => match resources::list(cfg, exec) {
            Ok(value) => result(id, value),
            Err(msg) => error(id, -32603, &msg),
        },
        "resources/read" => resources::read(params, id, cfg, exec),
        _ => error(id, -32601, &format!("method not found: {method}")),
    }
}

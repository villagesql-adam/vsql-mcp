//! SQL execution over the `vsql_mcp.db_url` connection.
//!
//! Every tool statement goes over a `db_url` connection as a prepared
//! statement. So the `db_url` account's grants apply to all of it, and one call
//! runs exactly one statement: the server parses a prepared statement as a
//! single statement, and text after a `;` is a syntax error. The text protocol
//! would run every statement it is given, because the driver always asks for
//! multi-statement support. Only fixed text uses the text protocol: the
//! session setup, `SELECT CURRENT_USER()`, `SELECT CONNECTION_ID()`, and
//! `KILL QUERY <id>`.

use std::fmt::Write as _;
use std::io::ErrorKind;
use std::time::Duration;

use mysql::consts::ColumnType;
use mysql::prelude::{Protocol, Queryable};
use mysql::{Column, Conn, Opts, OptsBuilder, Value as MyValue};
use serde_json::{Map, Value as Json};

use crate::config::RequestConfig;
use crate::guardrails;

pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Json>,
    pub truncated: bool,
}

/// List every schema name the account can see.
pub fn schema_names(exec: &Loopback, timeout_s: u64) -> Result<Vec<String>, String> {
    let rows = exec.read_params(
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME",
        vec![],
        timeout_s,
    )?;
    Ok(rows
        .rows
        .iter()
        .filter_map(|r| r.get("SCHEMA_NAME").and_then(Json::as_str).map(str::to_owned))
        .collect())
}

/// The tables and views in one schema that the allowlist lets an agent see,
/// with row estimates. A listing narrows rather than fails: an excluded table
/// is not offered, so an agent never learns it exists. A schema with no tables
/// and a schema that does not exist are different answers, so a mistyped name
/// is corrected rather than read as empty.
pub fn visible_tables(exec: &Loopback, schema: &str, cfg: &RequestConfig) -> Result<Vec<Json>, String> {
    let rows = exec.read_params(
        "SELECT TABLE_NAME, TABLE_TYPE, TABLE_ROWS FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
        vec![MyValue::from(schema.to_owned())],
        cfg.query_timeout,
    )?;
    if rows.rows.is_empty() {
        let found = exec.read_params(
            "SELECT 1 AS present FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
            vec![MyValue::from(schema.to_owned())],
            cfg.query_timeout,
        )?;
        if found.rows.is_empty() {
            return Err(format!("no such schema: {schema}"));
        }
    }
    Ok(rows
        .rows
        .into_iter()
        .filter(|r| {
            r.get("TABLE_NAME")
                .and_then(Json::as_str)
                .is_some_and(|t| guardrails::table_allowed(t, &cfg.allowed_tables))
        })
        .collect())
}

/// Opens a fresh connection per call. Connection-per-call
/// keeps every tool invocation independent and re-entrant; the worker handles
/// requests serially, so there is no connection contention to pool around.
pub struct Loopback<'a> {
    url: &'a str,
    schema: &'a str,
    /// `vsql_mcp.schema` or `vsql_mcp.allowed_tables` is set, so the account
    /// may not hold even a read privilege on every database.
    confined: bool,
}

impl<'a> Loopback<'a> {
    pub fn new(url: &'a str, schema: &'a str, allowlist_set: bool) -> Self {
        Self {
            url,
            schema,
            confined: !schema.is_empty() || allowlist_set,
        }
    }

    fn connect(&self, timeout_s: u64) -> Result<Conn, String> {
        let mut conn = self.open(timeout_s)?;
        self.check_account(&mut conn)?;
        Ok(conn)
    }

    /// Refuse an account that holds a privilege on every database, such as
    /// root. The account's grants are what limit an agent, so an account that
    /// can reach everything leaves no limit at all. Reading every database is
    /// allowed only while neither `schema` nor `allowed_tables` is set. This
    /// reads the account's own grants, not those of a role.
    fn check_account(&self, conn: &mut Conn) -> Result<(), String> {
        let user: String = conn
            .query_first("SELECT CURRENT_USER()")
            .map_err(map_err)?
            .unwrap_or_default();
        let (name, host) = user.rsplit_once('@').unwrap_or((&user, ""));
        let grantee = format!("'{name}'@'{host}'");
        let privileges: Vec<String> = conn
            .exec(
                "SELECT PRIVILEGE_TYPE FROM information_schema.USER_PRIVILEGES WHERE GRANTEE = ?",
                (&grantee,),
            )
            .map_err(map_err)?;
        // A grant on a database pattern such as `%` reaches every database
        // whose name matches, so it counts as a grant on every database.
        let patterns: Vec<String> = conn
            .exec(
                "SELECT DISTINCT TABLE_SCHEMA FROM information_schema.SCHEMA_PRIVILEGES \
                 WHERE GRANTEE = ?",
                (&grantee,),
            )
            .map_err(map_err)?;
        let allowed: &[&str] = if self.confined {
            &["USAGE"]
        } else {
            &["USAGE", "SELECT", "SHOW VIEW"]
        };
        let extra: Vec<String> = privileges
            .into_iter()
            .filter(|p| !allowed.contains(&p.as_str()))
            .chain(
                patterns
                    .into_iter()
                    .filter(|db| db.contains('%'))
                    .map(|db| format!("a grant on `{db}`.*")),
            )
            .collect();
        if extra.is_empty() {
            return Ok(());
        }
        let mut held = extra.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        if extra.len() > 5 {
            let _ = write!(held, " and {} more", extra.len() - 5);
        }
        Err(format!(
            "the vsql_mcp.db_url account {user} holds {held} on every database; grant \
             it privileges on the exposed schema or tables only"
        ))
    }

    fn open(&self, timeout_s: u64) -> Result<Conn, String> {
        if self.url.trim().is_empty() {
            return Err("vsql_mcp.db_url is not set".to_owned());
        }
        let opts = Opts::from_url(self.url).map_err(|e| format!("invalid db_url: {e}"))?;
        let opts = OptsBuilder::from_opts(opts)
            .read_timeout(Some(Duration::from_secs(timeout_s)))
            .write_timeout(Some(Duration::from_secs(timeout_s)));
        if self.schema.is_empty() {
            return Conn::new(opts).map_err(|e| format!("loopback connect failed: {}", describe(e)));
        }
        // The exposed schema is the session's default database, so a bare table
        // name resolves inside it. It replaces any database named in `db_url`:
        // `guardrails::schema_violation` checks only qualified names, and a
        // default pointing elsewhere would send bare names past it.
        // The server reports a database the account has no grant on as access
        // denied, so it cannot tell that case from a missing database either.
        Conn::new(opts.db_name(Some(self.schema))).map_err(|e| match &e {
            mysql::Error::MySqlError(db)
                if db.code == ER_BAD_DB_ERROR || db.code == ER_DBACCESS_DENIED_ERROR =>
            {
                format!(
                    "vsql_mcp.schema '{}' does not exist, or the db_url account has no grant on it",
                    self.schema
                )
            }
            _ => format!("loopback connect failed: {}", describe(e)),
        })
    }

    /// Open a read-only session with a statement timeout and, optionally, a
    /// server-side row cap so a huge result set is never fetched only to be
    /// discarded. `row_cap` of `None` leaves `SQL_SELECT_LIMIT` at its default.
    fn read_conn(&self, timeout_s: u64, row_cap: Option<usize>) -> Result<Conn, String> {
        // Headroom so MAX_EXECUTION_TIME below is the timer that fires.
        let mut conn = self.connect(timeout_s.saturating_add(READ_TIMEOUT_HEADROOM_S))?;
        // Defense in depth beyond the statement allowlist: the session cannot
        // write regardless of what slips through classification.
        conn.query_drop("SET SESSION TRANSACTION READ ONLY")
            .map_err(|e| format!("failed to set read-only session: {e}"))?;
        let ms = timeout_s.saturating_mul(1000);
        let mut setup = format!("SET SESSION MAX_EXECUTION_TIME = {ms}");
        if let Some(cap) = row_cap {
            // One extra row so the caller can still tell it truncated.
            let _ = write!(setup, ", SQL_SELECT_LIMIT = {}", cap.saturating_add(1));
        }
        conn.query_drop(setup)
            .map_err(|e| format!("failed to configure read session: {e}"))?;
        Ok(conn)
    }
}

/// Cancels the watchdog when dropped, so a write that finishes in time is never
/// killed. Holding the sender IS the signal: dropping it wakes the thread.
struct KillWatchdog {
    _cancel: Option<std::sync::mpsc::Sender<()>>,
}

impl Loopback<'_> {
    /// Arrange for this connection's statement to be killed if it outlives
    /// `timeout_s`, and return a guard that calls the whole thing off.
    ///
    /// `KILL QUERY` aborts the running statement and rolls back what it had
    /// done, so a timed-out write does not land — which is the difference
    /// between the caller being told "unknown" and being told "did not run".
    /// A connection can always kill its own threads, so this needs no grant
    /// beyond what `db_url` already has.
    fn spawn_kill_watchdog(&self, conn: &mut Conn, timeout_s: u64) -> KillWatchdog {
        let Ok(Some(connection_id)) = conn.query_first::<u64, _>("SELECT CONNECTION_ID()") else {
            // Without an id there is nothing to kill. The client read timeout
            // still bounds the call, which is the behaviour this replaces.
            return KillWatchdog { _cancel: None };
        };
        let url = self.url.to_owned();
        let (cancel, finished) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            // Timeout means the write is still running; Disconnected means the
            // guard was dropped because it finished.
            if finished.recv_timeout(Duration::from_secs(timeout_s))
                != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            {
                return;
            }
            let Ok(opts) = Opts::from_url(&url) else {
                return;
            };
            // No default database: `KILL QUERY` needs none, and a database in
            // `db_url` that does not exist would make this connect fail.
            let opts = OptsBuilder::from_opts(opts)
                .db_name(None::<String>)
                .read_timeout(Some(Duration::from_secs(KILL_TIMEOUT_S)))
                .write_timeout(Some(Duration::from_secs(KILL_TIMEOUT_S)));
            if let Ok(mut killer) = Conn::new(opts) {
                let _ = killer.query_drop(format!("KILL QUERY {connection_id}"));
            }
        });
        KillWatchdog {
            _cancel: Some(cancel),
        }
    }

    /// Run `work` on a session that can write, killing it if it outlives
    /// `timeout_s`. MAX_EXECUTION_TIME does not apply to INSERT/UPDATE/DELETE,
    /// so the only way to bound a write is to kill it. The client read timeout
    /// gets headroom so the kill is what ends the statement, and the caller
    /// gets a definite answer instead of a dropped socket.
    fn bounded<T>(
        &self,
        timeout_s: u64,
        work: impl FnOnce(&mut Conn) -> Result<T, mysql::Error>,
    ) -> Result<T, String> {
        let mut conn = self.connect(timeout_s.saturating_add(READ_TIMEOUT_HEADROOM_S))?;
        let watchdog = self.spawn_kill_watchdog(&mut conn, timeout_s);
        let outcome = work(&mut conn).map_err(map_write_err);
        drop(watchdog);
        outcome
    }

    /// Run a write statement, returning the affected-row count.
    pub fn write(&self, sql: &str, timeout_s: u64) -> Result<u64, String> {
        self.bounded(timeout_s, |conn| {
            conn.exec_drop(sql, ())?;
            Ok(conn.affected_rows())
        })
    }
}

/// How long the watchdog's own connection may take. Short: it exists only to
/// send one statement, and a slow kill helps nobody.
const KILL_TIMEOUT_S: u64 = 10;

impl Loopback<'_> {
    /// Run a read-only statement, returning at most `max_rows` rows as JSON
    /// objects. Sets a read-only session, a statement timeout, and a row cap.
    pub fn read(&self, sql: &str, max_rows: usize, timeout_s: u64) -> Result<Rows, String> {
        let mut conn = self.read_conn(timeout_s, Some(max_rows))?;
        let result = conn.exec_iter(sql, ()).map_err(map_err)?;
        collect(result, max_rows).map_err(map_err)
    }

    /// Run a read-only statement with positional parameters, returning all of
    /// its rows.
    pub fn read_params(&self, sql: &str, params: Vec<MyValue>, timeout_s: u64) -> Result<Rows, String> {
        let mut conn = self.read_conn(timeout_s, None)?;
        let result = conn.exec_iter(sql, params).map_err(map_err)?;
        collect(result, usize::MAX).map_err(map_err)
    }

    /// Run one `SELECT` of a function call with positional parameters and
    /// return its first row. With `read_only`, the session is the read-only,
    /// time-limited one `read` uses; without it, the session can write and is
    /// bounded the way `write` is.
    pub fn call_function(
        &self,
        sql: &str,
        params: Vec<MyValue>,
        read_only: bool,
        timeout_s: u64,
    ) -> Result<Rows, String> {
        if read_only {
            let mut conn = self.read_conn(timeout_s, Some(1))?;
            let result = conn.exec_iter(sql, params).map_err(map_err)?;
            return collect(result, 1).map_err(map_err);
        }
        // A writing function gets the write tool's session.
        self.bounded(timeout_s, |conn| {
            let result = conn.exec_iter(sql, params)?;
            collect(result, 1)
        })
    }
}

/// Extra seconds the client read timeout gets over the server-side statement
/// timeout, so `MAX_EXECUTION_TIME` is what stops a slow read. Without the gap
/// the two expire together and the client wins, which turns a clean server-side
/// kill into a socket timeout. Reads only: `MAX_EXECUTION_TIME` does not apply
/// to a write, so giving the write path headroom would only delay it.
const READ_TIMEOUT_HEADROOM_S: u64 = 5;

/// The kind of the first `io::Error` in this error's source chain, if any.
/// The driver reports a client read timeout as a `CodecError` wrapping the IO
/// error rather than as `Error::IoError`, so matching the outer variant alone
/// misses it — which is why a timed-out statement used to reach the agent as
/// the driver's internal wording.
fn io_kind_of(e: &mysql::Error) -> Option<ErrorKind> {
    match e {
        mysql::Error::IoError(io) => Some(io.kind()),
        // `mysql::Error` itself does not implement `source()`, so the chain has
        // to be entered at the variant that owns the IO error. `PacketCodecError`
        // does implement it, which is enough to reach the `io::Error` without
        // naming that type here.
        mysql::Error::CodecError(codec) => std::error::Error::source(codec)
            .and_then(|s| s.downcast_ref::<std::io::Error>())
            .map(std::io::Error::kind),
        _ => None,
    }
}

/// True when the statement ran out of time, whichever timer fired: the
/// server-side `MAX_EXECUTION_TIME` (error 3024) or the client read timeout.
fn is_timeout(e: &mysql::Error) -> bool {
    if let mysql::Error::MySqlError(db) = e {
        if db.code == ER_QUERY_TIMEOUT {
            return true;
        }
    }
    matches!(
        io_kind_of(e),
        Some(ErrorKind::WouldBlock | ErrorKind::TimedOut)
    )
}

/// Render a server error as the engine's own sentence. The driver's Display is
/// a Rust debug wrapper (`MySqlError { ERROR 1146 (42S02): ... }`), and this is
/// text an agent reads and acts on, so the wrapper comes off.
fn describe(e: mysql::Error) -> String {
    match e {
        mysql::Error::MySqlError(db) => {
            format!("ERROR {} ({}): {}", db.code, db.state, db.message)
        }
        other => other.to_string(),
    }
}

/// Error mapping for the read path, where a timeout means the statement was
/// stopped.
fn map_err(e: mysql::Error) -> String {
    if is_timeout(&e) {
        return "statement exceeded vsql_mcp.query_timeout".to_owned();
    }
    describe(e)
}

/// Error mapping for the write path. `MAX_EXECUTION_TIME` does not apply to
/// INSERT/UPDATE/DELETE, so a timeout here means the client stopped waiting —
/// not that the statement stopped. Saying so is the difference between an agent
/// retrying safely and an agent double-applying a write that already landed.
fn map_write_err(e: mysql::Error) -> String {
    // The watchdog killed it: the statement was aborted and rolled back, so
    // the caller can say the write did not happen.
    if let mysql::Error::MySqlError(db) = &e {
        if db.code == ER_QUERY_INTERRUPTED {
            return "statement exceeded vsql_mcp.query_timeout and was stopped; \
                    the write was rolled back and did not take effect"
                .to_owned();
        }
    }
    // The client stopped waiting before the kill landed. Rare, and the honest
    // answer here is still that the outcome is unknown.
    if is_timeout(&e) {
        return "timed out waiting for the write after vsql_mcp.query_timeout \
                seconds; the statement may still be running on the server and \
                may still commit, so its outcome is unknown"
            .to_owned();
    }
    describe(e)
}

/// `ER_QUERY_TIMEOUT` — what a statement stopped by `MAX_EXECUTION_TIME` reports.
const ER_QUERY_TIMEOUT: u16 = 3024;

/// `ER_QUERY_INTERRUPTED` — what a killed statement reports.
const ER_QUERY_INTERRUPTED: u16 = 1317;

/// `ER_BAD_DB_ERROR` — what a connect to a missing default database reports.
const ER_BAD_DB_ERROR: u16 = 1049;

/// `ER_DBACCESS_DENIED_ERROR` — what a connect to a default database that the
/// account has no grant on reports.
const ER_DBACCESS_DENIED_ERROR: u16 = 1044;

/// Drain a query result into JSON, capping at `max_rows`. When more rows exist
/// than the cap, `truncated` is set and the surplus is consumed and discarded.
/// An error raised while rows are read is returned as the driver's own, so the
/// caller renders it the same way as an error from starting the statement.
fn collect<T: Protocol>(
    mut result: mysql::QueryResult<'_, '_, '_, T>,
    max_rows: usize,
) -> Result<Rows, mysql::Error> {
    // Keep the column metadata, not just the names: how a value should be
    // rendered depends on the column's declared type, and the row itself does
    // not carry it.
    let meta: Vec<Column> = result.columns().as_ref().to_vec();
    let columns: Vec<String> = meta.iter().map(|c| c.name_str().into_owned()).collect();

    let mut rows = Vec::new();
    let mut truncated = false;
    for row in result.by_ref() {
        // A function that fails does so here, while its row is read, not when
        // the statement starts.
        let row = row?;
        if rows.len() >= max_rows {
            truncated = true;
            // Keep draining so the connection is left in a clean state. With a
            // server-side SQL_SELECT_LIMIT this loop sees at most one extra row.
            continue;
        }
        rows.push(row_to_json(row, &columns, &meta));
    }

    Ok(Rows {
        columns,
        rows,
        truncated,
    })
}

fn row_to_json(row: mysql::Row, columns: &[String], meta: &[Column]) -> Json {
    let mut obj = Map::with_capacity(columns.len());
    // Consume the row's values instead of cloning each cell.
    for ((name, cell), col) in columns.iter().zip(row.unwrap()).zip(meta) {
        obj.insert(name.clone(), value_to_json(cell, col));
    }
    Json::Object(obj)
}

/// Whether a column holds bytes rather than text, and so should be hex-encoded
/// whatever those bytes happen to be.
///
/// Deciding this by whether the bytes parse as UTF-8 gets it wrong per row: a
/// BIT or GEOMETRY value made only of bytes that are valid UTF-8 came back as
/// raw control characters while the next row of the same column came back as
/// hex, and nothing in the response said which. The column's declared type is
/// the same for every row, so it is the thing to ask.
fn is_binary_column(col: &Column) -> bool {
    // The binary "character set", which is how MySQL marks a string column as
    // holding bytes.
    const BINARY_CHARSET: u16 = 63;
    match col.column_type() {
        ColumnType::MYSQL_TYPE_BIT | ColumnType::MYSQL_TYPE_GEOMETRY => true,
        // JSON is always text, and carries the binary charset on some server
        // versions, so it has to be excluded before the charset test below.
        ColumnType::MYSQL_TYPE_JSON => false,
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB
        | ColumnType::MYSQL_TYPE_STRING
        | ColumnType::MYSQL_TYPE_VAR_STRING
        | ColumnType::MYSQL_TYPE_VARCHAR => col.character_set() == BINARY_CHARSET,
        _ => false,
    }
}

fn value_to_json(v: MyValue, col: &Column) -> Json {
    match v {
        MyValue::NULL => Json::Null,
        MyValue::Int(i) => Json::from(i),
        MyValue::UInt(u) => Json::from(u),
        MyValue::Float(f) => Json::from(f),
        MyValue::Double(d) => Json::from(d),
        MyValue::Bytes(b) => {
            if is_binary_column(col) {
                return Json::String(format!("0x{}", hex(&b)));
            }
            match String::from_utf8(b) {
                Ok(s) => Json::String(s),
                // A text column holding bytes that are not valid UTF-8 would
                // otherwise have to lose them; hex keeps the value whole.
                Err(e) => Json::String(format!("0x{}", hex(e.as_bytes()))),
            }
        }
        // A date-typed value carries a time whether or not it is used, so the
        // driver's rendering drops the time part for any DATETIME at midnight,
        // the zero datetime included. The column type says which it is.
        MyValue::Date(y, m, d, h, min, s, us) => {
            if col.column_type() == ColumnType::MYSQL_TYPE_DATE {
                Json::String(format!("{y:04}-{m:02}-{d:02}"))
            } else if us > 0 {
                Json::String(format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}.{us:06}"))
            } else {
                Json::String(format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}"))
            }
        }
        // TIME renders as its canonical SQL string form, negatives included.
        other => Json::String(other.as_sql(true).trim_matches('\'').to_owned()),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

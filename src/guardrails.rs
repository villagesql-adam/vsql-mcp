//! Statement checks applied before a tool query runs.
//!
//! They pick the tool and the session a statement runs in, and give an agent a
//! clear refusal. They are not the limit on what an agent can reach: the
//! `db_url` account's grants are, along with the read-only session and the
//! one-statement-per-call prepared protocol in `executor`.

use serde_json::Value as Json;

/// What a submitted statement is allowed to do.
#[derive(Debug, PartialEq, Eq)]
pub enum StmtKind {
    Read,
    Write,
    /// DDL, multi-statement, or anything not on either allowlist.
    Disallowed,
}

const READ_VERBS: &[&str] = &["select", "show", "describe", "desc", "explain", "with"];
const WRITE_VERBS: &[&str] = &["insert", "update", "delete", "replace"];

/// Classify a single SQL statement by its leading keyword, as the server reads
/// it: leading comments are skipped, and the text of an executable comment counts.
/// A statement the server may read in more than one way is `Disallowed`. A trailing second statement (a `;` with
/// non-whitespace after it) forces `Disallowed` — one statement per tool call.
pub fn classify(sql: &str) -> StmtKind {
    if has_trailing_statement(sql) || tokens(sql).contains(&Tok::Unclear) {
        return StmtKind::Disallowed;
    }
    let first = leading_verb(sql);
    if READ_VERBS.iter().any(|v| first.eq_ignore_ascii_case(v)) {
        StmtKind::Read
    } else if WRITE_VERBS.iter().any(|v| first.eq_ignore_ascii_case(v)) {
        StmtKind::Write
    } else {
        StmtKind::Disallowed
    }
}

/// Whether `sql` is one statement of a form the `ddl` tool runs:
/// `CREATE`/`ALTER`/`DROP`/`RENAME TABLE` or `CREATE`/`DROP INDEX`. Databases,
/// users, grants, views, stored programs and `TRUNCATE` are not on the list.
///
/// Comments are allowed only before the statement. After it starts, a
/// comment, a double quote or a backslash refuses it: no migration needs them,
/// and `ANSI_QUOTES` makes a double-quoted name an identifier the scanners read
/// as a string. A `/*! */` comment refuses it even before the statement,
/// because the server runs the text inside one.
pub fn is_allowed_ddl(sql: &str) -> bool {
    let stripped = strip_leading(sql);
    if sql.contains("/*!")
        || ["/*", "#", "--", "\"", "\\"]
            .iter()
            .any(|t| stripped.contains(t))
        || has_trailing_statement(stripped)
        || mentions_tablespace(stripped)
    {
        return false;
    }
    let lower = stripped.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().take(3).collect();
    matches!(
        words.as_slice(),
        ["create" | "alter" | "drop" | "rename", "table", ..]
            | ["create" | "drop", "temporary", "table"]
            | ["create" | "drop", "index", ..]
            | ["create", "unique" | "fulltext" | "spatial", "index"]
    )
}

/// No migration needs a tablespace clause. `DISCARD` and `IMPORT TABLESPACE`
/// need files an agent cannot place, and `TABLESPACE = innodb_system` puts a
/// table in the system tablespace, whose space is never given back.
fn mentions_tablespace(sql: &str) -> bool {
    idents_outside_strings(sql)
        .iter()
        .any(|w| w.eq_ignore_ascii_case("tablespace"))
}

/// The statement's leading keyword, lowercased. Empty when the statement
/// starts with something that is not a bare word.
pub fn leading_verb(sql: &str) -> String {
    match tokens(sql).first() {
        Some(Tok::Word { text, quoted: false }) => text.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// The file-writing target of a `SELECT ... INTO OUTFILE`/`INTO DUMPFILE`, if
/// the statement has one.
///
/// These begin with `SELECT`, so classification reads them as a read, and a
/// read-only transaction does not stop a write to the filesystem. Without this
/// the only thing refusing them is the loopback account lacking `FILE`, which
/// makes a guardrail called read-only depend entirely on a grant.
pub fn file_write_target(sql: &str) -> Option<&'static str> {
    let mut previous_was_into = false;
    for ident in idents_outside_strings(sql) {
        if previous_was_into {
            if ident.eq_ignore_ascii_case("outfile") {
                return Some("INTO OUTFILE");
            }
            if ident.eq_ignore_ascii_case("dumpfile") {
                return Some("INTO DUMPFILE");
            }
        }
        previous_was_into = ident.eq_ignore_ascii_case("into");
    }
    None
}

/// What a `SHOW` or `DESCRIBE` statement is asking about.
pub enum MetadataTarget {
    /// A specific table, which the allowlist can be applied to.
    Table(String),
    /// Something not scoped to one table — `SHOW TABLES`, `SHOW DATABASES`,
    /// `SHOW STATUS`. There is no single object to check, and the answer would
    /// enumerate names the allowlist exists to withhold.
    NotTableScoped,
}

/// Read the object out of a `SHOW`/`DESCRIBE` statement.
///
/// These cannot be planned with `EXPLAIN`, so the allowlist cannot learn what
/// they read the way it does for a query. The table-scoped forms name their
/// object plainly enough to check directly; the rest do not, and are refused.
///
/// Returns `None` when the statement is not a `SHOW`/`DESCRIBE` at all.
pub fn metadata_target(sql: &str) -> Option<MetadataTarget> {
    let idents = idents_outside_strings(sql);
    let mut it = idents.iter().map(String::as_str);
    let verb = it.next()?;

    // DESCRIBE t / DESC db.t name the table next.
    if verb.eq_ignore_ascii_case("describe") || verb.eq_ignore_ascii_case("desc") {
        return Some(last_of_qualified(&mut it));
    }
    if !verb.eq_ignore_ascii_case("show") {
        return None;
    }

    match it.next() {
        // SHOW CREATE TABLE t, SHOW CREATE VIEW v
        Some(w) if w.eq_ignore_ascii_case("create") => match it.next() {
            Some(k) if k.eq_ignore_ascii_case("table") || k.eq_ignore_ascii_case("view") => {
                Some(last_of_qualified(&mut it))
            }
            _ => Some(MetadataTarget::NotTableScoped),
        },
        // SHOW COLUMNS FROM t, SHOW INDEX FROM t, and their synonyms. The
        // object follows FROM or IN.
        Some(w)
            if ["columns", "fields", "index", "indexes", "keys"]
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k)) =>
        {
            for token in it.by_ref() {
                if token.eq_ignore_ascii_case("from") || token.eq_ignore_ascii_case("in") {
                    return Some(last_of_qualified(&mut it));
                }
            }
            Some(MetadataTarget::NotTableScoped)
        }
        _ => Some(MetadataTarget::NotTableScoped),
    }
}

/// True for a `SHOW` statement that can read outside one schema without a
/// `schema.` qualifier, which `schema_violation` would catch. `FROM` or `IN`
/// can name any schema (`SHOW TABLES FROM otherdb`), `SHOW CREATE DATABASE`
/// names one directly, and the rest list every schema at once.
pub fn show_outside_schema(sql: &str) -> bool {
    let idents: Vec<String> = idents_outside_strings(sql)
        .into_iter()
        .map(|w| w.to_ascii_lowercase())
        .collect();
    let w: Vec<&str> = idents.iter().map(String::as_str).collect();
    match w.as_slice() {
        ["show", "databases" | "schemas", ..]
        | ["show", "create", "database" | "schema", ..]
        | ["show", "procedure" | "function", "status", ..]
        | ["show", "open", "tables", ..] => true,
        ["show", rest @ ..] => rest.iter().any(|w| *w == "from" || *w == "in"),
        _ => false,
    }
}

/// Take the next name and keep the table half, matching how the allowlist
/// matches. `idents_outside_strings` emits `db.t` as one token, so the
/// qualifier is still visible here.
fn last_of_qualified<'a>(it: &mut impl Iterator<Item = &'a str>) -> MetadataTarget {
    match it.next() {
        None => MetadataTarget::NotTableScoped,
        Some(name) => MetadataTarget::Table(
            name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase(),
        ),
    }
}

/// One piece of a statement, split the way the server's lexer splits it.
/// Strings and comments produce nothing. The text inside an executable comment
/// (`/*! ... */`) is read as if the comment were not there, because the server
/// runs it.
#[derive(Debug, PartialEq)]
enum Tok<'a> {
    /// An identifier or keyword; `quoted` for a backtick-quoted one.
    Word { text: &'a str, quoted: bool },
    Number,
    Dot,
    Comma,
    Semi,
    Open,
    Close,
    Other,
    /// Text the server may read in more than one way: a versioned executable
    /// comment (`/*!80000 ... */`), which the server reads as code or skips
    /// depending on its own version, or a quote after an odd run of
    /// backslashes in a string, which ends the string only under
    /// `NO_BACKSLASH_ESCAPES`.
    Unclear,
}

/// Split `sql` into tokens. This is the one place that models MySQL's string,
/// comment and identifier rules, so every check reads a statement the same way:
/// `''` and a backslash both escape a quote inside a string, `--` starts a
/// comment only when whitespace or the end follows it, and an identifier may
/// start with a digit, `$`, or a non-ASCII character.
fn tokens(sql: &str) -> Vec<Tok<'_>> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if rest.starts_with(b"/*!") {
            i += 3;
            if bytes.get(i).is_some_and(u8::is_ascii_digit) {
                out.push(Tok::Unclear);
            }
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        } else if rest.starts_with(b"*/") {
            i += 2;
        } else if rest.starts_with(b"/*") {
            i = rest[2..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(bytes.len(), |end| i + 2 + end + 2);
        } else if c == b'#' || is_dash_comment(rest) {
            i = rest.iter().position(|&b| b == b'\n').map_or(bytes.len(), |end| i + end + 1);
        } else if c == b'\'' || c == b'"' {
            let (end, unclear) = skip_string(bytes, i);
            if unclear {
                out.push(Tok::Unclear);
            }
            i = end;
        } else if c == b'`' || is_ident_byte(c) {
            let (text, next) = read_ident(sql, i);
            i = next;
            out.push(if c != b'`' && is_number(text) {
                Tok::Number
            } else {
                Tok::Word { text, quoted: c == b'`' }
            });
        } else {
            out.push(match c {
                b'.' => Tok::Dot,
                b',' => Tok::Comma,
                b';' => Tok::Semi,
                b'(' => Tok::Open,
                b')' => Tok::Close,
                _ => Tok::Other,
            });
            i += 1;
        }
    }
    out
}

/// `--` starts a comment only when whitespace, a control character, or the end
/// of the statement follows it; `1--1` is arithmetic.
fn is_dash_comment(rest: &[u8]) -> bool {
    rest.starts_with(b"--") && rest.get(2).is_none_or(|&b| b.is_ascii_whitespace() || b.is_ascii_control())
}

/// The index just past the string that starts at `start`, honouring both the
/// doubled-quote and the backslash escape, and whether the string has a quote
/// after an odd run of backslashes. That quote is escaped by default but ends
/// the string under `NO_BACKSLASH_ESCAPES`, so the string's end is unclear.
fn skip_string(bytes: &[u8], start: usize) -> (usize, bool) {
    let quote = bytes[start];
    let mut unclear = false;
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let run = bytes[i..].iter().take_while(|&&b| b == b'\\').count();
            unclear |= run % 2 == 1 && bytes.get(i + run) == Some(&quote);
            i += run + run % 2;
        } else if bytes[i] == quote {
            if bytes.get(i + 1) != Some(&quote) {
                return (i + 1, unclear);
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    (bytes.len(), unclear)
}

/// Every identifier in the statement, in order, with `db.t` emitted as one
/// entry so a caller can tell a qualifier from the next word.
fn idents_outside_strings(sql: &str) -> Vec<String> {
    let toks = tokens(sql);
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if let Tok::Word { text, .. } = toks[i] {
            if let (Some(Tok::Dot), Some(Tok::Word { text: right, .. })) = (toks.get(i + 1), toks.get(i + 2)) {
                out.push(format!("{text}.{right}"));
                i += 3;
                continue;
            }
            out.push(text.to_owned());
        }
        i += 1;
    }
    out
}

/// Strip leading `--`, `#`, and `/* */` comments and whitespace.
fn strip_leading(sql: &str) -> &str {
    let mut s = sql.trim_start();
    loop {
        if is_dash_comment(s.as_bytes()) {
            s = s.split_once('\n').map_or("", |x| x.1).trim_start();
        } else if let Some(rest) = s.strip_prefix('#') {
            s = rest.split_once('\n').map_or("", |x| x.1).trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map_or("", |x| x.1).trim_start();
        } else {
            return s;
        }
    }
}

/// True if a `;` is followed by anything but more `;`s, that is a second
/// statement. A trailing `;` alone is fine.
fn has_trailing_statement(sql: &str) -> bool {
    let toks = tokens(sql);
    toks.iter()
        .position(|t| *t == Tok::Semi)
        .is_some_and(|p| toks[p + 1..].iter().any(|t| *t != Tok::Semi))
}

/// When a schema is configured, reject any `schema.` qualifier that names a
/// different schema. An unqualified name needs no check: the loopback session's
/// default database is the configured schema (see `Loopback::open`), so the
/// name resolves inside it.
///
/// Not every `x.y` is a schema reference. `p.name` qualifies a column with a
/// table alias and `t1.id` with a table, and reading those as schemas made
/// ordinary aliased SQL unusable whenever this setting was on. Names the
/// statement itself binds — the tables it selects from and the aliases it gives
/// them — are therefore excluded before the comparison.
///
/// The check stays deliberately broad otherwise, so a qualifier that is not a
/// table or alias is still tested. That is what keeps a call into another
/// schema's stored function (`otherdb.some_function()`) from slipping by.
/// Qualifiers inside string and backtick literals are ignored throughout.
pub fn schema_violation(sql: &str, configured: &str) -> Option<String> {
    if configured.is_empty() {
        return None;
    }
    let refs = table_refs(sql);

    // A qualifier in table position is a schema, whatever else the statement
    // binds. Testing these first means an alias cannot mask one: without this,
    // `FROM otherdb.t otherdb` binds the very name that qualifies it.
    for r in &refs {
        if let Some(schema) = &r.schema {
            if !schema.eq_ignore_ascii_case(configured) {
                return Some(schema.clone());
            }
        }
    }

    let mut bound: Vec<String> = Vec::new();
    for r in refs {
        bound.push(r.table);
        if let Some(alias) = r.alias {
            bound.push(alias);
        }
    }
    for q in qualified_refs(sql) {
        // `alias.column` is not a schema, but `name.f()` always calls a
        // function in schema `name`, whatever the statement binds.
        if !q.call && bound.iter().any(|b| b.eq_ignore_ascii_case(&q.schema)) {
            continue;
        }
        if !q.schema.eq_ignore_ascii_case(configured) {
            return Some(q.schema);
        }
    }
    None
}

/// The `ddl` tool's form of `schema_violation`: every `x.y` qualifier must name
/// the configured schema, with no exception for names the statement binds.
/// That exception lets `t.col` through in a query, but in DDL a qualifier after
/// a comma or `TO` is a schema that `table_refs` does not see, so in
/// `DROP TABLE app.secret, secret.salaries` the bound table name `secret` would
/// have hidden the schema `secret`.
pub fn ddl_schema_violation(sql: &str, configured: &str) -> Option<String> {
    if configured.is_empty() {
        return None;
    }
    qualified_refs(sql)
        .into_iter()
        .map(|q| q.schema)
        .find(|schema| !schema.eq_ignore_ascii_case(configured))
}

/// A `<ident>.<ident>` reference: the qualifier lowercased, and whether a `(`
/// follows, which makes it a call to a function in that schema.
struct Qualified {
    schema: String,
    call: bool,
}

/// Find `<ident>.<ident>` references.
fn qualified_refs(sql: &str) -> Vec<Qualified> {
    let toks = tokens(sql);
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if let (Tok::Word { text: left, .. }, Some(Tok::Dot), Some(Tok::Word { .. })) =
            (&toks[i], toks.get(i + 1), toks.get(i + 2))
        {
            out.push(Qualified {
                schema: left.to_ascii_lowercase(),
                call: toks.get(i + 3) == Some(&Tok::Open),
            });
            i += 3;
            continue;
        }
        i += 1;
    }
    out
}

/// A table named in table position, with its `schema.` qualifier and the alias
/// it was given, if it has either.
pub struct TableRef {
    pub schema: Option<String>,
    pub table: String,
    pub alias: Option<String>,
}

/// The base tables an allowlist should check for `sql`, given its EXPLAIN plan.
///
/// `EXPLAIN FORMAT=JSON` reports the **alias** in `table_name`, not the base
/// table — `FROM orders o` shows `table_name: "o"`. Checking those names
/// directly is wrong in both directions: a legitimate aliased join of allowed
/// tables is rejected (the alias is not on the list), and a forbidden table
/// aliased as an allowed name is admitted (`secret AS customers` shows
/// `customers`). So each EXPLAIN name is mapped back to the base table it
/// aliases, using the alias→table pairs the statement text yields. A name that
/// is not one of the statement's aliases is a real table — an unaliased table,
/// or the underlying table a view exposed to the plan — and is kept as-is. When
/// EXPLAIN named nothing (the optimizer answered without reading a table), the
/// base tables from the text are used instead.
pub fn tables_for_allowlist(sql: &str, explain: &Json) -> Vec<String> {
    let refs = table_refs(sql);
    let alias_to_base: std::collections::HashMap<String, String> = refs
        .iter()
        .filter_map(|r| r.alias.as_ref().map(|a| (a.clone(), r.table.clone())))
        .collect();
    let mut names = tables_in_explain(explain);
    if names.is_empty() {
        names = refs.into_iter().map(|r| r.table).collect();
    }
    names
        .into_iter()
        .map(|n| alias_to_base.get(&n).cloned().unwrap_or(n))
        .collect()
}

/// Tables appearing in table position in the statement text: the identifier
/// after `FROM`, `JOIN`, `INTO`, `UPDATE` or `TABLE`, with a `schema.`
/// qualifier dropped so the result matches what `EXPLAIN` reports, plus the
/// alias bound to it.
///
/// Two callers. The allowlist (via `tables_for_allowlist`) uses the alias→table
/// pairs to map EXPLAIN's alias-named plan entries back to base tables, and the
/// base names as a fallback when the optimizer answers without naming a table.
/// Schema scoping uses the names to tell a `schema.` qualifier apart from an
/// alias or a table qualifying one of its own columns.
///
/// Deliberately eager on the table side: over-reporting a name costs a
/// rejection, under-reporting one costs the allowlist.
pub fn table_refs(sql: &str) -> Vec<TableRef> {
    const TABLE_KEYWORDS: &[&str] = &["from", "join", "straight_join", "into", "update", "table"];
    // Words that can follow a table reference without being an alias. Missing
    // one costs a needless rejection; a schema named after one of these would
    // be the only way to lose a check, which is not a case worth widening for.
    const NOT_AN_ALIAS: &[&str] = &[
        "on", "using", "where", "group", "order", "limit", "having", "join", "inner", "left",
        "right", "full", "cross", "natural", "straight_join", "union", "set", "for", "into",
        "values", "select", "and", "or", "not", "partition", "force", "use", "ignore", "with",
        "window", "procedure", "lock", "offset", "as",
    ];
    let toks = tokens(sql);
    let mut out: Vec<TableRef> = Vec::new();
    let mut expect_table = false;
    let mut i = 0;
    while i < toks.len() {
        let Some((text, quoted)) = word(toks.get(i)) else {
            // `FROM (t1)` still names a table, so `(` keeps the expectation;
            // anything else ends it.
            if toks[i] != Tok::Open {
                expect_table = false;
            }
            i += 1;
            continue;
        };
        i += 1;
        if !quoted && TABLE_KEYWORDS.iter().any(|k| text.eq_ignore_ascii_case(k)) {
            expect_table = true;
            continue;
        }
        if !expect_table {
            continue;
        }
        expect_table = false;
        // `FROM (SELECT ...)` is a derived table, not a name; the inner FROM
        // sets the expectation again for the real one.
        if !quoted && ["select", "with", "values"].iter().any(|k| text.eq_ignore_ascii_case(k)) {
            continue;
        }
        // `schema.table` keeps the right half, matching EXPLAIN, and
        // remembers the left so scoping can test it.
        let mut schema = None;
        let mut table = text.to_ascii_lowercase();
        if toks.get(i) == Some(&Tok::Dot) {
            if let Some((right, _)) = word(toks.get(i + 1)) {
                schema = Some(table);
                table = right.to_ascii_lowercase();
                i += 2;
            }
        }
        // An alias may follow, with or without AS. A word that cannot be an
        // alias is left unconsumed, because some of them (JOIN) introduce the
        // next table.
        let mut alias = None;
        match word(toks.get(i)) {
            Some((next, false)) if next.eq_ignore_ascii_case("as") => {
                if let Some((named, _)) = word(toks.get(i + 1)) {
                    alias = Some(named.to_ascii_lowercase());
                    i += 2;
                }
            }
            Some((next, next_quoted))
                if next_quoted || !NOT_AN_ALIAS.iter().any(|k| next.eq_ignore_ascii_case(k)) =>
            {
                alias = Some(next.to_ascii_lowercase());
                i += 1;
            }
            _ => {}
        }
        out.push(TableRef { schema, table, alias });
        // A comma after a table, or after the `)` closing `(t1 a)`, starts the
        // next table of a comma join.
        while toks.get(i) == Some(&Tok::Close) {
            i += 1;
        }
        if toks.get(i) == Some(&Tok::Comma) {
            expect_table = true;
            i += 1;
        }
    }
    out
}

/// The text of a word token, and whether it was backtick-quoted.
fn word<'a>(t: Option<&Tok<'a>>) -> Option<(&'a str, bool)> {
    match t {
        Some(Tok::Word { text, quoted }) => Some((text, *quoted)),
        _ => None,
    }
}

/// Read one identifier (bare or backtick-quoted) starting at byte `start`,
/// returning the identifier text and the byte index just past it.
fn read_ident(sql: &str, start: usize) -> (&str, usize) {
    let bytes = sql.as_bytes();
    if bytes.get(start) == Some(&b'`') {
        let mut i = start + 1;
        // A doubled backtick is a backtick inside the name.
        while i < bytes.len() && (bytes[i] != b'`' || bytes.get(i + 1) == Some(&b'`')) {
            i += if bytes[i] == b'`' { 2 } else { 1 };
        }
        let text = &sql[start + 1..i];
        // Skip the closing backtick when present.
        return (text, if i < bytes.len() { i + 1 } else { i });
    }
    let mut i = start;
    while i < bytes.len() && is_ident_byte(bytes[i]) {
        i += 1;
    }
    (&sql[start..i], i)
}

/// Whether `c` can be part of an unquoted MySQL identifier: ASCII letters,
/// digits, `_`, `$`, and any byte of a non-ASCII character. An identifier may
/// start with any of them, so `2mydb.t` and `$mydb.t` name schemas `2mydb` and
/// `$mydb`, not `mydb`.
fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

/// An unquoted word made only of digits is a number, never an identifier, so
/// the `1` in `1.5` does not qualify the `5`.
fn is_number(word: &str) -> bool {
    !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit())
}

/// Given the JSON returned by `EXPLAIN FORMAT=JSON`, collect every
/// `table_name`. Used to enforce `allowed_tables` on whatever the optimizer
/// says the query actually touches — joins and subqueries included.
pub fn tables_in_explain(explain: &Json) -> Vec<String> {
    let mut out = Vec::new();
    walk_tables(explain, &mut out);
    out
}

fn walk_tables(node: &Json, out: &mut Vec<String>) {
    match node {
        Json::Object(map) => {
            if let Some(Json::String(name)) = map.get("table_name") {
                // The optimizer names its own materialisation steps here —
                // `<union1,2>`, `<derived2>`, `<subquery3>`. They are never on
                // an allowlist and are not tables; the real tables feeding them
                // appear elsewhere in the same plan and are still collected.
                if !name.starts_with('<') {
                    out.push(name.to_ascii_lowercase());
                }
            }
            for v in map.values() {
                walk_tables(v, out);
            }
        }
        Json::Array(items) => {
            for v in items {
                walk_tables(v, out);
            }
        }
        _ => {}
    }
}

/// Refuse `table` when the allowlist does not permit it.
pub fn check_table(table: &str, allowlist: &[String]) -> Result<(), String> {
    if table_allowed(table, allowlist) {
        Ok(())
    } else {
        Err(format!("table '{table}' is not in vsql_mcp.allowed_tables"))
    }
}

/// True if `table` (a bare name from EXPLAIN) is permitted by an allowlist that
/// may hold bare or `schema.table` entries. Matching is on the table name.
pub fn table_allowed(table: &str, allowlist: &[String]) -> bool {
    if allowlist.is_empty() {
        return true;
    }
    allowlist.iter().any(|entry| {
        let bare = entry.rsplit('.').next().unwrap_or(entry);
        bare.eq_ignore_ascii_case(table)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_reads_the_leading_verb() {
        assert_eq!(classify("SELECT 1"), StmtKind::Read);
        assert_eq!(classify("  -- note\n/* c */ select 1"), StmtKind::Read);
        assert_eq!(classify("INSERT INTO t VALUES (1)"), StmtKind::Write);
        assert_eq!(classify("DROP TABLE t"), StmtKind::Disallowed);
        assert_eq!(classify("(SELECT 1)"), StmtKind::Disallowed);
    }

    #[test]
    fn classify_reads_inside_an_executable_comment() {
        assert_eq!(classify("/*! DELETE FROM t */ SELECT 1"), StmtKind::Write);
        // A `#` comment inside the executable comment hides its first `*/`.
        assert_eq!(
            classify("/*!40000 # */ INSERT INTO t VALUES (1)\n DROP TABLE t */"),
            StmtKind::Disallowed
        );
    }

    #[test]
    fn a_second_statement_is_disallowed() {
        assert_eq!(classify("SELECT 1; DROP TABLE t"), StmtKind::Disallowed);
        assert_eq!(classify("SELECT 1;"), StmtKind::Read);
        assert_eq!(classify("SELECT 1; -- done"), StmtKind::Read);
        assert_eq!(classify("SELECT 'a;b'"), StmtKind::Read);
        assert_eq!(classify("SELECT 1 -- ;x"), StmtKind::Read);
        // A quote inside a comment does not open a string.
        assert_eq!(classify("SELECT 1 -- '\n; DELETE FROM t; -- '"), StmtKind::Disallowed);
        // A backslash before a quote makes the string's end unclear.
        assert_eq!(classify("SELECT 'a\\' ; SELECT 2' AS s"), StmtKind::Disallowed);
        assert_eq!(classify("SELECT 'a\\'' ; DELETE FROM t"), StmtKind::Disallowed);
    }

    #[test]
    fn statements_the_server_may_read_two_ways_are_disallowed() {
        // The server skips a versioned comment newer than itself.
        assert_eq!(classify("SELECT * FROM /*!999999 'x */ other.t /*!999999 ' */"), StmtKind::Disallowed);
        assert_eq!(classify("/*!80000 SELECT */ 1"), StmtKind::Disallowed);
        // `\'` ends the string under NO_BACKSLASH_ESCAPES.
        assert_eq!(classify("SELECT 'a\\' AS x, other.f() AS y, 'b' AS z"), StmtKind::Disallowed);
        // An even run of backslashes is the same either way.
        assert_eq!(classify("SELECT 'a\\\\' AS x"), StmtKind::Read);
        assert_eq!(classify("SELECT 'a\\_b' AS x"), StmtKind::Read);
        // An unversioned executable comment is always code, so it is read.
        assert_eq!(classify("SELECT /*! 1 */"), StmtKind::Read);
    }

    #[test]
    fn schema_violation_finds_every_other_schema() {
        let other = |sql: &str| schema_violation(sql, "app");
        assert_eq!(other("SELECT * FROM app.t"), None);
        assert_eq!(other("SELECT p.total FROM orders p"), None);
        assert_eq!(other("SELECT t1.id FROM t1"), None);
        assert_eq!(other("SELECT 'other.t'"), None);
        assert_eq!(other("SELECT 1.5"), None);
        for sql in [
            "SELECT * FROM other.t",
            "SELECT * FROM `other`.t",
            "SELECT * FROM other . t",
            "SELECT * FROM other/* gap */.t",
            "SELECT * FROM 2app.t",
            "SELECT * FROM $app.t",
            "SELECT * FROM \u{e9}app.t",
            "SELECT other.f()",
            // A comma join or STRAIGHT_JOIN with an alias named after the schema.
            "SELECT * FROM t1 other, other.t",
            "SELECT * FROM t1 other STRAIGHT_JOIN other.t",
            "SELECT * FROM (t1 other), other.t",
            // A quote in a comment, a backslash escape, and `--` without a space.
            "SELECT /* ' */ other.f() /* ' */",
            "SELECT '\\'' , other.f() -- '",
            "SELECT 1--1, other.f()",
            "SELECT /*!other.f()*/",
            // An alias named after the schema does not hide a function call.
            "SELECT other.f() FROM app.t1 other",
        ] {
            assert!(other(sql).is_some(), "{sql}");
        }
    }

    #[test]
    fn ddl_schema_violation_checks_every_qualifier() {
        let other = |sql: &str| ddl_schema_violation(sql, "app");
        assert_eq!(other("CREATE TABLE app.t (i INT)"), None);
        assert_eq!(other("DROP TABLE app.secret, secret.t").as_deref(), Some("secret"));
        assert_eq!(other("RENAME TABLE app.t TO other.t").as_deref(), Some("other"));
        assert_eq!(other("DROP TABLE 2app.t").as_deref(), Some("2app"));
    }

    #[test]
    fn file_writes_are_found() {
        assert_eq!(file_write_target("SELECT 1 INTO OUTFILE '/tmp/x'"), Some("INTO OUTFILE"));
        assert_eq!(file_write_target("SELECT 1 INTO/* x */DUMPFILE '/tmp/x'"), Some("INTO DUMPFILE"));
        assert_eq!(file_write_target("SELECT 1--1 INTO OUTFILE '/tmp/x'"), Some("INTO OUTFILE"));
        assert_eq!(file_write_target("SELECT 'into outfile'"), None);
    }

    #[test]
    fn metadata_target_reads_the_table() {
        let table = |sql: &str| match metadata_target(sql) {
            Some(MetadataTarget::Table(t)) => Some(t),
            _ => None,
        };
        assert_eq!(table("DESCRIBE app.orders").as_deref(), Some("orders"));
        assert_eq!(table("SHOW COLUMNS FROM orders").as_deref(), Some("orders"));
        assert_eq!(table("SHOW CREATE TABLE `secret`").as_deref(), Some("secret"));
        assert_eq!(table("DESCRIBE `secret` orders").as_deref(), Some("secret"));
        assert!(matches!(metadata_target("SHOW TABLES"), Some(MetadataTarget::NotTableScoped)));
        assert!(metadata_target("SELECT 1").is_none());
    }

    #[test]
    fn show_outside_schema_forms() {
        assert!(show_outside_schema("SHOW TABLES FROM other"));
        assert!(show_outside_schema("SHOW DATABASES"));
        assert!(show_outside_schema("SHOW /*!40000 DATABASES */"));
        assert!(!show_outside_schema("SHOW TABLES"));
        assert!(!show_outside_schema("SELECT 1 FROM t"));
    }

    #[test]
    fn ddl_forms() {
        assert!(is_allowed_ddl("CREATE TABLE t (i INT)"));
        assert!(is_allowed_ddl("create unique index ix on t (i)"));
        assert!(!is_allowed_ddl("DROP DATABASE d"));
        assert!(!is_allowed_ddl("CREATE TABLE t (i INT); DROP TABLE u"));
        assert!(!is_allowed_ddl("CREATE TABLE t (i INT) TABLESPACE = innodb_system"));
        assert!(!is_allowed_ddl("/*!CREATE TABLE t (i INT)*/"));
    }

    #[test]
    fn allowlist_maps_aliases_to_tables() {
        let plan = serde_json::json!({"query_block": {"table": {"table_name": "customers"}}});
        assert_eq!(tables_for_allowlist("SELECT * FROM secret AS customers", &plan), ["secret"]);
        let plan = serde_json::json!({"query_block": {"nested_loop": [
            {"table": {"table_name": "a"}}, {"table": {"table_name": "customers"}}]}});
        assert_eq!(
            tables_for_allowlist("SELECT * FROM orders a, secret customers", &plan),
            ["orders", "secret"]
        );
        assert!(table_allowed("orders", &["app.orders".to_owned()]));
        assert!(!table_allowed("secret", &["orders".to_owned()]));
    }
}

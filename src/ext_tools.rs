//! Tools that other extensions add to `tools/list`.
//!
//! An extension declares tools by registering a zero-argument function named
//! `mcp_tools` that returns a JSON array of tool definitions. Each definition
//! names a handler, another function the same extension registers, and a call
//! to the tool becomes `SELECT <extension>.<handler>(?)` with the arguments
//! bound as one JSON string. The call is always qualified with the extension
//! name: several extensions may register `mcp_tools`, and the server refuses an
//! unqualified call to a name that more than one of them provides.
//!
//! Nothing is cached. An `UNINSTALL EXTENSION` removes the extension's
//! functions and its registration row, so the next `tools/list` does not find
//! its tools and a call to one of them is an unknown tool.

use mysql::Value as MyValue;
use serde_json::{json, Value as Json};

use crate::config::RequestConfig;
use crate::executor::{col, QueryExecutor};
use crate::httpd::log;
use crate::resources::quote_ident;

/// The function an extension registers to declare its tools.
const MANIFEST_FN: &str = "mcp_tools";

/// Joins the extension name and the tool name in an advertised tool name, such
/// as `vsql_branch__create`. A tool name may not contain it, so the last
/// occurrence in an advertised name always ends the extension name.
const SEP: &str = "__";

struct ExtTool {
    extension: String,
    name: String,
    description: String,
    input_schema: Json,
    handler: String,
    write: bool,
}

impl ExtTool {
    fn full_name(&self) -> String {
        format!("{}{SEP}{}", self.extension, self.name)
    }

    fn definition(&self) -> Json {
        json!({
            "name": self.full_name(),
            "description": self.description,
            "inputSchema": self.input_schema
        })
    }
}

/// The extension tools an agent may see. A `write` tool is left out while
/// `allow_write` is OFF, for the same reason the built-in `write` tool is.
pub fn list(cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Vec<Json> {
    discover(None, cfg, exec)
        .iter()
        .filter(|t| cfg.allow_write || !t.write)
        .map(ExtTool::definition)
        .collect()
}

/// Run an extension tool and return the handler's result as text. A name that
/// no installed extension declares is an unknown tool.
pub fn call(
    name: &str,
    args: &Json,
    cfg: &RequestConfig,
    exec: &dyn QueryExecutor,
) -> Result<String, String> {
    let unknown = || format!("unknown tool: {name}");
    let (extension, _) = name.rsplit_once(SEP).ok_or_else(unknown)?;
    let tool = discover(Some(extension), cfg, exec)
        .into_iter()
        .find(|t| t.full_name() == name)
        .ok_or_else(unknown)?;
    if tool.write && !cfg.allow_write {
        return Err(format!(
            "{name} is a write tool and is disabled (set vsql_mcp.allow_write = ON)"
        ));
    }
    let args = if args.is_null() {
        "{}".to_owned()
    } else {
        args.to_string()
    };
    let sql = format!(
        "SELECT {}.{}(?) AS result",
        quote_ident(&tool.extension),
        quote_ident(&tool.handler)
    );
    let rows = exec.call_function(
        &sql,
        vec![MyValue::from(args)],
        !tool.write,
        cfg.query_timeout,
    )?;
    match first_cell(&rows.rows) {
        Some(Json::String(s)) => Ok(s.clone()),
        // A function that raises a warning returns NULL, and the warning text
        // stays on the connection. NULL is never a useful tool result, so it
        // is reported as a failure the author can fix by raising an error.
        Some(Json::Null) | None => Err(format!(
            "{name} returned NULL; the handler '{}' reported no result",
            tool.handler
        )),
        Some(other) => Ok(other.to_string()),
    }
}

/// Read the tools of every installed extension, or of the one named in
/// `only`. An extension whose manifest cannot be read or is invalid adds no
/// tools, and one warning naming it goes to the error log; the other
/// extensions' tools are unaffected.
fn discover(only: Option<&str>, cfg: &RequestConfig, exec: &dyn QueryExecutor) -> Vec<ExtTool> {
    // Every manifest and handler call needs the db_url connection; without it
    // there is nothing to offer, and a warning per extension would only repeat
    // what an unset db_url already says.
    if cfg.db_url.trim().is_empty() {
        return Vec::new();
    }
    // `only` comes from the tool name the client sent, so it is matched here
    // rather than passed to the query: `read_params` may splice its parameters
    // into the SQL text on the in-process path.
    let rows = exec.read_params(
        "SELECT EXTENSION_NAME, REGISTRATION_JSON FROM INFORMATION_SCHEMA.EXTENSION_REGISTRATION \
         ORDER BY EXTENSION_NAME",
        vec![],
        &[col("EXTENSION_NAME"), col("REGISTRATION_JSON")],
        cfg.query_timeout,
    );
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            log(&format!(
                "could not read extension registrations for MCP tools: {e}"
            ));
            return Vec::new();
        }
    };
    let mut tools = Vec::new();
    for row in &rows.rows {
        let (Some(extension), Some(registration)) = (
            row.get("EXTENSION_NAME").and_then(Json::as_str),
            row.get("REGISTRATION_JSON").and_then(Json::as_str),
        ) else {
            continue;
        };
        if only.is_some_and(|o| o != extension) {
            continue;
        }
        let funcs = registered_functions(registration);
        if !funcs.iter().any(|f| f == MANIFEST_FN) {
            continue;
        }
        match read_manifest(extension, &funcs, cfg, exec) {
            Ok(mut declared) => tools.append(&mut declared),
            Err(e) => log(&format!(
                "ignoring the MCP tools of extension '{extension}': {e}"
            )),
        }
    }
    tools
}

/// The names of the functions in one `REGISTRATION_JSON` value.
fn registered_functions(registration: &str) -> Vec<String> {
    let Ok(parsed) = serde_json::from_str::<Json>(registration) else {
        return Vec::new();
    };
    parsed
        .get("funcs")
        .and_then(Json::as_array)
        .map(|funcs| {
            funcs
                .iter()
                .filter_map(|f| f.get("name").and_then(Json::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Call one extension's `mcp_tools()` in a read-only session and validate what
/// it returns. Any defect rejects the whole manifest, so an extension never
/// shows an agent half of its tools.
fn read_manifest(
    extension: &str,
    funcs: &[String],
    cfg: &RequestConfig,
    exec: &dyn QueryExecutor,
) -> Result<Vec<ExtTool>, String> {
    let sql = format!(
        "SELECT {}.{MANIFEST_FN}() AS manifest",
        quote_ident(extension)
    );
    let rows = exec.call_function(&sql, vec![], true, cfg.query_timeout)?;
    let text = match first_cell(&rows.rows) {
        Some(Json::String(s)) => s,
        _ => return Err(format!("{MANIFEST_FN}() did not return a string")),
    };
    let entries: Vec<Json> = serde_json::from_str(text)
        .map_err(|e| format!("{MANIFEST_FN}() did not return a JSON array: {e}"))?;
    let mut tools: Vec<ExtTool> = Vec::with_capacity(entries.len());
    for entry in &entries {
        let tool = parse_tool(extension, funcs, entry)?;
        if tools.iter().any(|t| t.name == tool.name) {
            return Err(format!("tool '{}' is declared twice", tool.name));
        }
        tools.push(tool);
    }
    Ok(tools)
}

fn parse_tool(extension: &str, funcs: &[String], entry: &Json) -> Result<ExtTool, String> {
    let text = |key: &str| {
        entry
            .get(key)
            .and_then(Json::as_str)
            .ok_or_else(|| format!("a tool has no string field \"{key}\""))
    };
    let name = text("name")?;
    if !valid_tool_name(name) {
        return Err(format!(
            "tool name '{name}' must start with a letter or digit, use only letters, \
             digits, '_' and '-', and not contain '{SEP}'"
        ));
    }
    let description = text("description")?;
    let input_schema = entry
        .get("inputSchema")
        .filter(|s| s.is_object())
        .ok_or_else(|| format!("tool '{name}' has no object field \"inputSchema\""))?;
    let handler = text("handler")?;
    // The handler is called qualified with this extension's name, so a
    // function from anywhere else could not be reached anyway; refusing it here
    // says why in the error log instead of failing on every call.
    if !funcs.iter().any(|f| f == handler) {
        return Err(format!(
            "tool '{name}': handler '{handler}' is not a function extension '{extension}' registers"
        ));
    }
    let write = match text("kind")? {
        "read" => false,
        "write" => true,
        other => {
            return Err(format!(
                "tool '{name}': kind '{other}' is not 'read' or 'write'"
            ))
        }
    };
    Ok(ExtTool {
        extension: extension.to_owned(),
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: input_schema.clone(),
        handler: handler.to_owned(),
        write,
    })
}

fn valid_tool_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !name.contains(SEP)
}

fn first_cell(rows: &[Json]) -> Option<&Json> {
    rows.first()?.as_object()?.values().next()
}

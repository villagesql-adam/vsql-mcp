# VillageSQL MCP Extension

`vsql_mcp` exposes a VillageSQL database as a
[Model Context Protocol](https://modelcontextprotocol.io) server, so MCP
clients — Claude Code, Claude Desktop, IDE agents — can discover the schema and
run governed queries without a sidecar process. Where `vsql-rest` turns a schema
into a REST API for programs, `vsql-mcp` turns it into a tool surface for
agents.

It serves MCP over the **Streamable HTTP** transport (spec revision
`2025-06-18`) on a listener owned by a background worker inside the server
process. See [Quick start](#quick-start) to go from installing the extension to
a connected agent.

## Requirements

`vsql_mcp` needs VillageSQL 0.0.8 or later. A 0.0.7 server refuses it:

```
ERROR 3219 (HY000): Failed to load VEF extension 'vsql_mcp': no matching ABI version for capability 'vsql::sys_var' (extension requires ver-2 + descriptor ver-1)
```

`vsql_mcp` uses the VEF preview capabilities `thread_worker`, `sys_var`, and
`status_var`, so the server must allow preview extensions:

```bash
mysqld --vsql_allow_preview_extensions=ON ...
```

On a running server, `SET PERSIST vsql_allow_preview_extensions = ON` takes
effect at once: the next `INSTALL EXTENSION` succeeds, with no restart.
`SET GLOBAL` is rejected, because the server requires the setting to survive a
restart.

Without preview extensions allowed, the install fails:

```
ERROR 3219 (HY000): Failed to load VEF extension 'vsql_mcp': extension requires preview capabilities but vsql_allow_preview_extensions is OFF
```

## Building

Requires `cargo-vsql` (`cargo install cargo-vsql`) and a Rust toolchain 1.88+.
The [VillageSQL Rust SDK](https://github.com/villagesql/vsql-rust-sdk) comes
from crates.io.

```bash
cargo vsql package
```

That compiles in release mode and writes `dist/vsql_mcp.veb`. Copy the bundle
into the directory the server loads extensions from. Ask the server which
directory that is:

```sql
SHOW VARIABLES LIKE 'veb_dir';
```

`cargo vsql install` packages and installs into a server build tree, if you
have one:

```bash
export VillageSQL_BUILD_DIR=/path/to/villagesql/build
cargo vsql install
```

## Installing

```sql
INSTALL EXTENSION vsql_mcp;
```

The extension registers its configuration and status variables immediately;
nothing listens until you turn `vsql_mcp.vsql_mcp_enabled` ON.
Every tool that reads the database needs `vsql_mcp.db_url` — see
[How queries run](#how-queries-run).

`UNINSTALL EXTENSION vsql_mcp` takes those variables away again, and with them
every value you gave them, including values set with `SET PERSIST`. A reinstall
therefore starts from the defaults in [Configuration](#configuration), and the
configuration has to be applied again.

## Quick start

Expose one schema and connect an agent to it. Run this SQL in a MySQL client
connected to your VillageSQL server as an administrator (for example
`mysql -u root -h 127.0.0.1`); the server must already be running with preview
extensions allowed (see [Requirements](#requirements)):

```sql
INSTALL EXTENSION vsql_mcp;

-- A dedicated least-privilege account the server runs tool queries as.
CREATE USER 'mcp'@'127.0.0.1' IDENTIFIED BY 'change-me';
GRANT SELECT ON mydb.* TO 'mcp'@'127.0.0.1';

SET GLOBAL vsql_mcp.db_url = 'mysql://mcp:change-me@127.0.0.1:3306';
SET GLOBAL vsql_mcp.schema = 'mydb';
SET GLOBAL vsql_mcp.require_auth = ON;
SET GLOBAL vsql_mcp.bearer_token = 'a-long-random-token';
SET GLOBAL vsql_mcp.vsql_mcp_enabled = ON;
```

The server now listens on `http://127.0.0.1:3100/mcp` (the default `port`).
Register that URL and the bearer token with your client.

### Claude Code

```bash
claude mcp add --transport http vsql http://127.0.0.1:3100/mcp \
  --header "Authorization: Bearer a-long-random-token"
```

### Codex

```bash
export VSQL_MCP_TOKEN=a-long-random-token
codex mcp add vsql --url http://127.0.0.1:3100/mcp \
  --bearer-token-env-var VSQL_MCP_TOKEN
```

`--bearer-token-env-var` takes the name of an environment variable, not the
token itself. Codex reads that variable when it connects, so export it in the
shell you start Codex from. Passing the token where the name belongs is accepted
without complaint and the server then loads no tools.

The command writes the server to `~/.codex/config.toml`, which you can edit
directly instead:

```toml
[mcp_servers.vsql]
url = "http://127.0.0.1:3100/mcp"
bearer_token_env_var = "VSQL_MCP_TOKEN"
```

Run `/mcp` in a Codex session to confirm the tools arrived.

### Antigravity

Add the server to `~/.gemini/config/mcp_config.json`:

```json
{
  "mcpServers": {
    "vsql": {
      "serverUrl": "http://127.0.0.1:3100/mcp",
      "headers": { "Authorization": "Bearer a-long-random-token" }
    }
  }
}
```

Antigravity names the endpoint field `serverUrl`, where most other clients call
it `url`. Recent builds accept `url` too, but `serverUrl` is the documented
field and the one every version reads.

Your agent can now discover the schema and query it. Any other client that
speaks MCP Streamable HTTP connects the same way — a URL plus an
`Authorization: Bearer` header (Cursor, VS Code, Windsurf, and the OpenAI
Responses API all support it).
There is no stdio transport: the server runs inside the database, so there is no
child process for a client to spawn. A stdio-only client, or a remote client
that proxies through a vendor cloud (such as Claude Desktop's connectors, which
cannot reach `127.0.0.1`), needs a generic stdio↔HTTP bridge or a tunnel in
front of the endpoint.

## How queries run

Every tool and resource runs its SQL over a **loopback client connection** to
the server, configured by `vsql_mcp.db_url`, so every call runs as that account
under its real `GRANT`s. Each statement goes to the server as a prepared
statement, which the server runs as exactly one statement: text after a `;` is
a syntax error, never a second statement. Values that a tool looks up, such as
a schema or table name, are bound parameters. The `vsql://<schema>/<table>`
resource puts the names into `SHOW CREATE TABLE` quoted with backticks. The
listing tools show only the schemas and tables that the account has grants on.

## Configuration

All variables are `SET GLOBAL vsql_mcp.<name>`.

| Variable | Default | Purpose |
|---|---|---|
| `vsql_mcp_enabled` | OFF | Start/stop the server |
| `port` | 3100 | HTTP listen port (0 = OS-assigned) |
| `schema` | `""` | Schema to expose (empty = every schema the `db_url` account can see) |
| `require_auth` | OFF | Require a bearer token on every request |
| `bearer_token` | `""` | Static token checked when `require_auth` is ON |
| `allow_write` | OFF | Enable the `write` tool |
| `allow_ddl` | OFF | Enable the `ddl` tool |
| `allowed_tables` | `""` | Comma-separated table allowlist; empty means all |
| `max_rows` | 1000 | Row cap per `query` result |
| `query_timeout` | 30 | Per-tool-call statement timeout (seconds) |
| `session_ttl` | 1800 | Idle MCP session lifetime (seconds) |
| `db_url` | `""` | Loopback DSN tool queries run through |

A port change takes effect when the server is next enabled — toggle
`vsql_mcp_enabled` OFF then ON after changing it.

## Tools

The server advertises seven built-in tools via `tools/list`, followed by the
tools that installed extensions declare (see
[Tools from other extensions](#tools-from-other-extensions)):

| Tool | Purpose |
|---|---|
| `list_schemas` | Schemas the `db_url` account can see |
| `list_tables` | Tables and views in a schema, with row estimates |
| `describe_table` | Columns, types, and keys for a table |
| `query` | Run a single read-only `SELECT` and return JSON rows |
| `explain` | `EXPLAIN FORMAT=JSON` for a candidate query |
| `write` | One `INSERT`/`UPDATE`/`DELETE`/`REPLACE` (requires `allow_write = ON`) |
| `ddl` | One `CREATE`/`ALTER`/`DROP`/`RENAME TABLE` or `CREATE`/`DROP INDEX` (requires `allow_ddl = ON`) |

`write` is advertised in `tools/list` only while `allow_write` is ON, and `ddl`
only while `allow_ddl` is ON and `allowed_tables` is empty, so an agent never
plans around a tool it cannot use.

Tool results follow the MCP shape: a `content` array with a JSON text block,
plus `isError`. A rejected statement returns `isError: true` with a message
naming the guardrail that stopped it. An `UPDATE` sent to the `query` tool is
refused with:

```
only a single read-only statement is allowed by the query tool
```

### Schema changes

The `ddl` tool lets an agent run a migration without a MySQL client. It accepts
one statement of these forms, and refuses every other statement:

- `CREATE TABLE`, `CREATE TEMPORARY TABLE`, `ALTER TABLE`, `DROP TABLE`,
  `RENAME TABLE`
- `CREATE INDEX` (also `UNIQUE`, `FULLTEXT`, `SPATIAL`), `DROP INDEX`

It does not create or drop a database, a view, a user, a grant, a stored
program, or a trigger, and it does not run `TRUNCATE` or any statement with a
`TABLESPACE` clause, such as `ALTER TABLE ... DISCARD TABLESPACE`. A `DROP DATABASE` sent
to it is refused with:

```
the ddl tool accepts a single CREATE, ALTER, DROP or RENAME TABLE, or CREATE or DROP INDEX statement
```

The statement runs over the `db_url` connection, so the account's grants decide
which schemas the tool can change. Grant `CREATE`, `ALTER`, `DROP`, and `INDEX`
only on the schemas that agents may change, and do not grant `FILE`: without
it, the server refuses a `DATA DIRECTORY` clause. A statement outside those grants
reaches the server and fails there:

```
ERROR 1142 (42000): CREATE command denied to user 'mcp'@'localhost' for table 'x'
```

`schema` scoping applies to `ddl` as it does to `write`. While `allowed_tables`
is set, `ddl` is not listed and a call to it is refused: the allowlist is
checked by planning a statement with `EXPLAIN`, which cannot plan DDL.

Comments are allowed before the statement, such as a `-- migration 12` line.
After the statement starts, a comment, a double quote, or a backslash refuses
it, and so does a `/*! */` comment anywhere. The statement is sent as a
prepared statement, which the server runs as exactly one statement, so text
after a `;` is a syntax error and never a second statement.

### Guardrails

Applied to every `query`, `explain`, and `write` call (and to `ddl` as described
in [Schema changes](#schema-changes)). The guardrails read the statement text to
give an agent a clear refusal. A statement can get past them, so they do not
replace the `db_url` account's grants (see
[Security Considerations](#security-considerations)):

- **Read-only enforcement.** `query` accepts a single `SELECT`/`SHOW`/`WITH`/
  `EXPLAIN`/`DESCRIBE`; the loopback session is also set `READ ONLY`. `write`
  accepts a single `INSERT`/`UPDATE`/`DELETE`/`REPLACE` and only when
  `allow_write` is ON. A trailing second statement is rejected, and so is a
  statement that the server could read in two ways: one with a versioned
  comment such as `/*!80000 ... */`, or a string with a quote after a backslash
  (write `''` for a quote instead).
- **No row ceiling on writes.** `max_rows` caps what a `query` returns; nothing
  caps what a `write` changes. An unqualified `DELETE` empties the table. Give
  `db_url` an account whose grants match the blast radius you are willing to
  accept.
- **Schema scoping.** With `schema` set, a reference to another schema is
  rejected. A statement can still reach another schema without naming it, for
  example through a view or a stored function, so grant the `db_url` account
  nothing outside the exposed schema. The exposed schema is the loopback connection's default database,
  so a bare table name such as `orders` resolves inside it, and any database
  named in `db_url` is ignored. If the schema does not exist, every tool that
  runs SQL fails with
  `vsql_mcp.schema 'mydb' does not exist, or the db_url account has no grant on it`. Aliases and table-qualified columns
  work normally — `p.total` is a column, not a schema.

  A `SHOW` statement with `FROM` or `IN`, `SHOW CREATE DATABASE`, and a `SHOW`
  statement that lists every schema (`SHOW DATABASES`, `SHOW OPEN TABLES`) are
  rejected. Use the `list_tables` and `describe_table` tools instead.

  Scoping reads the statement, so a view inside the exposed schema that selects
  from another one is reachable through it, as SQL intends for a view. Use
  `allowed_tables` when you need confinement that holds through a view: that
  check plans the statement and sees the underlying table.
- **Table allowlist.** With `allowed_tables` set, the statement is planned with
  `EXPLAIN FORMAT=JSON` and every table it touches (joins and subqueries
  included) must be on the list. When the optimizer answers without naming a
  table — `MIN()`/`MAX()` from an index, an impossible `WHERE`, `LIMIT 0` — the
  tables named in the statement text are checked instead, so an elided plan is
  not a way past the list. Matching is on the table name.

  The allowlist also governs what an agent can learn about a table, not only
  what it can read: `describe_table` and `vsql://<schema>/<table>` refuse an
  excluded table, and `list_tables` and `resources/list` omit it.

  `SHOW` and `DESCRIBE` cannot be planned, so their object is read from the
  statement instead. The forms naming one table — `DESCRIBE t`,
  `SHOW CREATE TABLE t`, `SHOW COLUMNS FROM t`, `SHOW INDEX FROM t` — are
  checked against the list like any other reference. The forms that enumerate,
  such as `SHOW TABLES` and `SHOW DATABASES`, are refused while a list is set,
  because their answer is the names the list exists to withhold; the message
  points at `list_tables` and `describe_table`, which filter.
- **No writes to the filesystem.** `SELECT ... INTO OUTFILE` and `INTO DUMPFILE`
  are refused. They begin with `SELECT`, so neither the statement allowlist nor
  the read-only session stops them; without this the only thing that would is
  the `db_url` account lacking `FILE`.
- **Row cap.** `max_rows` caps a `query` result and marks it `truncated`.
- **Statement timeout.** `query_timeout` bounds each call. A read is stopped by
  `MAX_EXECUTION_TIME`; a write, which that setting does not govern, is stopped
  by killing the statement, so it rolls back rather than landing after the call
  has returned. A client read timeout backs both up.

## Resources

Clients can pull schema context without a tool round-trip:

- `vsql://<schema>` — a schema overview (its tables)
- `vsql://<schema>/<table>` — that table's `CREATE TABLE` DDL

## Authentication

Set `require_auth = ON` and `bearer_token` to require
`Authorization: Bearer <token>` on every request; requests without it get
HTTP 401. This is a single static token — for per-user identity, use a dedicated
`db_url` account so MySQL `GRANT`s do the enforcing.

The transport also validates the `Origin` header (a request from a non-local
origin gets HTTP 403), as the MCP Streamable HTTP spec requires.

## Monitoring

`SHOW STATUS LIKE 'vsql_mcp%'` reports:

| Status variable | Meaning |
|---|---|
| `vsql_mcp.tool_calls_total` | Tool calls handled |
| `vsql_mcp.tool_errors_total` | Tool calls that returned an error |
| `vsql_mcp.sessions_active` | Live MCP sessions |
| `vsql_mcp.rows_returned_total` | Rows returned by the `query` tool |
| `vsql_mcp.http_port` | Actually-bound port (0 = not listening) |

`SELECT vsql_mcp.info();` returns the same liveness summary as JSON. The
worker tries to bind the listener on its next wakeup, which comes every 25 ms,
so a call right after `vsql_mcp_enabled` is set can report `"starting": true`
with `"enabled": false`.

## Troubleshooting

Each error below is what a tool call returns to the agent.

```
vsql_mcp.db_url is not set
```

Set `vsql_mcp.db_url` to the account that tool queries run as. See
[Quick start](#quick-start).

```
the vsql_mcp.db_url account root@localhost holds INSERT, UPDATE, DELETE, CREATE, DROP and 64 more on every database; grant it privileges on the exposed schema or tables only
```

The `db_url` account can reach every database. Create a dedicated account with
grants on the exposed schema only, as in [Quick start](#quick-start), and point
`db_url` at it.

```
vsql_mcp.schema 'mydb' does not exist, or the db_url account has no grant on it
```

Check the spelling of `vsql_mcp.schema`, then grant the `db_url` account access
to that schema.

```
loopback connect failed: ERROR 1045 (28000): Access denied for user 'mcp'@'localhost' (using password: YES)
```

The user name or password in `db_url` is wrong.

For the errors that `INSTALL EXTENSION` can return, see
[Requirements](#requirements).

## Known Limitations

- **No SSE / server-initiated messages.** `GET /mcp` returns HTTP 405, which the
  spec allows for servers without a stream; there are no progress notifications.
- **Requests are handled one at a time.** The worker drains and serves requests
  serially; `query_timeout` bounds how long any one call can hold the line.
  Each open HTTP connection holds a thread in the database server, before any
  authentication, so a local process that opens many idle connections adds many
  server threads.
- **A read can hold locks.** A read-only session still allows
  `SELECT ... FOR SHARE` and `GET_LOCK()`, so a `query` call can make writers
  wait for up to `query_timeout` seconds.
- **The server timeout covers `SELECT` only.** `MAX_EXECUTION_TIME` does not
  apply to `SHOW` or `DESCRIBE`. For those, the client stops waiting after
  `query_timeout` plus five seconds, and the server may run the statement a
  little longer. A
  stalled request body is read on a helper thread with a bounded wait, so a slow
  client cannot wedge the worker or block disable/shutdown.
- **The allowlist does not see through a stored function.** `allowed_tables` and
  `schema` are enforced on the tables a statement plans with `EXPLAIN`, which
  does not descend into a function body. A `SELECT some_function()` names no
  table, so it passes both checks even if the function reads an excluded table.
  When you need hard confinement, put it in the `db_url` account's `GRANT`s, not
  only in `allowed_tables`. The same holds for the
  [tools other extensions declare](#tools-from-other-extensions): `allowed_tables`
  and `schema` do not apply to them.
- **An agent can call every installed extension's functions.** VillageSQL does
  not check a privilege when a statement calls an extension function, so a
  `db_url` account with only `SELECT` on one schema can still call them, and no
  grant stops it. A function that opens a network connection, such as
  `http_get()` or a DuckDB query that reads a URL, lets an agent reach the network
  from the database server. Install only the extensions that you would let an
  agent call.
- **Any account can read the secrets.** `bearer_token` and `db_url` (which
  embeds a password) are global variables, and any MySQL account can read them
  with `SHOW VARIABLES`, even one with only `USAGE`. The agent can too, through
  the `query` tool. An extension cannot mark a
  variable as sensitive yet. Anyone who reads the password can connect as the
  `db_url` account, so give that account only what an agent may have.
- **A `write` that overruns is killed, not waited out.** `MAX_EXECUTION_TIME`
  applies to `SELECT` only, so `query_timeout` is enforced on a write by issuing
  `KILL QUERY` against its own connection. The statement is aborted and rolled
  back, so a timed-out write does not take effect and the tool says so. If the
  kill itself cannot be delivered the result falls back to reporting the outcome
  as unknown.

## Security Considerations

`vsql_mcp` opens a network listener and executes agent-supplied SQL, so treat it
as a security boundary:

- The listener binds `127.0.0.1` only and serves plain HTTP. For remote access
  or TLS, put a reverse proxy in front of it, and let the proxy limit how many
  connections a client can hold open.
- The `db_url` account's `GRANT`s are the limit on what an agent can read and
  change. `schema`, `allowed_tables`, and the statement checks give an agent a
  clear refusal, but a statement can get past them. Grant the account only what
  an agent may reach:

  | When you use | Grant on the exposed schema, or on each allowed table |
  |---|---|
  | The read tools | `SELECT` (and `SHOW VIEW` for views) |
  | `write` | `INSERT`, `UPDATE`, `DELETE` |
  | `ddl` | `CREATE`, `ALTER`, `DROP`, `INDEX` |

  Do not grant `FILE`, any global privilege, or any privilege on another
  schema. Every tool that runs SQL checks the account first, and refuses to run
  when it holds a privilege on every database (`*.*`), such as root, or a
  grant on a database pattern that contains `%`. `SELECT`
  and `SHOW VIEW` on every database are allowed only while neither `schema` nor
  `allowed_tables` is set. The check reads the account's own grants, not the
  privileges of a role it uses.
- Keep `require_auth` ON for anything beyond a local experiment, and prefer a
  proxy that maps identities over the single static token.
- `allowed_tables` and `schema` also decide which tables the tools list.

## Adding a tool

The tool surface lives in `src/tools.rs`. `tool_definitions()` holds the
definition of every tool, `list()` decides which of them an agent sees, and
`call()` routes a tool name to its handler. A tool of your own is one
definition, one match arm, and one function.

Add the definition to `tool_definitions()`:

```rust
{
    "name": "list_indexes",
    "description": "Indexes on a table, one row per indexed column.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "schema": { "type": "string" },
            "table": { "type": "string" }
        },
        "required": ["table"]
    }
},
```

Route the name in `call()`:

```rust
"list_indexes" => list_indexes(args, cfg, exec),
```

Then write the handler, which receives the call arguments, the live
configuration, and the executor:

```rust
fn list_indexes(args: &Json, cfg: &RequestConfig, exec: &Loopback) -> Result<Json, String>
```

`describe_table` is the closest handler to copy, and the helpers it uses are
available to any handler:

- `guardrails::table_allowed` applies the operator's `allowed_tables` list.
- `effective_schema` returns the configured schema when `vsql_mcp.schema` is
  set, and the caller's `schema` argument when it is not.
- `exec.read_params` runs a statement with positional parameters. Pass every
  caller value as a parameter, never in the SQL text.
- `exec.read()` runs a statement with the `max_rows` cap.

Build the new bundle and copy it into `veb_dir`:

```bash
cargo vsql package
```

Then reload the extension. No server restart is involved:

```sql
UNINSTALL EXTENSION vsql_mcp;
INSTALL EXTENSION vsql_mcp;
```

The next `tools/list` includes your tool. The reinstall also returns every
`vsql_mcp.*` setting to its default, so apply the configuration again.

The `mcp_basic`, `mcp_protocol`, and `mcp_security` tests assert the exact tool
list, so a new tool has to be named in them too. Bump the version in
`manifest.json` and `Cargo.toml` together for any behavior change.

## Tools from other extensions

Any installed extension can add tools to `tools/list` without a change to
`vsql_mcp`. The extension registers a function named `mcp_tools` that takes no
arguments and returns a JSON array. Each element of the array declares one tool:

| Field | Meaning |
|---|---|
| `name` | Tool name, unique within the extension. Letters, digits, `_` and `-`; it starts with a letter or digit and does not contain `__` |
| `description` | Text that the agent reads |
| `inputSchema` | JSON Schema object for the arguments |
| `handler` | Name of a function, registered by the same extension, that runs the tool |
| `kind` | `read` or `write` |

`tools/list` shows each tool as `<extension>__<name>`, so two extensions cannot
declare the same tool name. Several extensions can each register `mcp_tools`;
`vsql_mcp` always calls it as `<extension>.mcp_tools()`.

A call to the tool runs `SELECT <extension>.<handler>(?)` over the `db_url`
connection, so the `db_url` account's `GRANT`s apply. The call arguments are
bound as one JSON string and are never put into the SQL text. `vsql_mcp` does
not check the arguments against `inputSchema`, so the handler checks them. The
function's result is the tool result, as text. An error that the function
raises is a tool result with `isError: true`, and so is a NULL result, which is
what a function returns when it raises a warning. Raise an error, not a
warning, so that the agent reads why the call failed.

The rules:

- `vsql_mcp` reads every manifest again on each `tools/list` and each call.
  After `UNINSTALL EXTENSION`, the extension's tools are gone.
- Extension tools need `vsql_mcp.db_url`. While it is not set, no extension
  tools are listed.
- A `handler` must be a function that the same extension registers. A manifest
  that names any other function is refused.
- A `read` tool runs in the read-only, time-limited session that the `query`
  tool uses. A `write` tool runs in the session that the `write` tool uses, and
  is listed and callable only while `vsql_mcp.allow_write` is ON.
- `kind` is a label that each extension sets for itself. A read-only session
  stops the statement from writing to tables, but it does not stop side effects
  inside the extension's function, such as a network call. Install an extension
  only when you trust what its tools do.
- When a manifest is not a JSON array, or one of its tools breaks a rule above,
  `vsql_mcp` drops every tool of that extension and writes one warning to the
  error log on each `tools/list`. The other tools stay listed:

  ```
  [vsql_mcp] ignoring the MCP tools of extension 'mcp_tooltest': a tool has no string field "description"
  ```

- `mcp_tools()` must return the whole array within its `buffer_size`. With the
  Rust SDK, a longer result fails the call, so the extension's tools are
  dropped as above.

A complete Rust extension, `vsql_greet`, with one `read` tool. Its
`Cargo.toml` depends on `villagesql` and `serde_json`:

```rust
use villagesql::{InValue, VdfReturn};

const TOOLS: &str = r#"[
  {
    "name": "greet",
    "description": "Greet someone by name.",
    "inputSchema": {
      "type": "object",
      "properties": { "name": { "type": "string" } },
      "required": ["name"]
    },
    "handler": "greet",
    "kind": "read"
  }
]"#;

fn mcp_tools(_args: &[InValue]) -> VdfReturn {
    VdfReturn::string(TOOLS)
}

// The tool's arguments arrive as one JSON string, such as {"name":"Ada"}.
fn greet(args: &[InValue]) -> VdfReturn {
    let Some(InValue::String(text)) = args.first() else {
        return VdfReturn::error("greet expects its arguments as a JSON string");
    };
    let parsed: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return VdfReturn::error(format!("arguments are not JSON: {e}")),
    };
    match parsed.get("name").and_then(serde_json::Value::as_str) {
        Some(name) => VdfReturn::string(format!("Hello, {name}!")),
        None => VdfReturn::error("missing argument: name"),
    }
}

villagesql::extension! {
    funcs: [
        villagesql::func!(mcp_tools, "mcp_tools", [] -> villagesql::Type::String, buffer_size: 1024),
        villagesql::func!(greet, "greet", [villagesql::Type::String] -> villagesql::Type::String),
    ],
}
```

After `INSTALL EXTENSION vsql_greet`, `tools/list` includes:

```json
{
  "description": "Greet someone by name.",
  "inputSchema": {
    "properties": {
      "name": {
        "type": "string"
      }
    },
    "required": [
      "name"
    ],
    "type": "object"
  },
  "name": "vsql_greet__greet"
}
```

A `tools/call` of `vsql_greet__greet` with `{"name": "Ada"}` returns the text
`Hello, Ada!`. The same call with `{}` returns `isError: true` and the text:

```
ERROR 3200 (HY000): VDF error in function 'greet': missing argument: name
```

## Testing

See [TESTING.md](TESTING.md).

## Contributing

See the [VillageSQL Contributing Guide](https://github.com/villagesql/villagesql-server/blob/main/CONTRIBUTING.md).

## Reporting Bugs and Requesting Features

Open an issue on [GitHub](https://github.com/villagesql/vsql-mcp/issues).

## Contact

- Discord: https://discord.gg/KSr6whd3Fr
- GitHub Issues: https://github.com/villagesql/vsql-mcp/issues

## License

GPL-2.0. See [LICENSE](LICENSE).

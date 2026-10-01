//! Tools from MCP servers.
//!
//! `ATHENA_MCP_CONFIG` names a JSON file in the `mcpServers` shape that
//! Claude Code and Claude Desktop use:
//!
//! ```json
//! {"mcpServers": {
//!   "files": {"command": "mcp-server-files", "args": ["--root", "/srv"],
//!             "env": {"API_TOKEN": "${FILES_TOKEN}"}},
//!   "wiki":  {"url": "https://wiki.example/mcp",
//!             "headers": {"Authorization": "Bearer ${WIKI_TOKEN}"}}
//! }}
//! ```
//!
//! Unset means no MCP tools, and Athena looks in no default place: a file in
//! the working directory must never add tools to an agent that did not ask.
//!
//! - A server with `command` is a stdio server, a child process of Athena
//!   **on the host, with Athena's user permissions, not in the sandbox**.
//!   One with `url` is a streamable-HTTP server. Legacy SSE is refused.
//! - Secrets stay out of the file: `${VAR}` (or `${VAR:-default}`) in `args`,
//!   `env` values and `headers` values is read from Athena's environment,
//!   which `.env` or a secrets file fills. `$${` is a literal `${`. An unset
//!   variable skips that server. `url` is not expanded, and the path and
//!   query of any URL are cut from the messages that name one. No value is
//!   ever logged, only variable names.
//! - A stdio child gets only its declared `env`, plus `PATH` and `HOME` from
//!   Athena. Not the rest of the environment: it holds Athena's own keys.
//!   (`TMPDIR`, `LANG` and proxy settings are among what a server must
//!   declare if it needs them.)
//! - Every server is connected at startup, concurrently, each within
//!   `startupTimeoutSecs` (default 60). One that fails to start, to
//!   handshake or to list its tools is a warning and its tools are missing;
//!   the agent runs without them. Tool calls time out after `timeoutSecs`
//!   (default 120) with an error the model sees.
//! - Rig registers a tool under the name its server gave it and cannot alias
//!   one, so a tool whose name is taken (by a built-in, or by a server that
//!   sorts first) is skipped with a warning. So is one that a model provider
//!   would reject, failing every request with it: a name of the wrong form,
//!   or a schema that is not an object. A server's tools past
//!   [`MAX_TOOLS_PER_SERVER`] are skipped, and a description is cut to
//!   [`MAX_DESCRIPTION_BYTES`].
//! - Their descriptions and output are untrusted data, like a web page, and
//!   `ToolPolicy` limits calls and results like any other tool's
//!   (`policy.rs`).
//! - One connection serves every user and session of the process, with the
//!   credentials in its `env` and `headers`.

use crate::sandbox::stream::split_at_boundary;
use anyhow::{Context, Result, bail};
use axum::http::{HeaderName, HeaderValue};
use futures_util::future::join_all;
use regex::Regex;
use rig_agent::agent::{AgentBuilder, WithBuilderTools};
use rmcp::model::{ClientCapabilities, ClientInfo, Implementation, Tool};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::process::Command;

/// The environment variable that names the MCP config file.
pub const CONFIG_ENV: &str = "ATHENA_MCP_CONFIG";
/// How long a tool call may take unless the server's entry says otherwise.
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a server may take to start, handshake and list its tools. Long
/// enough for `npx` or `uvx` to fetch a package on first use.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a server gets to exit on its own at shutdown before it is killed.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Model providers reject longer tool names.
const MAX_TOOL_NAME_BYTES: usize = 64;
/// More tools than this from one server are skipped: every request carries
/// every tool's definition.
pub const MAX_TOOLS_PER_SERVER: usize = 100;
/// A longer tool description is cut to this.
pub const MAX_DESCRIPTION_BYTES: usize = 4096;
/// The variables a stdio child inherits from Athena.
const INHERITED_ENV: [&str; 2] = ["PATH", "HOME"];

/// Where `${VAR}` and the inherited variables are read from.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;
/// Where startup problems go.
pub type Warn<'a> = &'a dyn Fn(&str);

/// How to reach one server.
#[derive(Clone, PartialEq, Eq)]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

/// Names only. The values are secrets.
impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names =
            |pairs: &[(String, String)]| pairs.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
        match self {
            Self::Stdio { command, args, env } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", &args.len())
                .field("env", &names(env))
                .finish(),
            Self::Http { url, headers } => f
                .debug_struct("Http")
                .field("url", url)
                .field("headers", &names(headers))
                .finish(),
        }
    }
}

/// One entry of `mcpServers`, after `${VAR}` expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    pub transport: Transport,
    pub tool_timeout: Duration,
    pub startup_timeout: Duration,
}

/// One entry as written. Unknown fields are ignored, so a file written for
/// Claude Code loads. (`disabled: true` is read by [`parse`] first.)
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    r#type: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    timeout_secs: Option<u64>,
    startup_timeout_secs: Option<u64>,
}

/// The servers of a config file, in name order, and a warning for each
/// entry that cannot be used. Fails only when the file is not a JSON object
/// with an `mcpServers` object. Warnings never carry a value from the file
/// or the environment: serde's own messages would echo one.
pub fn parse(text: &str, lookup: Lookup) -> Result<(Vec<Server>, Vec<String>)> {
    let file: Value = serde_json::from_str(text).context("not valid JSON")?;
    let Some(Value::Object(entries)) = file.get("mcpServers") else {
        bail!("no \"mcpServers\" object");
    };
    let sorted: BTreeMap<_, _> = entries.iter().collect();
    let (mut servers, mut warnings) = (Vec::new(), Vec::new());
    for (name, value) in sorted {
        // Claude Desktop and Cline switch a server off this way.
        if value.get("disabled") == Some(&Value::Bool(true)) {
            continue;
        }
        match server(name, value, lookup) {
            Ok(server) => servers.push(server),
            Err(why) => warnings.push(format!("mcp server `{name}` skipped: {why}")),
        }
    }
    Ok((servers, warnings))
}

fn server(name: &str, value: &Value, lookup: Lookup) -> Result<Server, String> {
    let entry = Entry::deserialize(value).map_err(|_| {
        "the entry is malformed: command, url and type are strings, args is a list of \
         strings, env and headers map names to strings, the timeouts are whole seconds"
            .to_string()
    })?;
    if entry.command.is_some() && entry.url.is_some() {
        return Err("it has both a command and a url; use one".into());
    }
    let mut missing = BTreeSet::new();
    let mut expand_all = |texts: Vec<&String>| -> Result<Vec<String>, String> {
        texts
            .into_iter()
            .map(|text| expand(text, lookup, &mut missing))
            .collect()
    };
    let transport = match (entry.r#type.as_deref(), entry.command, entry.url) {
        (Some("sse"), ..) => {
            return Err("the SSE transport is not supported; use a streamable-HTTP url".into());
        }
        (Some(other), ..) if !["stdio", "http", "streamable-http"].contains(&other) => {
            return Err(format!("unsupported type `{other}`"));
        }
        (Some("stdio") | None, Some(command), None) => Transport::Stdio {
            command,
            args: expand_all(entry.args.iter().collect())?,
            env: pairs(&entry.env, &mut expand_all)?,
        },
        (Some("http" | "streamable-http") | None, None, Some(url)) => Transport::Http {
            url: http_url(url)?,
            headers: pairs(&entry.headers, &mut expand_all)?,
        },
        _ => return Err("it needs a command (stdio) or a url (HTTP) matching its type".into()),
    };
    if !missing.is_empty() {
        let names: Vec<_> = missing.into_iter().collect();
        return Err(format!(
            "unset environment variable(s): {}",
            names.join(", ")
        ));
    }
    Ok(Server {
        name: name.into(),
        transport,
        tool_timeout: seconds(entry.timeout_secs, TOOL_TIMEOUT, "timeoutSecs")?,
        startup_timeout: seconds(
            entry.startup_timeout_secs,
            STARTUP_TIMEOUT,
            "startupTimeoutSecs",
        )?,
    })
}

/// `url`, if it is an http or https address.
fn http_url(url: String) -> Result<String, String> {
    match url::Url::parse(&url) {
        Ok(parsed) if ["http", "https"].contains(&parsed.scheme()) => Ok(url),
        _ => Err("the url is not an http or https address".into()),
    }
}

/// `map`'s names with their values expanded.
fn pairs(
    map: &BTreeMap<String, String>,
    expand_all: &mut impl FnMut(Vec<&String>) -> Result<Vec<String>, String>,
) -> Result<Vec<(String, String)>, String> {
    let values = expand_all(map.values().collect())?;
    Ok(map.keys().cloned().zip(values).collect())
}

fn seconds(configured: Option<u64>, default: Duration, field: &str) -> Result<Duration, String> {
    match configured {
        Some(0) => Err(format!("{field} must be at least 1")),
        Some(secs) => Ok(Duration::from_secs(secs)),
        None => Ok(default),
    }
}

/// `text` with each `${VAR}` and `${VAR:-default}` replaced from `lookup`,
/// and `$${` turned into `${`. A default ends at the first `}`. A variable
/// with no value and no default is added to `missing` and replaced by
/// nothing: the caller refuses the server. The error is a syntax problem,
/// described without the text around it.
fn expand(text: &str, lookup: Lookup, missing: &mut BTreeSet<String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        if rest[..start].ends_with('$') {
            out.push_str(&rest[..start - 1]);
            out.push_str("${");
            rest = &rest[start + 2..];
            continue;
        }
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or("a `${` is never closed with `}`")?;
        let (name, default) = match after[..end].split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (&after[..end], None),
        };
        let valid = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err("a `${...}` does not hold a variable name".into());
        }
        if default.is_some_and(|default| default.contains("${")) {
            return Err("a default cannot hold another `${`".into());
        }
        match (lookup(name), default) {
            (Some(value), _) => out.push_str(&value),
            (None, Some(default)) => out.push_str(default),
            (None, None) => {
                missing.insert(name.to_string());
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A server that answered: its connection and the tools it offered.
struct Connected {
    name: String,
    /// Dropping this closes the connection and kills a stdio child.
    service: RunningService<RoleClient, ClientInfo>,
    tools: Vec<Tool>,
    timeout: Duration,
}

/// The connected MCP servers. Keep it alive as long as the agents built
/// from it run, and call [`Mcp::shutdown`] when the process is done.
pub struct Mcp {
    servers: Vec<Connected>,
}

impl Mcp {
    /// No servers: what an agent gets when MCP is not configured.
    pub fn none() -> Self {
        Self {
            servers: Vec::new(),
        }
    }

    /// Connect the servers `ATHENA_MCP_CONFIG` lists. `reserved` are tool
    /// names MCP tools may not take.
    pub async fn from_env(reserved: &[&str], warn: Warn<'_>) -> Self {
        let path = std::env::var(CONFIG_ENV).ok();
        Self::start(path, &|name| std::env::var(name).ok(), reserved, warn).await
    }

    /// [`Mcp::from_env`] with the config path and environment given.
    pub async fn start(
        path: Option<String>,
        lookup: Lookup<'_>,
        reserved: &[&str],
        warn: Warn<'_>,
    ) -> Self {
        let Some(path) = path.filter(|path| !path.is_empty()) else {
            return Self::none();
        };
        let loaded = std::fs::read_to_string(&path)
            .context("cannot read it")
            .and_then(|text| parse(&text, lookup).context("cannot use it"));
        let (servers, warnings) = match loaded {
            Ok(loaded) => loaded,
            Err(e) => {
                warn(&format!("{CONFIG_ENV}={path}: {e:#}; no MCP tools"));
                return Self::none();
            }
        };
        warnings.iter().for_each(|w| warn(w));
        Self::connect(&servers, &inherited_env(lookup), reserved, warn).await
    }

    /// Connect `servers` concurrently and keep the tools that can be used.
    pub async fn connect(
        servers: &[Server],
        env: &[(String, String)],
        reserved: &[&str],
        warn: Warn<'_>,
    ) -> Self {
        let results = join_all(servers.iter().map(|s| connect(s, env))).await;
        let mut taken: HashMap<String, String> = reserved
            .iter()
            .map(|name| (name.to_string(), "a built-in tool".to_string()))
            .collect();
        let mut connected = Vec::new();
        for (server, result) in servers.iter().zip(results) {
            match result {
                Err(why) => warn(&format!(
                    "mcp server `{}` skipped: {}",
                    server.name,
                    hide_urls(&why)
                )),
                Ok(mut server) => {
                    server.tools = admit(&server.name, server.tools, &mut taken, warn);
                    let tools = server.tools.len();
                    tracing::info!(server = server.name, tools, "mcp server connected");
                    connected.push(server);
                }
            }
        }
        Self { servers: connected }
    }

    /// The tools that will be registered, by name.
    pub fn tool_names(&self) -> HashSet<String> {
        self.servers
            .iter()
            .flat_map(|s| s.tools.iter().map(|t| t.name.to_string()))
            .collect()
    }

    /// Add every server's tools to an agent, each call bounded by its
    /// server's timeout.
    pub fn register(
        &self,
        builder: AgentBuilder<WithBuilderTools>,
    ) -> AgentBuilder<WithBuilderTools> {
        self.servers.iter().fold(builder, |builder, server| {
            builder.rmcp_tools_with_timeout(
                server.tools.clone(),
                server.service.peer().clone(),
                server.timeout,
            )
        })
    }

    /// Close every connection. A stdio server gets a few seconds to exit on
    /// its own, then is killed; none is left running or unreaped.
    pub async fn shutdown(self) {
        join_all(self.servers.into_iter().map(|mut server| async move {
            // Nothing to do about a server that will not close: it is
            // killed when `server` drops, which is also how a crash of
            // Athena ends it.
            let _ = server.service.close_with_timeout(SHUTDOWN_TIMEOUT).await;
        }))
        .await;
    }
}

static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(https?://)(?:[^/@\s]*@)?([^/\s)?#'"]+)[^\s)'"]*"#).expect("a valid pattern")
});

/// `message` with each URL cut to its scheme, host and port. An HTTP error
/// names the URL it was about, and a URL may carry a key in its path or query.
fn hide_urls(message: &str) -> String {
    URL.replace_all(message, "$1$2").into_owned()
}

/// What a stdio child inherits from Athena.
fn inherited_env(lookup: Lookup) -> Vec<(String, String)> {
    INHERITED_ENV
        .iter()
        .filter_map(|name| Some((name.to_string(), lookup(name)?)))
        .collect()
}

/// The child process for a stdio server: its declared `env` over `inherited`,
/// and nothing else.
fn command(
    program: &str,
    args: &[String],
    env: &[(String, String)],
    inherited: &[(String, String)],
) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(inherited.iter().cloned())
        .envs(env.iter().cloned())
        // Also when Athena unwinds before the connection can close it.
        .kill_on_drop(true);
    command
}

/// Start, handshake and list one server, within its startup timeout.
async fn connect(server: &Server, inherited: &[(String, String)]) -> Result<Connected, String> {
    let work = async {
        let info = ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("athena", env!("CARGO_PKG_VERSION")),
        );
        let service = match &server.transport {
            Transport::Stdio {
                command: program,
                args,
                env,
            } => {
                let child = TokioChildProcess::new(command(program, args, env, inherited))
                    .map_err(|e| format!("cannot start `{program}`: {e}"))?;
                info.serve(child).await
            }
            Transport::Http { url, headers } => {
                let config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                    .custom_headers(header_map(headers)?);
                info.serve(StreamableHttpClientTransport::from_config(config))
                    .await
            }
        }
        .map_err(|e| format!("the handshake failed: {e}"))?;
        let tools = service
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| format!("listing its tools failed: {e}"))?;
        Ok(Connected {
            name: server.name.clone(),
            service,
            tools,
            timeout: server.tool_timeout,
        })
    };
    tokio::time::timeout(server.startup_timeout, work)
        .await
        .map_err(|_| {
            format!(
                "it did not start within {} s",
                server.startup_timeout.as_secs()
            )
        })?
}

fn header_map(headers: &[(String, String)]) -> Result<HashMap<HeaderName, HeaderValue>, String> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("`{name}` is not a valid header name"))?;
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| format!("the value of header `{name}` is not valid"))?;
            value.set_sensitive(true);
            Ok((name, value))
        })
        .collect()
}

/// The tools of `server` that may be registered. A name already in `taken`
/// (which this adds the rest to), a name or schema a provider would refuse,
/// and tools past [`MAX_TOOLS_PER_SERVER`] are skipped with a warning.
fn admit(
    server: &str,
    tools: Vec<Tool>,
    taken: &mut HashMap<String, String>,
    warn: Warn<'_>,
) -> Vec<Tool> {
    let mut kept = Vec::new();
    for mut tool in tools {
        let name = tool.name.to_string();
        let allowed = (1..=MAX_TOOL_NAME_BYTES).contains(&name.len())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !allowed {
            warn(&format!(
                "mcp server `{server}`: tool `{name}` skipped: model providers accept only \
                 1 to {MAX_TOOL_NAME_BYTES} letters, digits, `_` and `-` in a tool name"
            ));
        } else if tool.input_schema.get("type") != Some(&Value::from("object")) {
            warn(&format!(
                "mcp server `{server}`: tool `{name}` skipped: its input schema is not \
                 of type object"
            ));
        } else if let Some(owner) = taken.get(&name) {
            warn(&format!(
                "mcp server `{server}`: tool `{name}` skipped: the name belongs to {owner}"
            ));
        } else if kept.len() == MAX_TOOLS_PER_SERVER {
            warn(&format!(
                "mcp server `{server}`: tool `{name}` skipped: past its first \
                 {MAX_TOOLS_PER_SERVER} tools"
            ));
        } else {
            if let Some(description) = &tool.description {
                let (head, left_out) = split_at_boundary(description, MAX_DESCRIPTION_BYTES);
                if left_out > 0 {
                    tool.description = Some(format!("{head}...").into());
                }
            }
            taken.insert(name, format!("server `{server}`"));
            kept.push(tool);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `assert!` with what a failure should show, kept on one line so that
    /// the coverage gate counts the message as run.
    fn ensure(ok: bool, shown: &str) {
        assert!(ok, "{shown}");
    }

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn expanded(text: &str, vars: &[(&str, &str)]) -> (Result<String, String>, Vec<String>) {
        let mut missing = BTreeSet::new();
        let result = expand(text, &env(vars), &mut missing);
        (result, missing.into_iter().collect())
    }

    fn tool(name: &'static str) -> Tool {
        let schema = serde_json::json!({"type": "object"});
        Tool::new(name, "", schema.as_object().unwrap().clone())
    }

    fn parsed(text: &str, vars: &[(&str, &str)]) -> (Vec<Server>, Vec<String>) {
        parse(text, &env(vars)).unwrap()
    }

    // ---- ${VAR} ----

    #[test]
    fn text_without_a_variable_is_unchanged() {
        assert_eq!(
            expanded("plain $HOME {x}", &[]),
            (Ok("plain $HOME {x}".into()), vec![])
        );
    }

    #[test]
    fn variables_are_replaced_wherever_they_stand() {
        let vars = [("A", "1"), ("B_2", "two")];
        assert_eq!(
            expanded("x${A}y${B_2}${A}", &vars),
            (Ok("x1ytwo1".into()), vec![])
        );
    }

    #[test]
    fn an_unset_variable_is_reported_by_name_and_a_default_fills_in() {
        let (result, missing) = expanded("${NOPE}-${ALSO}-${NOPE}", &[]);
        assert_eq!(result, Ok("--".into()));
        assert_eq!(missing, ["ALSO", "NOPE"]);
        assert_eq!(
            expanded("${NOPE:-fallback}|${SET:-unused}", &[("SET", "v")]),
            (Ok("fallback|v".into()), vec![])
        );
        // Set but empty is a value, not a hole.
        assert_eq!(expanded("[${E}]", &[("E", "")]), (Ok("[]".into()), vec![]));
    }

    #[test]
    fn a_doubled_dollar_is_a_literal_reference_that_is_not_expanded() {
        let vars = [("A", "1")];
        assert_eq!(
            expanded("sh -c 'echo $${A}' ${A}", &vars),
            (Ok("sh -c 'echo ${A}' 1".into()), vec![])
        );
        // Other dollar signs are text.
        assert_eq!(expanded("$ $$ $x", &vars), (Ok("$ $$ $x".into()), vec![]));
    }

    #[test]
    fn a_malformed_reference_is_an_error_that_quotes_nothing() {
        for bad in ["${A", "${}", "${1A}", "${A B}", "${:-x}"] {
            let (result, _) = expanded(&format!("secret-prefix{bad}"), &[("A", "v")]);
            let why = result.unwrap_err();
            assert!(!why.contains("secret"), "{bad}: {why}");
        }
    }

    // ---- the config file ----

    #[test]
    fn both_kinds_of_server_load_with_their_timeouts() {
        let (servers, warnings) = parsed(
            r#"{"mcpServers": {
                "wiki": {"url": "https://w.example/mcp", "headers": {"Authorization": "Bearer ${TOK}"},
                         "startupTimeoutSecs": 5},
                "files": {"command": "srv", "args": ["--k", "${TOK}"], "env": {"A": "${TOK}", "B": "b"},
                          "timeoutSecs": 7, "disabled": false}
            }}"#,
            &[("TOK", "s3cret")],
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        // Name order, whatever the file's.
        assert_eq!(
            servers.iter().map(|s| &s.name[..]).collect::<Vec<_>>(),
            ["files", "wiki"]
        );
        assert_eq!(
            servers[0],
            Server {
                name: "files".into(),
                transport: Transport::Stdio {
                    command: "srv".into(),
                    args: vec!["--k".into(), "s3cret".into()],
                    env: vec![("A".into(), "s3cret".into()), ("B".into(), "b".into())],
                },
                tool_timeout: Duration::from_secs(7),
                startup_timeout: STARTUP_TIMEOUT,
            }
        );
        assert_eq!(
            servers[1],
            Server {
                name: "wiki".into(),
                transport: Transport::Http {
                    url: "https://w.example/mcp".into(),
                    headers: vec![("Authorization".into(), "Bearer s3cret".into())],
                },
                tool_timeout: TOOL_TIMEOUT,
                startup_timeout: Duration::from_secs(5),
            }
        );
    }

    #[test]
    fn a_type_field_is_honoured_where_it_names_a_supported_transport() {
        let (servers, warnings) = parsed(
            r#"{"mcpServers": {
                "a": {"type": "stdio", "command": "x"},
                "b": {"type": "http", "url": "http://b"},
                "c": {"type": "streamable-http", "url": "http://c"}
            }}"#,
            &[],
        );
        assert_eq!((servers.len(), warnings.len()), (3, 0));
    }

    #[test]
    fn an_entry_that_cannot_be_used_is_a_warning_and_the_rest_load() {
        let (servers, warnings) = parsed(
            r#"{"mcpServers": {
                "good": {"command": "x"},
                "unset": {"command": "x", "env": {"T": "${MISSING_ONE}"}, "args": ["${MISSING_TWO}"]},
                "sse": {"type": "sse", "url": "http://s"},
                "weird": {"type": "carrier-pigeon", "command": "x"},
                "both": {"command": "x", "url": "http://u"},
                "neither": {},
                "mismatch": {"type": "http", "command": "x"},
                "mismatch2": {"type": "stdio", "url": "http://u"},
                "zero": {"command": "x", "timeoutSecs": 0},
                "zero2": {"command": "x", "startupTimeoutSecs": 0},
                "broken": {"command": "TOPSECRET", "args": "not-a-list"},
                "syntax": {"command": "x", "args": ["${oops"]},
                "nested": {"command": "x", "args": ["${A:-${B}}"]},
                "ftp": {"url": "ftp://files.example/mcp"},
                "nourl": {"url": "wiki"},
                "off": {"command": "x", "disabled": true},
                "on": {"command": "x", "disabled": false},
                "notobject": 7
            }}"#,
            &[],
        );
        // The disabled one is left out without a word.
        let loaded: Vec<_> = servers.iter().map(|s| &s.name[..]).collect();
        assert_eq!(loaded, ["good", "on"]);
        let all = warnings.join("\n");
        for needle in [
            "`unset` skipped: unset environment variable(s): MISSING_ONE, MISSING_TWO",
            "`sse` skipped: the SSE transport",
            "`weird` skipped: unsupported type `carrier-pigeon`",
            "`both` skipped: it has both",
            "`neither` skipped: it needs a command",
            "`mismatch` skipped: it needs a command",
            "`mismatch2` skipped: it needs a command",
            "`zero` skipped: timeoutSecs must be at least 1",
            "`zero2` skipped: startupTimeoutSecs must be at least 1",
            "`broken` skipped: the entry is malformed",
            "`syntax` skipped: a `${` is never closed",
            "`nested` skipped: a default cannot hold another `${`",
            "`ftp` skipped: the url is not an http or https address",
            "`nourl` skipped: the url is not an http or https address",
            "`notobject` skipped: the entry is malformed",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in\n{all}");
        }
        assert_eq!(warnings.len(), 15);
        assert!(!all.contains("`off`"));
        ensure(!all.contains("TOPSECRET"), &all);
        ensure(!all.contains("not-a-list"), &all);
    }

    #[test]
    fn a_file_that_is_not_a_config_is_an_error() {
        for text in ["", "not json", "[]", "{}", r#"{"mcpServers": []}"#] {
            assert!(parse(text, &env(&[])).is_err(), "{text}");
        }
        assert_eq!(parsed(r#"{"mcpServers": {}}"#, &[]), (vec![], vec![]));
    }

    #[test]
    fn debug_output_shows_names_and_never_values() {
        let (servers, _) = parsed(
            r#"{"mcpServers": {
                "a": {"command": "srv", "args": ["--token=ARGSECRET"], "env": {"K": "${S}"}},
                "b": {"url": "http://b", "headers": {"Authorization": "${S}"}}
            }}"#,
            &[("S", "VALSECRET")],
        );
        let shown = format!("{servers:?}");
        ensure(!shown.contains("VALSECRET"), &shown);
        ensure(!shown.contains("ARGSECRET"), &shown);
        assert!(shown.contains(r#"env: ["K"]"#), "{shown}");
        assert!(shown.contains(r#"headers: ["Authorization"]"#), "{shown}");
    }

    // ---- startup without servers ----

    #[tokio::test]
    async fn no_path_and_an_empty_path_mean_no_tools_and_no_noise() {
        let seen = Mutex::new(Vec::<String>::new());
        let warn = |w: &str| seen.lock().unwrap().push(w.to_string());
        for path in [None, Some(String::new())] {
            let mcp = Mcp::start(path, &env(&[]), &[], &warn).await;
            assert!(mcp.tool_names().is_empty());
            mcp.shutdown().await;
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unreadable_or_invalid_file_is_one_warning_and_no_tools() {
        let dir = std::env::temp_dir().join(format!("athena-mcp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{ not json").unwrap();
        let seen = Mutex::new(Vec::<String>::new());
        let warn = |w: &str| seen.lock().unwrap().push(w.to_string());
        for (path, why) in [
            (dir.join("missing.json"), "cannot read it"),
            (bad, "cannot use it"),
        ] {
            let mcp = Mcp::start(Some(path.display().to_string()), &env(&[]), &[], &warn).await;
            assert!(mcp.tool_names().is_empty());
            let last = seen.lock().unwrap().pop().unwrap();
            assert!(last.starts_with(CONFIG_ENV) && last.contains(why), "{last}");
            assert!(last.ends_with("no MCP tools"), "{last}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---- the child process ----

    #[test]
    fn a_child_inherits_only_path_and_home() {
        let lookup = env(&[
            ("PATH", "/bin"),
            ("HOME", "/h"),
            ("OPENROUTER_API_KEY", "k"),
        ]);
        assert_eq!(
            inherited_env(&lookup),
            [("PATH".into(), "/bin".into()), ("HOME".into(), "/h".into())]
        );
        assert_eq!(
            inherited_env(&env(&[("PATH", "/bin")])),
            [("PATH".into(), "/bin".into())]
        );
    }

    // ---- names ----

    #[test]
    fn names_that_are_taken_or_that_a_provider_would_refuse_are_skipped_with_a_warning() {
        let long: &'static str = Box::leak("x".repeat(65).into_boxed_str());
        let seen = Mutex::new(Vec::<String>::new());
        let warn = |w: &str| seen.lock().unwrap().push(w.to_string());
        let mut taken = HashMap::from([("shell".to_string(), "a built-in tool".to_string())]);

        let first = admit(
            "a",
            vec![
                tool("search"),
                tool("shell"),
                tool("dotted.name"),
                tool(""),
                tool(long),
            ],
            &mut taken,
            &warn,
        );
        let second = admit(
            "b",
            vec![tool("search"), tool("other"), tool("with space")],
            &mut taken,
            &warn,
        );

        let names = |tools: &[Tool]| tools.iter().map(|t| t.name.to_string()).collect::<Vec<_>>();
        assert_eq!(names(&first), ["search"]);
        assert_eq!(names(&second), ["other"]);
        let seen = seen.lock().unwrap().join("\n");
        let shell = "`a`: tool `shell` skipped: the name belongs to a built-in tool";
        let search = "`b`: tool `search` skipped: the name belongs to server `a`";
        ensure(seen.contains(shell), &seen);
        ensure(seen.contains(search), &seen);
        assert_eq!(seen.matches("model providers accept only").count(), 4);
        assert_eq!(seen.lines().count(), 6);

        let max = "y".repeat(MAX_TOOL_NAME_BYTES);
        let edge: &'static str = Box::leak(max.into_boxed_str());
        assert_eq!(admit("c", vec![tool(edge)], &mut taken, &|_| ()).len(), 1);
    }

    #[test]
    fn a_schema_that_is_not_an_object_a_flood_of_tools_and_a_long_description_are_handled() {
        let seen = Mutex::new(Vec::<String>::new());
        let warn = |w: &str| seen.lock().unwrap().push(w.to_string());
        let mut taken = HashMap::new();

        let mut no_type = tool("no_type");
        no_type.input_schema = Default::default();
        let mut string_type = tool("string_type");
        string_type.input_schema = std::sync::Arc::new(
            serde_json::json!({"type": "string"})
                .as_object()
                .unwrap()
                .clone(),
        );
        let kept = admit(
            "s",
            vec![no_type, string_type, tool("fine")],
            &mut taken,
            &warn,
        );
        assert_eq!(kept.len(), 1);
        let said = seen.lock().unwrap().join("\n");
        ensure(
            said.contains("`no_type` skipped: its input schema is not of type object"),
            &said,
        );
        ensure(said.contains("`string_type` skipped"), &said);

        seen.lock().unwrap().clear();
        let many: Vec<Tool> = (0..MAX_TOOLS_PER_SERVER + 2)
            .map(|n| tool(Box::leak(format!("t{n}").into_boxed_str())))
            .collect();
        let kept = admit("big", many, &mut taken, &warn);
        assert_eq!(kept.len(), MAX_TOOLS_PER_SERVER);
        let said = seen.lock().unwrap().join("\n");
        assert_eq!(said.lines().count(), 2);
        ensure(
            said.contains("tool `t100` skipped: past its first 100 tools"),
            &said,
        );

        let mut wordy = tool("wordy");
        wordy.description = Some("é".repeat(MAX_DESCRIPTION_BYTES).into());
        let mut brief = tool("brief");
        brief.description = Some("short".into());
        let kept = admit("d", vec![wordy, brief], &mut taken, &warn);
        let wordy = kept[0].description.as_deref().unwrap();
        assert!(wordy.len() <= MAX_DESCRIPTION_BYTES + 3 && wordy.ends_with("é..."));
        assert_eq!(kept[1].description.as_deref(), Some("short"));
    }

    #[test]
    fn a_url_in_a_message_is_cut_to_its_host() {
        let shown = hide_urls(
            "error sending request for url (https://user:pw@mcp.example:8443/v1/KEY123/mcp?token=Q1#f): \
             refused, then http://localhost/x and plain text",
        );
        assert_eq!(
            shown,
            "error sending request for url (https://mcp.example:8443): refused, then http://localhost and plain text"
        );
        assert_eq!(hide_urls("no url here"), "no url here");
    }

    #[test]
    fn header_names_and_values_are_checked_and_marked_sensitive() {
        let map = header_map(&[("X-Key".into(), "v".into())]).unwrap();
        let (name, value) = map.iter().next().unwrap();
        assert_eq!(name.as_str(), "x-key");
        assert!(value.is_sensitive());
        assert!(
            header_map(&[("bad name".into(), "v".into())])
                .unwrap_err()
                .contains("not a valid header name")
        );
        let why = header_map(&[("ok".into(), "line\nbreak".into())]).unwrap_err();
        ensure(why.contains("header `ok`"), &why);
        ensure(!why.contains("break"), &why);
    }
}

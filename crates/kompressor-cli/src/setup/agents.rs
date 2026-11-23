//! Durable wiring for each agent: what `setup` writes, and how it is undone.
//!
//! Every edit is computed as a whole-file change first — the file before, the
//! file after — so the plan can be shown before anything is touched and a dry
//! run is the same code path minus the write. What is needed to undo it goes
//! into the manifest ([`Record`]): for a JSON file, each key's value before
//! the first `setup` touched it; for Codex's TOML, nothing but the markers,
//! because the block and the lines it disabled are self-describing.
//!
//! The JSON edits restore a key only while it still holds the value `setup`
//! wrote. A value the user changed since is theirs, and is left alone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::CliError;
use crate::wrap::configfile::{self, CodexAuth, Markers, Target};
use crate::wrap::opencode;

/// The agents `setup` knows how to wire.
pub const AGENTS: &[&str] = &["claude", "codex", "opencode"];

/// Codex's block, distinct from `wrap`'s so neither strips the other's.
pub const CODEX_MARKERS: Markers = Markers {
    begin: "# --- kompressor setup: managed block, removed by `kompressor setup remove` ---",
    end: "# --- end kompressor setup ---",
    disabled: "# kompressor setup disabled this line: ",
};

/// How Claude Code is pointed at the proxy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClaudeMode {
    /// `HTTPS_PROXY` at the intercept listener and its CA in
    /// `NODE_EXTRA_CA_CERTS`. The only route that also reaches Claude
    /// Desktop's Code tab, which overwrites `ANTHROPIC_BASE_URL`.
    Intercept,
    /// `ANTHROPIC_BASE_URL` at the proxy. No CA, but the CLI only.
    BaseUrl,
}

/// The addresses the agents are pointed at, taken from the service's
/// effective configuration.
#[derive(Clone, Debug)]
pub struct Endpoints {
    /// The proxy's port on loopback.
    pub port: u16,
    /// The intercept listener (`host:port`) and its CA certificate, when the
    /// configuration runs one.
    pub intercept: Option<(String, PathBuf)>,
}

/// One JSON key `setup` set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonKey {
    /// The key's path, as a list of object keys.
    pub path: Vec<String>,
    /// What was there before the first `setup` run; `None` when absent.
    pub before: Option<Value>,
    /// What `setup` wrote.
    pub after: Value,
}

/// What it takes to undo one agent's wiring.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The file edited.
    pub file: PathBuf,
    /// Whether the file existed before `setup` first wrote it.
    pub existed: bool,
    /// JSON keys set, for JSON files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<JsonKey>,
    /// Whether a marker-delimited TOML block was written.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub block: bool,
    /// Whether the retrieval MCP server was registered with Claude Code.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mcp: bool,
    /// Claude's mode, for status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ClaudeMode>,
    /// A JSON file's text before the first `setup` run, so `remove` can put it
    /// back byte for byte — formatting included — when nothing else changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<String>,
}

/// The text to remember as the original: the first run's, kept across runs.
fn original_of(before: &Option<String>, previous: Option<&Record>) -> Option<String> {
    match previous {
        Some(record) => record.original.clone(),
        None => before.clone(),
    }
}

/// One agent's planned change.
#[derive(Debug)]
pub struct Plan {
    pub agent: &'static str,
    pub file: PathBuf,
    pub before: Option<String>,
    pub after: String,
    /// One line per decision, for the printed plan.
    pub notes: Vec<String>,
    pub record: Record,
    /// Run after the file is written: `opencode reload`, `claude mcp add`.
    pub post: Post,
}

/// What has to happen once the file is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Post {
    None,
    OpenCodeReload,
    ClaudeMcp,
}

impl Plan {
    /// Whether the file would change.
    pub fn changes(&self) -> bool {
        self.before.as_deref() != Some(self.after.as_str())
    }
}

/// Whether an agent's binary is on `PATH`.
pub fn installed(agent: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let names: Vec<String> = if cfg!(windows) {
        ["exe", "cmd", "bat"]
            .iter()
            .map(|ext| format!("{agent}.{ext}"))
            .collect()
    } else {
        vec![agent.to_string()]
    };
    std::env::split_paths(&path).any(|dir| names.iter().any(|name| dir.join(name).is_file()))
}

/// `<agent> --version`, first line, if it answers.
pub fn version(agent: &str) -> Option<String> {
    let output = std::process::Command::new(agent)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
}

fn read_text(path: &Path) -> Result<Option<String>, CliError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CliError::runtime(
            &format!("could not read {}", path.display()),
            e,
        )),
    }
}

// --- JSON key editing --------------------------------------------------------

fn get<'a>(map: &'a Map<String, Value>, path: &[String]) -> Option<&'a Value> {
    let (last, parents) = path.split_last()?;
    let mut current = map;
    for part in parents {
        current = current.get(part)?.as_object()?;
    }
    current.get(last)
}

/// Set, or with `None` remove, the value at `path`, pruning objects a removal
/// leaves empty.
fn set(map: &mut Map<String, Value>, path: &[String], value: Option<Value>) {
    let [head, rest @ ..] = path else { return };
    if rest.is_empty() {
        match value {
            Some(value) => {
                map.insert(head.clone(), value);
            }
            None => {
                map.remove(head);
            }
        }
        return;
    }
    if value.is_none() && !map.contains_key(head) {
        return;
    }
    let child = map
        .entry(head.clone())
        .or_insert_with(|| Value::Object(Map::new()));
    if !child.is_object() {
        if value.is_none() {
            return;
        }
        *child = Value::Object(Map::new());
    }
    if let Some(inner) = child.as_object_mut() {
        set(inner, rest, value);
        if inner.is_empty() {
            map.remove(head);
        }
    }
}

fn path_of(dotted: &str) -> Vec<String> {
    dotted.split('.').map(str::to_owned).collect()
}

fn render_json(map: &Map<String, Value>) -> Result<String, CliError> {
    serde_json::to_string_pretty(&Value::Object(map.clone()))
        .map(|text| text + "\n")
        .map_err(|e| CliError::runtime("could not render JSON", e))
}

/// Set `wanted` keys in `map`, recording what each held before. A key a
/// previous run already set keeps that run's `before`: the true original.
fn set_keys(
    map: &mut Map<String, Value>,
    wanted: Vec<(Vec<String>, Value)>,
    previous: Option<&Record>,
) -> Vec<JsonKey> {
    wanted
        .into_iter()
        .map(|(path, after)| {
            let before = previous
                .and_then(|record| record.keys.iter().find(|key| key.path == path))
                .map(|key| key.before.clone())
                .unwrap_or_else(|| get(map, &path).cloned());
            set(map, &path, Some(after.clone()));
            JsonKey {
                path,
                before,
                after,
            }
        })
        .collect()
}

/// Undo [`set_keys`]: each key still holding what `setup` wrote goes back to
/// what it held before. Returns the keys left alone because they changed.
fn restore_keys(map: &mut Map<String, Value>, keys: &[JsonKey]) -> Vec<String> {
    let mut changed = Vec::new();
    for key in keys {
        if get(map, &key.path) == Some(&key.after) {
            set(map, &key.path, key.before.clone());
        } else {
            changed.push(key.path.join("."));
        }
    }
    changed
}

/// A previous run's keys this run no longer sets (a mode switched from
/// intercept to base URL, say), put back before the new ones go in.
fn drop_stale(map: &mut Map<String, Value>, previous: Option<&Record>, keep: &[Vec<String>]) {
    if let Some(previous) = previous {
        let stale: Vec<JsonKey> = previous
            .keys
            .iter()
            .filter(|key| !keep.contains(&key.path))
            .cloned()
            .collect();
        restore_keys(map, &stale);
    }
}

// --- Claude Code ---------------------------------------------------------------

/// Claude Code's user settings: `$CLAUDE_CONFIG_DIR/settings.json`, else
/// `~/.claude/settings.json`.
pub fn claude_settings() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| super::service::home().join(".claude"))
        .join("settings.json")
}

/// The `env` entries Claude Code is given in `mode`.
pub fn claude_env(
    mode: ClaudeMode,
    endpoints: &Endpoints,
) -> Result<Vec<(&'static str, String)>, String> {
    match mode {
        ClaudeMode::Intercept => {
            let (listen, ca) = endpoints.intercept.as_ref().ok_or(
                "the service runs no intercept listener ([proxy.intercept] listen); use \
                 --claude-mode base-url, or name an address there",
            )?;
            Ok(vec![
                ("HTTPS_PROXY", format!("http://{listen}")),
                ("NODE_EXTRA_CA_CERTS", ca.display().to_string()),
            ])
        }
        ClaudeMode::BaseUrl => Ok(vec![
            (
                "ANTHROPIC_BASE_URL",
                format!("http://127.0.0.1:{}", endpoints.port),
            ),
            // A custom base URL turns off Claude Code's deferred tool loading;
            // this asks for it back (see wrap's catalogue entry for claude).
            ("ENABLE_TOOL_SEARCH", "true".into()),
        ]),
    }
}

pub fn plan_claude(
    mode: ClaudeMode,
    with_mcp: bool,
    force: bool,
    endpoints: &Endpoints,
    previous: Option<&Record>,
) -> Result<Plan, CliError> {
    let file = claude_settings();
    let before = read_text(&file)?;
    let mut map = crate::wrap::settings::read_json(&file)?;
    let env = claude_env(mode, endpoints).map_err(CliError::Usage)?;
    let mut notes = Vec::new();

    let keep: Vec<Vec<String>> = env
        .iter()
        .map(|(name, _)| vec!["env".to_string(), name.to_string()])
        .collect();
    drop_stale(&mut map, previous, &keep);

    for (name, value) in &env {
        let path = vec!["env".to_string(), name.to_string()];
        let ours = previous.is_some_and(|record| record.keys.iter().any(|key| key.path == path));
        if let Some(current) = get(&map, &path).and_then(Value::as_str)
            && current != value
            && !ours
            && !force
        {
            return Err(CliError::Usage(format!(
                "{} already sets env.{name} = {current:?}. Claude Code takes one value, and \
                 replacing it could cut off a proxy or CA you rely on. Remove it, or pass \
                 --force to replace it (the old value comes back on `setup remove`).",
                file.display()
            )));
        }
    }
    if mode == ClaudeMode::BaseUrl {
        notes.push(
            "base-url mode reaches the CLI only: Claude Desktop's Code tab overrides \
             ANTHROPIC_BASE_URL"
                .into(),
        );
    } else {
        notes.push(
            "HTTPS_PROXY also reaches every command Claude runs; hosts other than \
             api.anthropic.com are tunnelled unread, but stop the service only after \
             `setup remove`"
                .into(),
        );
    }
    let wanted = env
        .into_iter()
        .map(|(name, value)| {
            notes.push(format!("env.{name} = {value}"));
            (
                vec!["env".to_string(), name.to_string()],
                Value::String(value),
            )
        })
        .collect();
    let keys = set_keys(&mut map, wanted, previous);
    if with_mcp {
        notes.push(format!(
            "register MCP server `{}` (user scope) for ref:k_… retrieval outside the proxy",
            crate::wrap::mcp::SERVER_NAME
        ));
    }
    Ok(Plan {
        agent: "claude",
        after: render_json(&map)?,
        record: Record {
            file: file.clone(),
            existed: previous.map_or(before.is_some(), |record| record.existed),
            keys,
            block: false,
            mcp: with_mcp,
            mode: Some(mode),
            original: original_of(&before, previous),
        },
        file,
        before,
        notes,
        post: if with_mcp {
            Post::ClaudeMcp
        } else {
            Post::None
        },
    })
}

/// `claude mcp add --scope user` for the retrieval tools, replacing any
/// earlier registration of the same name.
pub fn register_claude_mcp(config: &Path) -> Result<(), String> {
    let binary = std::env::current_exe().map_err(|e| e.to_string())?;
    let _ = claude(&[
        "mcp",
        "remove",
        "--scope",
        "user",
        crate::wrap::mcp::SERVER_NAME,
    ]);
    claude(&[
        "mcp",
        "add",
        "--scope",
        "user",
        crate::wrap::mcp::SERVER_NAME,
        "--",
        &binary.display().to_string(),
        "mcp",
        "serve",
        "--config",
        &config.display().to_string(),
    ])
}

pub fn unregister_claude_mcp() -> Result<(), String> {
    claude(&[
        "mcp",
        "remove",
        "--scope",
        "user",
        crate::wrap::mcp::SERVER_NAME,
    ])
}

fn claude(args: &[&str]) -> Result<(), String> {
    let output = std::process::Command::new("claude")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run claude: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let said = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "`claude {}` failed: {}",
            args.join(" "),
            said.lines().next().unwrap_or("").trim()
        ))
    }
}

// --- Codex -----------------------------------------------------------------------

/// The proxy URL Codex is given on a ChatGPT plan: the mount Codex Desktop
/// calls, which forwards to chatgpt.com with the plan's token.
pub fn codex_subscription_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/backend-api/codex")
}

/// The block for this install's login.
pub fn codex_target(port: u16, home: &Path, auth: CodexAuth) -> Target {
    match auth {
        // Observed on codex 0.166.0 with a ChatGPT login: `openai_base_url` in
        // config.toml pointed at the `/backend-api/codex` mount keeps the
        // plan's token, and the traffic reaches chatgpt.com through the proxy
        // (the desktop app reads the same file). The `/v1` mount in the file
        // is what made 0.164.0 send an API key instead.
        CodexAuth::Subscription => Target {
            path: home.join("config.toml"),
            head: vec![(
                "openai_base_url".into(),
                configfile::quoted(&codex_subscription_url(port)),
            )],
            tables: Vec::new(),
        },
        CodexAuth::ApiKey | CodexAuth::StoredApiKey => Target::codex_for(port, home, auth),
    }
}

pub fn plan_codex(endpoints: &Endpoints, previous: Option<&Record>) -> Result<Plan, CliError> {
    let home = configfile::codex_home();
    let auth = configfile::codex_auth(&home);
    let target = codex_target(endpoints.port, &home, auth);
    let before = read_text(&target.path)?;
    let current = before.clone().unwrap_or_default();
    // A file that already says, in the user's own words, exactly what the
    // block would is left as it is: there is nothing for setup to own, and so
    // nothing for `remove` to take away from them.
    let outside = configfile::clean_with(&current, &CODEX_MARKERS);
    let wanted = configfile::inject_with(&target, "", &CODEX_MARKERS);
    if let (Ok(have), Ok(want)) = (
        outside.parse::<toml::Table>(),
        wanted.parse::<toml::Table>(),
    ) && contains(&have, &want)
    {
        return Ok(Plan {
            agent: "codex",
            record: Record {
                file: target.path.clone(),
                existed: true,
                block: false,
                ..Record::default()
            },
            file: target.path,
            before,
            after: outside,
            notes: vec!["already points at the proxy in your own settings; left as it is".into()],
            post: Post::None,
        });
    }
    let after = configfile::inject_with(&target, &current, &CODEX_MARKERS);
    if after.parse::<toml::Table>().is_err() {
        return Err(CliError::Usage(format!(
            "{} would not be valid TOML after the edit (is it valid now?); left alone",
            target.path.display()
        )));
    }
    let mut notes = vec![format!(
        "login: {}",
        match auth {
            CodexAuth::Subscription => "ChatGPT plan",
            CodexAuth::StoredApiKey => "API key stored by `codex login`",
            CodexAuth::ApiKey => "API key in OPENAI_API_KEY",
        }
    )];
    for (key, value) in &target.head {
        notes.push(format!("{key} = {value}"));
    }
    for table in &target.tables {
        notes.push(format!("[{}]", table.name));
    }
    if current.contains("openai_base_url") && !current.contains(CODEX_MARKERS.begin) {
        notes.push(
            "your own top-level settings of the same keys are commented out, and come back \
             on `setup remove`"
                .into(),
        );
    }
    Ok(Plan {
        agent: "codex",
        record: Record {
            file: target.path.clone(),
            existed: previous.map_or(before.is_some(), |record| record.existed),
            block: true,
            ..Record::default()
        },
        file: target.path,
        before,
        after,
        notes,
        post: Post::None,
    })
}

/// Whether every key of `want`, at any depth, is in `have` with the same value.
fn contains(have: &toml::Table, want: &toml::Table) -> bool {
    want.iter()
        .all(|(key, value)| match (have.get(key), value) {
            (Some(toml::Value::Table(have)), toml::Value::Table(want)) => contains(have, want),
            (Some(have), want) => have == want,
            (None, _) => false,
        })
}

// --- opencode ----------------------------------------------------------------

/// The global opencode config `setup` edits: `opencode.jsonc` when it exists
/// (opencode reads it second, so it wins), else `opencode.json`.
pub fn opencode_file() -> PathBuf {
    let dir = opencode::config_dir();
    let jsonc = dir.join("opencode.jsonc");
    if jsonc.is_file() {
        jsonc
    } else {
        dir.join("opencode.json")
    }
}

pub fn plan_opencode(endpoints: &Endpoints, previous: Option<&Record>) -> Result<Plan, CliError> {
    let file = opencode_file();
    let (before, mut map) = opencode::read_for_edit(&file)?;
    let mut notes = Vec::new();
    let mut wanted = Vec::new();
    for (id, path) in opencode::PROVIDERS {
        let dotted = opencode::setting(id, "baseURL");
        let key = path_of(&dotted);
        let ours = previous.is_some_and(|record| record.keys.iter().any(|k| k.path == key));
        if let Some(url) = get(&map, &key).and_then(Value::as_str)
            && !ours
            && !opencode::is_ours(id, url)
        {
            notes.push(format!("{id}: left alone, already routed to {url}"));
            continue;
        }
        let url = opencode::base_url(endpoints.port, path);
        notes.push(format!("{id}: baseURL = {url}"));
        wanted.push((key, Value::String(url)));
        if *id == "openai" {
            let transport = path_of(&opencode::setting(id, "transport"));
            let mine =
                previous.is_some_and(|record| record.keys.iter().any(|k| k.path == transport));
            if mine || get(&map, &transport).is_none() {
                wanted.push((transport, Value::String(opencode::OPENAI_TRANSPORT.into())));
            }
        }
    }
    let keep: Vec<Vec<String>> = wanted.iter().map(|(path, _)| path.clone()).collect();
    drop_stale(&mut map, previous, &keep);
    let keys = set_keys(&mut map, wanted, previous);
    if let Some(version) = version("opencode")
        && older_than(&version, (2, 2, 4))
    {
        notes.push(format!(
            "opencode {version} is older than 2.2.4, which does not reliably honour baseURL \
             (docs/OPENCODE.md)"
        ));
    }
    Ok(Plan {
        agent: "opencode",
        after: render_json(&map)?,
        record: Record {
            file: file.clone(),
            existed: previous.map_or(before.is_some(), |record| record.existed),
            keys,
            original: original_of(&before, previous),
            ..Record::default()
        },
        file,
        before,
        notes,
        post: Post::OpenCodeReload,
    })
}

/// Whether the first `x.y.z` in `text` is below `floor`. Unparseable text is
/// not reported as old.
fn older_than(text: &str, floor: (u32, u32, u32)) -> bool {
    let digits = text.trim_start_matches(|c: char| !c.is_ascii_digit());
    let mut parts = digits
        .split(|c: char| !c.is_ascii_digit())
        .take(3)
        .map(|part| part.parse::<u32>().ok());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Some(a)), Some(Some(b)), Some(Some(c))) => (a, b, c) < floor,
        _ => false,
    }
}

// --- Writing and undoing -----------------------------------------------------

/// Write a planned file, creating its directory.
pub fn write(plan: &Plan) -> Result<(), CliError> {
    if let Some(dir) = plan.file.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::runtime(&format!("could not create {}", dir.display()), e))?;
    }
    std::fs::write(&plan.file, &plan.after)
        .map_err(|e| CliError::runtime(&format!("could not write {}", plan.file.display()), e))
}

/// What undoing one agent's wiring would write.
#[derive(Debug, PartialEq, Eq)]
pub enum Undo {
    /// Write this text back.
    Write(String),
    /// The file was created by `setup` and holds nothing else: remove it.
    Delete,
    /// Nothing of ours is left in it.
    Nothing,
}

/// Compute the undo for `record`, and the keys left alone because the user
/// changed them since.
pub fn undo(record: &Record) -> Result<(Undo, Vec<String>), CliError> {
    let Some(current) = read_text(&record.file)? else {
        return Ok((Undo::Nothing, Vec::new()));
    };
    let (next, changed) = if record.block {
        (configfile::clean_with(&current, &CODEX_MARKERS), Vec::new())
    } else {
        let mut map = match serde_json::from_str::<Value>(&current) {
            Ok(Value::Object(map)) => map,
            _ => {
                return Err(CliError::Runtime(format!(
                    "{} is no longer a JSON object; undo it by hand: {}",
                    record.file.display(),
                    record
                        .keys
                        .iter()
                        .map(|key| key.path.join("."))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        };
        let changed = restore_keys(&mut map, &record.keys);
        if !record.existed && map.is_empty() {
            return Ok((Undo::Delete, changed));
        }
        let as_it_was = record.original.as_ref().filter(|original| {
            serde_json::from_str::<Value>(original).ok() == Some(Value::Object(map.clone()))
        });
        match as_it_was {
            Some(original) => (original.clone(), changed),
            None => (render_json(&map)?, changed),
        }
    };
    if record.block && !record.existed && next.trim().is_empty() {
        return Ok((Undo::Delete, changed));
    }
    if next == current {
        return Ok((Undo::Nothing, changed));
    }
    Ok((Undo::Write(next), changed))
}

// --- Status --------------------------------------------------------------------

/// Whether an agent's files point at this proxy right now, read off the files
/// themselves rather than the manifest: a user edit or a `wrap` session in
/// progress shows up as it is.
pub fn wired(agent: &str, endpoints: &Endpoints) -> Option<String> {
    match agent {
        "claude" => {
            let map = crate::wrap::settings::read_json(&claude_settings()).ok()?;
            let env = |name: &str| {
                get(&map, &["env".to_string(), name.to_string()])
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            };
            if let Some((listen, _)) = &endpoints.intercept
                && env("HTTPS_PROXY").as_deref() == Some(&format!("http://{listen}"))
            {
                return Some(format!("intercept via HTTPS_PROXY=http://{listen}"));
            }
            let base = format!("http://127.0.0.1:{}", endpoints.port);
            env("ANTHROPIC_BASE_URL")
                .filter(|url| url.trim_end_matches('/') == base)
                .map(|url| format!("ANTHROPIC_BASE_URL={url}"))
        }
        "codex" => {
            let path = configfile::codex_home().join("config.toml");
            let table: toml::Table = std::fs::read_to_string(&path).ok()?.parse().ok()?;
            let url = table.get("openai_base_url")?.as_str()?.to_owned();
            url.starts_with(&format!("http://127.0.0.1:{}/", endpoints.port))
                .then(|| format!("openai_base_url={url}"))
        }
        "opencode" => {
            let text = std::fs::read_to_string(opencode_file()).ok()?;
            let map = opencode::parse_lenient(&text)?;
            let routed: Vec<&str> = opencode::PROVIDERS
                .iter()
                .filter(|(id, path)| {
                    get(&map, &path_of(&opencode::setting(id, "baseURL"))).and_then(Value::as_str)
                        == Some(opencode::base_url(endpoints.port, path).as_str())
                })
                .map(|(id, _)| *id)
                .collect();
            (!routed.is_empty()).then(|| format!("providers {}", routed.join(", ")))
        }
        _ => None,
    }
}

/// The upstream mounts an agent's traffic lands on, for the traffic column.
pub fn mounts(agent: &str) -> &'static [&'static str] {
    match agent {
        "claude" => &["anthropic", "anthropic-api"],
        "codex" => &["codex-desktop-chatgpt", "openai", "chatgpt-rest"],
        "opencode" => &["opencode-zen"],
        _ => &[],
    }
}

/// Group a list of records by agent, as the manifest keeps them.
pub type Records = BTreeMap<String, Record>;

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoints() -> Endpoints {
        Endpoints {
            port: 8817,
            intercept: Some(("127.0.0.1:8840".into(), PathBuf::from("/s/ca/ca.pem"))),
        }
    }

    fn keys(text: &str) -> Map<String, Value> {
        serde_json::from_str::<Value>(text)
            .unwrap()
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn set_then_restore_puts_every_key_back() {
        let original = keys(r#"{"env":{"KEEP":"1","HTTPS_PROXY":"http://corp:3128"},"x":true}"#);
        let mut map = original.clone();
        let set = set_keys(
            &mut map,
            vec![
                (
                    path_of("env.HTTPS_PROXY"),
                    Value::String("http://127.0.0.1:8840".into()),
                ),
                (
                    path_of("env.NODE_EXTRA_CA_CERTS"),
                    Value::String("/ca.pem".into()),
                ),
            ],
            None,
        );
        assert_eq!(
            get(&map, &path_of("env.HTTPS_PROXY")),
            Some(&Value::String("http://127.0.0.1:8840".into()))
        );
        let changed = restore_keys(&mut map, &set);
        assert!(changed.is_empty());
        assert_eq!(map, original);
    }

    #[test]
    fn a_key_the_user_changed_since_is_left_alone() {
        let mut map = Map::new();
        let set = set_keys(
            &mut map,
            vec![(path_of("env.A"), Value::String("ours".into()))],
            None,
        );
        map.insert("env".into(), serde_json::json!({"A": "theirs"}));
        assert_eq!(restore_keys(&mut map, &set), vec!["env.A".to_string()]);
        assert_eq!(
            get(&map, &path_of("env.A")),
            Some(&Value::String("theirs".into()))
        );
    }

    #[test]
    fn a_second_run_keeps_the_first_runs_original() {
        let mut map = keys(r#"{"env":{"A":"original"}}"#);
        let first = set_keys(
            &mut map,
            vec![(path_of("env.A"), Value::String("one".into()))],
            None,
        );
        let record = Record {
            keys: first,
            ..Record::default()
        };
        let second = set_keys(
            &mut map,
            vec![(path_of("env.A"), Value::String("two".into()))],
            Some(&record),
        );
        assert_eq!(second[0].before, Some(Value::String("original".into())));
    }

    #[test]
    fn switching_claude_modes_drops_the_old_modes_keys() {
        let mut map = Map::new();
        let intercept = claude_env(ClaudeMode::Intercept, &endpoints()).unwrap();
        let first = set_keys(
            &mut map,
            intercept
                .into_iter()
                .map(|(k, v)| (vec!["env".into(), k.into()], Value::String(v)))
                .collect(),
            None,
        );
        let record = Record {
            keys: first,
            ..Record::default()
        };
        drop_stale(
            &mut map,
            Some(&record),
            &[path_of("env.ANTHROPIC_BASE_URL")],
        );
        assert!(map.is_empty(), "{map:?}");
    }

    #[test]
    fn intercept_mode_needs_an_intercept_listener() {
        let none = Endpoints {
            port: 8817,
            intercept: None,
        };
        assert!(claude_env(ClaudeMode::Intercept, &none).is_err());
        let base = claude_env(ClaudeMode::BaseUrl, &none).unwrap();
        assert_eq!(
            base[0],
            ("ANTHROPIC_BASE_URL", "http://127.0.0.1:8817".into())
        );
    }

    #[test]
    fn a_codex_subscription_gets_the_backend_mount_and_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let original = "# mine\nmodel = \"gpt-5.5\"\nopenai_base_url = \"http://127.0.0.1:8817/backend-api/codex\"\n\n[mcp_servers.x]\ncommand = \"y\"\n";
        let target = codex_target(8817, dir.path(), CodexAuth::Subscription);
        let injected = configfile::inject_with(&target, original, &CODEX_MARKERS);
        let table: toml::Table = injected.parse().expect("still valid TOML");
        assert_eq!(
            table["openai_base_url"].as_str(),
            Some("http://127.0.0.1:8817/backend-api/codex")
        );
        assert!(injected.contains(CODEX_MARKERS.disabled), "{injected}");
        assert!(
            table.get("model_provider").is_none(),
            "no provider under a plan"
        );
        assert_eq!(configfile::clean_with(&injected, &CODEX_MARKERS), original);
        // Idempotent: a second run replaces its own block.
        let again = configfile::inject_with(&target, &injected, &CODEX_MARKERS);
        assert_eq!(again, injected);
    }

    #[test]
    fn a_codex_api_key_gets_the_provider_block() {
        let dir = tempfile::tempdir().unwrap();
        let target = codex_target(9000, dir.path(), CodexAuth::StoredApiKey);
        let injected = configfile::inject_with(&target, "", &CODEX_MARKERS);
        let table: toml::Table = injected.parse().unwrap();
        assert_eq!(table["model_provider"].as_str(), Some("kompressor"));
        assert_eq!(
            table["model_providers"]["kompressor"]["base_url"].as_str(),
            Some("http://127.0.0.1:9000/v1")
        );
        assert!(
            configfile::clean_with(&injected, &CODEX_MARKERS)
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn the_codex_block_survives_a_wrap_session_on_top() {
        // wrap strips only its own markers; ours are plain lines to it.
        let target = codex_target(8817, Path::new("/x"), CodexAuth::Subscription);
        let ours = configfile::inject_with(&target, "model = \"m\"\n", &CODEX_MARKERS);
        assert!(!ours.contains("kompressor wrap"));
        assert!(ours.contains(CODEX_MARKERS.begin));
    }

    #[test]
    fn undo_deletes_a_file_setup_created_and_restores_one_it_did_not() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let mut map = Map::new();
        let keys = set_keys(
            &mut map,
            vec![(path_of("env.A"), Value::String("1".into()))],
            None,
        );
        std::fs::write(&file, render_json(&map).unwrap()).unwrap();
        let record = Record {
            file: file.clone(),
            existed: false,
            keys: keys.clone(),
            ..Record::default()
        };
        assert_eq!(undo(&record).unwrap().0, Undo::Delete);
        let record = Record {
            existed: true,
            ..record
        };
        assert_eq!(undo(&record).unwrap().0, Undo::Write("{}\n".into()));
    }

    #[test]
    fn a_codex_file_that_already_says_it_is_adopted_not_rewritten() {
        let target = codex_target(8817, Path::new("/x"), CodexAuth::Subscription);
        let wanted: toml::Table = configfile::inject_with(&target, "", &CODEX_MARKERS)
            .parse()
            .unwrap();
        let same: toml::Table =
            "model = \"m\"\nopenai_base_url = \"http://127.0.0.1:8817/backend-api/codex\"\n"
                .parse()
                .unwrap();
        let other_port: toml::Table =
            "openai_base_url = \"http://127.0.0.1:9/backend-api/codex\"\n"
                .parse()
                .unwrap();
        assert!(contains(&same, &wanted));
        assert!(!contains(&other_port, &wanted));
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(older_than("opencode v2.0.11", (2, 2, 4)));
        assert!(!older_than("v2.3.0", (2, 2, 4)));
        assert!(!older_than("2.10.0", (2, 2, 4)));
        assert!(!older_than("dev build", (2, 2, 4)));
    }
}

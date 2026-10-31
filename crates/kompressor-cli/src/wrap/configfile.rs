//! Pointing an agent at the proxy by injecting a block into the config file it
//! reads.
//!
//! [`super::settings`] handles agents configured by a JSON settings file, and
//! environment variables handle the rest. Codex is neither: it ignores
//! `OPENAI_BASE_URL` outright — measured on 0.155.1, where setting it sent
//! every request straight to the provider and the wrapped session compressed
//! nothing — and the file it does read is TOML that the user owns and comments.
//!
//! So the edit is textual rather than parsed. A block delimited by two marker
//! comments is appended, and removing it is an exact reversal: everything
//! outside the markers is preserved byte for byte, which a TOML round-trip
//! through a value tree could not promise because it would discard the
//! comments and reorder the tables.
//!
//! # Why two blocks
//!
//! Bare TOML keys belong to the table that precedes them, so a key appended
//! after the user's `[model_providers.…]` section would silently land inside
//! it. The top-level keys therefore go at the head of the file and the table we
//! define goes at the foot, each in its own marker pair, so neither span can
//! swallow the user's content between them.
//!
//! # Concurrency
//!
//! The same problem [`super::settings`] documents, and the same answer: a
//! sidecar records who currently holds the file and what it looked like before
//! the first of them arrived, a claim counts only while its owner is alive, and
//! the last session out restores the original.

use std::path::{Path, PathBuf};

use super::settings::{Claim, Holder, Lock, Released, read_owners, sibling, sweep, write_owners};
use crate::error::CliError;

/// The marker opening a block this command owns.
const BEGIN: &str = "# --- kompressor wrap: managed block, removed on exit ---";
/// The marker closing it.
const END: &str = "# --- end kompressor wrap ---";

/// What a top-level key the user set for themselves is prefixed with while
/// this command holds the file.
///
/// TOML rejects a duplicate key outright, so a key the file already assigns
/// cannot simply be overridden by appending ours — the file would stop
/// parsing and take the agent down with it. Commenting theirs out for the
/// duration is the override TOML does not offer, and it is exactly reversible:
/// the prefix is removed again on release, including from a file the user
/// edited mid-session, which the plain block-stripping path cannot do.
const DISABLED: &str = "# kompressor wrap disabled this line: ";

/// The key the ownership sidecar records this file's claim under.
///
/// One key for the whole file rather than one per TOML key: the blocks are
/// written and removed together, so a partial claim could not be restored to
/// anything coherent.
const CLAIM_KEY: &str = "\u{1}config-file";

/// A config file this command injects a block into.
pub struct Target {
    /// The file itself.
    pub path: PathBuf,
    /// Keys written at the head of the file, before any table.
    pub head: Vec<(String, String)>,
    /// Whole tables written at the foot, as rendered TOML.
    pub tables: Vec<Table>,
}

/// One `[table]` this command defines.
pub struct Table {
    /// The table's dotted name, e.g. `model_providers.kompressor`.
    pub name: String,
    /// Its keys, already rendered as TOML values.
    pub entries: Vec<(String, String)>,
}

/// How a codex install authenticates.
///
/// This decides what may be written, not merely what is convenient: the two
/// modes need *different* blocks, and the block for one breaks the other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodexAuth {
    /// `codex login --with-api-key`. The key is in the environment.
    ApiKey,
    /// A ChatGPT plan. There is no API key anywhere.
    Subscription,
}

/// Read the auth mode out of codex's `auth.json`.
///
/// Only the `auth_mode` field is read, and nothing else in that file is
/// touched, logged or copied — it holds the credential itself.
///
/// An unreadable or unfamiliar file is reported as [`CodexAuth::ApiKey`],
/// because that is the mode whose block is inert when it is wrong: a
/// subscription session that is handed the API-key block fails loudly at
/// startup, which is recoverable, while the reverse would route a key-holder's
/// traffic past the proxy in silence.
fn codex_auth(home: &Path) -> CodexAuth {
    let Ok(text) = std::fs::read_to_string(home.join("auth.json")) else {
        return CodexAuth::ApiKey;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return CodexAuth::ApiKey;
    };
    match value.get("auth_mode").and_then(serde_json::Value::as_str) {
        Some("chatgpt") => CodexAuth::Subscription,
        _ => CodexAuth::ApiKey,
    }
}

impl Target {
    /// Codex's `config.toml`, honouring `CODEX_HOME`.
    pub fn codex(port: u16) -> Target {
        let home = codex_home();
        Target::codex_for(port, &home, codex_auth(&home))
    }

    /// The codex target for a given home and auth mode.
    ///
    /// # Why the two modes get different blocks
    ///
    /// Measured on codex 0.155.1, against both a live ChatGPT plan and an
    /// API-key login:
    ///
    /// * **API key.** A custom provider is the good answer. It is the only
    ///   place `supports_websockets = false` can be set, and without that codex
    ///   spends five refused handshakes — about 13 seconds — on the first
    ///   request of every session before falling back to HTTP.
    ///
    /// * **Subscription.** The same block is fatal. `model_provider` *is*
    ///   honoured under a ChatGPT plan — which contradicts the obvious reading,
    ///   and is why this was measured rather than assumed — so codex selects
    ///   our provider, finds the `env_key` it declares, and refuses the session
    ///   with `Missing environment variable: OPENAI_API_KEY` before sending
    ///   anything. So a subscription install gets `openai_base_url` alone: it
    ///   overrides the built-in provider's URL and leaves the ChatGPT
    ///   credential on the path that already carries it.
    ///
    /// The subscription path therefore cannot suppress the WebSocket attempt —
    /// there is no top-level key for it — and pays the handshake. That is a
    /// real cost and it is recorded in docs/CODEX.md rather than hidden.
    pub fn codex_for(port: u16, home: &Path, auth: CodexAuth) -> Target {
        let base = format!("http://127.0.0.1:{port}/v1");
        let path = home.join("config.toml");
        match auth {
            CodexAuth::Subscription => Target {
                path,
                head: vec![("openai_base_url".into(), quoted(&base))],
                tables: Vec::new(),
            },
            CodexAuth::ApiKey => Target {
                path,
                head: vec![
                    ("model_provider".into(), quoted("kompressor")),
                    ("openai_base_url".into(), quoted(&base)),
                ],
                tables: vec![Table {
                    name: "model_providers.kompressor".into(),
                    entries: vec![
                        ("name".into(), quoted("OpenAI via kompressor")),
                        ("base_url".into(), quoted(&base)),
                        ("wire_api".into(), quoted("responses")),
                        ("env_key".into(), quoted("OPENAI_API_KEY")),
                        ("supports_websockets".into(), "false".into()),
                    ],
                }],
            },
        }
    }

    fn owners_path(&self) -> PathBuf {
        sibling(&self.path, ".kompressor-wrap.json")
    }

    fn lock_path(&self) -> PathBuf {
        sibling(&self.path, ".kompressor-wrap.lock")
    }

    /// The text of the two blocks, head first.
    fn render(&self) -> (String, String) {
        let mut head = String::new();
        if !self.head.is_empty() {
            head.push_str(BEGIN);
            head.push('\n');
            for (key, value) in &self.head {
                head.push_str(&format!("{key} = {value}\n"));
            }
            head.push_str(END);
            head.push('\n');
        }

        let mut foot = String::new();
        if !self.tables.is_empty() {
            foot.push_str(BEGIN);
            foot.push('\n');
            for table in &self.tables {
                foot.push_str(&format!("[{}]\n", table.name));
                for (key, value) in &table.entries {
                    foot.push_str(&format!("{key} = {value}\n"));
                }
            }
            foot.push_str(END);
            foot.push('\n');
        }
        (head, foot)
    }

    /// What we would write that the file already defines for itself: its own
    /// top-level keys, and its own copy of a table we append.
    fn collisions(&self, content: &str) -> Vec<String> {
        let outside = strip_blocks(content);
        let keys = self
            .head
            .iter()
            .map(|(key, _)| key)
            .filter(|key| declares_key(&outside, key))
            .cloned();
        let tables = self
            .tables
            .iter()
            .filter(|table| declares_table(&outside, &table.name))
            .map(|table| format!("[{}]", table.name));
        keys.chain(tables).collect()
    }

    /// Comment out the user's own definition of anything we are about to write.
    ///
    /// A top-level key is only looked for before the first table header, for
    /// the same reason [`declares_key`] only reads there: a key of the same
    /// name inside a table is a different key. A table we append is commented
    /// out whole, header to the next header, because TOML rejects a table
    /// defined twice just as it rejects a key.
    ///
    /// Whole statements are disabled, not lines: a multi-line string or array
    /// commented out on its first line only leaves the rest of the value
    /// standing on its own, which is a file codex will not load.
    fn disable_collisions(&self, body: &str) -> String {
        let keys: Vec<&str> = self.head.iter().map(|(key, _)| key.as_str()).collect();
        let tables: Vec<&str> = self
            .tables
            .iter()
            .map(|table| table.name.as_str())
            .collect();
        if keys.is_empty() && tables.is_empty() {
            return body.to_string();
        }
        let lines: Vec<&str> = body.lines().collect();
        let mut out = String::with_capacity(body.len());
        let mut in_tables = false;
        let mut in_ours = false;
        for (start, end) in statements(&lines) {
            let first = lines[start].trim_start();
            // Any header ends the top level, `[[array]]` included; only a
            // plain `[table]` can be one of ours.
            let disable = if first.starts_with('[') {
                in_tables = true;
                in_ours = table_header(first).is_some_and(|name| tables.contains(&name.as_str()));
                in_ours
            } else if in_tables {
                in_ours
            } else {
                keys.iter().any(|key| assigns(first, key))
            };
            for line in &lines[start..end] {
                if disable {
                    out.push_str(DISABLED);
                }
                out.push_str(line);
                out.push('\n');
            }
        }
        if !body.ends_with('\n') {
            out.pop();
        }
        out
    }
}

/// Codex's configuration directory.
fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".codex"))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A TOML string literal.
///
/// The values here are URLs and identifiers this command builds, so the escape
/// set is small — but a path from `CODEX_HOME` can reach a base URL, and an
/// unescaped quote there would produce a file Codex cannot read.
fn quoted(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Whether `content` assigns `key` at the top level.
///
/// Only the region before the first table header counts: `model_provider`
/// inside `[some.table]` is a different key that happens to share a name.
fn declares_key(content: &str, key: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    statements(&lines)
        .into_iter()
        .map(|(start, _)| lines[start].trim_start())
        .take_while(|line| !line.starts_with('['))
        .any(|line| assigns(line, key))
}

/// Whether `content` opens a `[name]` table of its own.
fn declares_table(content: &str, name: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    statements(&lines)
        .into_iter()
        .filter_map(|(start, _)| table_header(lines[start].trim_start()))
        .any(|header| header == name)
}

/// Whether one line assigns `key`, in any of the three spellings TOML allows
/// for a key: bare, `"basic"` or `'literal'`.
///
/// A line already carrying the [`DISABLED`] prefix is a comment and assigns
/// nothing, which is what makes claiming the file twice idempotent.
fn assigns(line: &str, key: &str) -> bool {
    [key.to_string(), format!("\"{key}\""), format!("'{key}'")]
        .iter()
        .any(|spelling| {
            line.strip_prefix(spelling.as_str())
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
}

/// The dotted name a `[table]` header line opens, normalised the way TOML
/// reads it: `[ a . "b" ]` is `a.b`. `None` for anything that is not a plain
/// table header, array-of-tables headers included.
fn table_header(line: &str) -> Option<String> {
    if !line.starts_with('[') || line.starts_with("[[") {
        return None;
    }
    let mut table = line.parse::<toml::Table>().ok()?;
    let mut name = Vec::new();
    loop {
        let mut entries = table.into_iter();
        let (key, value) = entries.next()?;
        if entries.next().is_some() {
            return None;
        }
        name.push(key);
        match value {
            toml::Value::Table(inner) if inner.is_empty() => return Some(name.join(".")),
            toml::Value::Table(inner) => table = inner,
            _ => return None,
        }
    }
}

/// The file as statements, each the `[start, end)` range of lines it spans.
///
/// A value can run over several lines — a multi-line string, an array — and
/// a line inside one is neither an assignment nor a table header, whatever it
/// starts with. A statement is grown a line at a time until it parses on its
/// own; one that never does is left a single line, so a file that is already
/// broken is read as it was before this existed.
fn statements(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let mut end = start + 1;
        let first = lines[start].trim_start();
        let opens_a_value = !first.starts_with('#')
            && !first.starts_with('[')
            && first.contains('=')
            && first.parse::<toml::Table>().is_err();
        if opens_a_value {
            let mut text = lines[start].to_string();
            let mut grown = end;
            while grown < lines.len() {
                text.push('\n');
                text.push_str(lines[grown]);
                grown += 1;
                if text.parse::<toml::Table>().is_ok() {
                    end = grown;
                    break;
                }
            }
        }
        spans.push((start, end));
        start = end;
    }
    spans
}

/// Undo [`Target::disable_collisions`], whatever else happened to the file.
fn reinstate(content: &str) -> String {
    if !content.contains(DISABLED) {
        return content.to_string();
    }
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        match line.strip_prefix(DISABLED) {
            Some(rest) => out.push_str(rest),
            None => out.push_str(line),
        }
        out.push('\n');
    }
    if !content.ends_with('\n') {
        out.pop();
    }
    out
}

/// Remove every block this command owns, leaving the rest untouched.
///
/// Unmatched markers are left alone: a file holding only a `BEGIN` is one a
/// user edited by hand, and guessing where the block ended could delete their
/// content.
fn strip_blocks(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find(BEGIN) {
        let Some(end_rel) = rest[start..].find(END) else {
            break;
        };
        let end = start + end_rel + END.len();
        out.push_str(&rest[..start]);
        rest = rest[end..].strip_prefix('\n').unwrap_or(&rest[end..]);
    }
    out.push_str(rest);
    out
}

/// What [`claim`] decided to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Claimed {
    /// The block was written.
    Injected,
    /// An identical block was already there, from this session or another.
    AlreadyCurrent,
}

/// Inject the block, recording this process as an owner.
pub fn claim(target: &Target, port: u16) -> Result<Claimed, CliError> {
    let _lock = Lock::acquire(target.lock_path());
    let owners_path = target.owners_path();
    let mut owners = read_owners(&owners_path);

    let current = std::fs::read_to_string(&target.path).unwrap_or_default();
    let existed = target.path.exists();

    // The edit is textual, so it is checked the one way that matters: a file
    // codex could load before must still load after. A shape the override
    // does not understand is refused with the file untouched, rather than
    // written in a form that stops the session before it starts.
    let next = injected_content(target, &current);
    if current.parse::<toml::Table>().is_ok()
        && let Err(error) = next.parse::<toml::Table>()
    {
        return Err(CliError::Runtime(format!(
            "{} defines something this command cannot override safely ({}); it was left \
             untouched. Remove or rename that definition and run this again",
            target.path.display(),
            error.message()
        )));
    }

    // A key the file already sets is commented out for the duration rather
    // than refused. Refusing was the first answer and it was the wrong one: it
    // stopped the session dead over a line the user is perfectly happy for us
    // to override, and the only way forward was to hand-edit the file and
    // remember to put it back. Say what was done, and undo it on release.
    let collisions = target.collisions(&current);
    if !collisions.is_empty() {
        eprintln!(
            "kompressor: {} already sets {} — commented out for this session, \
             restored on exit",
            target.path.display(),
            collisions.join(" and "),
        );
    }

    let mut claim = owners.keys.remove(CLAIM_KEY).unwrap_or_default();
    sweep(&mut claim, &owners_path);
    // Recorded once, by the first live claimant: a later session must restore
    // what was there before any of us, not the injected state it walked in on.
    if claim.holders.is_empty() {
        claim.original = existed.then(|| current.clone());
    }
    claim.holders.retain(|h| h.pid != std::process::id());
    claim.holders.push(Holder::me(Some(port)));

    let changed = next != current;
    if changed {
        if let Some(dir) = target.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                CliError::runtime(&format!("could not create {}", dir.display()), e)
            })?;
        }
        std::fs::write(&target.path, &next).map_err(|e| {
            CliError::runtime(&format!("could not write {}", target.path.display()), e)
        })?;
    }
    owners.keys.insert(CLAIM_KEY.to_string(), claim);
    write_owners(&owners_path, &owners)?;
    Ok(if changed {
        Claimed::Injected
    } else {
        Claimed::AlreadyCurrent
    })
}

/// Give up this session's claim, restoring the file if it was the last holder.
///
/// `force` is `unwrap`: every claim is dropped and the file restored whoever
/// else is running, because the user is asking for their config back.
pub fn release(target: &Target, force: bool) -> Result<Released, CliError> {
    let _lock = Lock::acquire(target.lock_path());
    let owners_path = target.owners_path();
    let mut owners = read_owners(&owners_path);
    let Some(mut claim) = owners.keys.remove(CLAIM_KEY) else {
        // No claim recorded, but a block may still be there from a session
        // that was killed. Removing it is the repair `unwrap` exists for.
        return strip_orphan(target, force);
    };
    sweep(&mut claim, &owners_path);
    if force {
        claim.holders.clear();
    } else {
        claim.holders.retain(|h| h.pid != std::process::id());
    }
    if !claim.holders.is_empty() {
        owners.keys.insert(CLAIM_KEY.to_string(), claim);
        write_owners(&owners_path, &owners)?;
        return Ok(Released::StillInUse);
    }

    restore(target, &claim)?;
    write_owners(&owners_path, &owners)?;
    Ok(Released::Restored)
}

fn injected_content(target: &Target, current: &str) -> String {
    let (head, foot) = target.render();
    let body = target.disable_collisions(&strip_blocks(current));
    let mut next = String::new();
    next.push_str(&head);
    if !body.trim().is_empty() {
        next.push_str(body.trim_start_matches('\n'));
        if !next.ends_with('\n') {
            next.push('\n');
        }
    }
    if !foot.is_empty() {
        if !next.is_empty() && !next.ends_with("\n\n") {
            next.push('\n');
        }
        next.push_str(&foot);
    }

    next
}

/// Remove our settings while preserving edits made during the session.
fn restore(target: &Target, claim: &Claim) -> Result<(), CliError> {
    let current = match std::fs::read_to_string(&target.path) {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(CliError::runtime(
                "could not read configuration for cleanup",
                error,
            ));
        }
    };
    let original = claim.original.as_deref().unwrap_or_default();
    let unchanged = current == injected_content(target, original);
    let cleaned = if unchanged {
        original.to_owned()
    } else {
        // The user edited the file while we held it, so their edits are kept
        // and only our changes undone — which has to include un-commenting
        // what `disable_collisions` turned off, or the key stays dead after we
        // are gone.
        reinstate(&strip_blocks(&current))
    };
    if claim.original.is_none() && unchanged {
        std::fs::remove_file(&target.path)
            .map_err(|e| CliError::runtime("could not remove managed configuration", e))
    } else {
        std::fs::write(&target.path, cleaned)
            .map_err(|e| CliError::runtime("could not clean managed configuration", e))
    }
}

/// Strip a block left behind by a session that never got to clean up.
fn strip_orphan(target: &Target, force: bool) -> Result<Released, CliError> {
    if !force {
        return Ok(Released::NotOurs);
    }
    let Ok(current) = std::fs::read_to_string(&target.path) else {
        return Ok(Released::NotOurs);
    };
    if !current.contains(BEGIN) && !current.contains(DISABLED) {
        return Ok(Released::NotOurs);
    }
    let stripped = reinstate(&strip_blocks(&current));
    std::fs::write(&target.path, &stripped)
        .map_err(|e| CliError::runtime(&format!("could not write {}", target.path.display()), e))?;
    Ok(Released::Restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "kompressor-configfile-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn target(&self, port: u16) -> Target {
            Target::codex_for(port, &self.0, CodexAuth::ApiKey)
        }
        fn subscription_target(&self, port: u16) -> Target {
            Target::codex_for(port, &self.0, CodexAuth::Subscription)
        }
        fn write_auth(&self, mode: &str) {
            std::fs::write(
                self.0.join("auth.json"),
                format!("{{\"auth_mode\":\"{mode}\",\"tokens\":{{}}}}"),
            )
            .unwrap();
        }
        fn write(&self, content: &str) {
            std::fs::write(self.0.join("config.toml"), content).unwrap();
        }
        fn read(&self) -> String {
            std::fs::read_to_string(self.0.join("config.toml")).unwrap_or_default()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cleanup_preserves_edits_to_existing_settings_and_comments() {
        let scratch = Scratch::new("session-edits");
        scratch.write("# original comment\nmodel = \"old\"\n");
        let target = scratch.target(9230);
        claim(&target, 9230).unwrap();
        let edited = scratch.read().replace("\"old\"", "\"new\"") + "# added during session\n";
        scratch.write(&edited);
        release(&target, false).unwrap();
        let cleaned = scratch.read();
        assert!(cleaned.contains("model = \"new\""));
        assert!(cleaned.contains("# original comment"));
        assert!(cleaned.contains("# added during session"));
        assert!(!cleaned.contains(BEGIN));
    }

    #[test]
    fn forced_cleanup_preserves_user_settings_in_a_file_we_created() {
        let scratch = Scratch::new("new-session-edits");
        let target = scratch.target(9231);
        claim(&target, 9231).unwrap();
        scratch.write(&(scratch.read() + "[projects.example]\ntrust_level = \"trusted\"\n"));
        release(&target, true).unwrap();
        assert!(target.path.exists());
        assert!(scratch.read().contains("[projects.example]"));
        assert!(!scratch.read().contains(BEGIN));
    }

    #[test]
    fn the_injected_block_carries_what_codex_needs_to_reach_the_proxy() {
        let scratch = Scratch::new("inject");
        scratch.write("model = \"gpt-5.5\"\n");
        let target = scratch.target(9123);
        assert_eq!(claim(&target, 9123).unwrap(), Claimed::Injected);

        let text = scratch.read();
        assert!(text.contains("openai_base_url = \"http://127.0.0.1:9123/v1\""));
        assert!(text.contains("model_provider = \"kompressor\""));
        assert!(text.contains("[model_providers.kompressor]"));
        // The measured cure for a five-attempt WebSocket handshake.
        assert!(text.contains("supports_websockets = false"));
        // The user's own setting survives.
        assert!(text.contains("model = \"gpt-5.5\""));
    }

    #[test]
    fn the_top_level_keys_precede_every_table() {
        // A bare key after a table header belongs to that table, so getting
        // this wrong would define `model_provider` inside the user's section.
        let scratch = Scratch::new("order");
        scratch.write("[mcp_servers.thing]\ncommand = \"x\"\n");
        let target = scratch.target(9124);
        claim(&target, 9124).unwrap();

        let text = scratch.read();
        let key = text.find("model_provider = ").unwrap();
        let table = text.find("[mcp_servers.thing]").unwrap();
        assert!(
            key < table,
            "top-level key must come before any table:\n{text}"
        );
    }

    #[test]
    fn releasing_restores_the_file_byte_for_byte() {
        let scratch = Scratch::new("restore");
        let original = "# my notes\nmodel = \"gpt-5.5\"\n\n[mcp_servers.thing]\ncommand = \"x\"\n";
        scratch.write(original);
        let target = scratch.target(9125);
        claim(&target, 9125).unwrap();
        assert_ne!(scratch.read(), original);

        assert_eq!(release(&target, false).unwrap(), Released::Restored);
        assert_eq!(
            scratch.read(),
            original,
            "a comment or a blank line lost here is the user's file damaged"
        );
    }

    #[test]
    fn a_file_we_created_is_removed_rather_than_left_empty() {
        let scratch = Scratch::new("created");
        let target = scratch.target(9126);
        claim(&target, 9126).unwrap();
        assert!(target.path.exists());

        release(&target, false).unwrap();
        assert!(
            !target.path.exists(),
            "a config directory we do not own should not keep our leftovers"
        );
    }

    #[test]
    fn claiming_twice_is_idempotent() {
        let scratch = Scratch::new("twice");
        scratch.write("model = \"gpt-5.5\"\n");
        let target = scratch.target(9127);
        claim(&target, 9127).unwrap();
        let once = scratch.read();
        assert_eq!(claim(&target, 9127).unwrap(), Claimed::AlreadyCurrent);
        assert_eq!(scratch.read(), once, "a second claim must not stack blocks");
    }

    #[test]
    fn a_key_the_user_already_sets_is_commented_out_rather_than_duplicated() {
        // TOML rejects duplicate keys, so appending ours would not win the
        // argument — it would stop Codex reading the file at all. Commenting
        // theirs out is the override TOML does not offer.
        let scratch = Scratch::new("collide");
        scratch.write("model_provider = \"mine\"\n");
        let target = scratch.target(9128);
        claim(&target, 9128).expect("a collision is not a reason to give up");

        let written = scratch.read();
        assert!(
            written.contains(&format!("{DISABLED}model_provider = \"mine\"")),
            "the user's line should have been commented, not removed: {written}"
        );
        assert_eq!(
            written
                .lines()
                .filter(|line| assigns(line.trim_start(), "model_provider"))
                .count(),
            1,
            "exactly one live assignment, or the file does not parse: {written}"
        );
    }

    #[test]
    fn the_line_it_commented_out_comes_back_on_release() {
        let scratch = Scratch::new("collide-restore");
        let original = "# my own settings\nmodel_provider = \"mine\"\nmodel = \"gpt-5\"\n";
        scratch.write(original);
        let target = scratch.target(9129);

        claim(&target, 9129).unwrap();
        assert!(scratch.read().contains(DISABLED));
        release(&target, false).unwrap();
        assert_eq!(
            scratch.read(),
            original,
            "release must put the file back exactly as it was"
        );
    }

    #[test]
    fn a_line_commented_out_by_a_session_that_died_is_repaired_by_unwrap() {
        // The session is killed before it can clean up, so the sidecar never
        // records a release and the user is left with their own setting
        // commented out by a process that no longer exists. `--unwrap` is the
        // repair, and it has to reach the commented line, not just the block.
        let scratch = Scratch::new("collide-orphan");
        scratch.write("model_provider = \"mine\"\n");
        let target = scratch.target(9130);
        claim(&target, 9130).unwrap();
        std::fs::remove_file(target.owners_path()).unwrap();

        release(&target, true).unwrap();
        let repaired = scratch.read();
        assert!(
            !repaired.contains(DISABLED),
            "the user's setting is still commented out: {repaired:?}"
        );
        assert!(
            repaired
                .lines()
                .any(|line| line.trim() == "model_provider = \"mine\""),
            "the user's setting did not come back: {repaired:?}"
        );
        // Whitespace is not asserted: `--unwrap` leaves the blank line that
        // separated the two blocks, which it did before this existed.
    }

    #[test]
    fn claiming_a_file_twice_does_not_comment_the_line_out_twice() {
        let scratch = Scratch::new("collide-twice");
        scratch.write("model_provider = \"mine\"\n");
        let target = scratch.target(9131);
        claim(&target, 9131).unwrap();
        let once = scratch.read();
        claim(&target, 9131).unwrap();
        assert_eq!(scratch.read(), once, "the second claim changed the file");
        assert_eq!(once.matches(DISABLED).count(), 1);
    }

    #[test]
    fn a_key_of_the_same_name_inside_a_table_is_left_alone() {
        // `openai_base_url` under `[model_providers.x]` is a different key that
        // happens to share a name, and commenting it out would break a provider
        // the user configured for something else entirely.
        let scratch = Scratch::new("collide-table");
        let original = "[model_providers.other]\nmodel_provider = \"theirs\"\n";
        scratch.write(original);
        let target = scratch.target(9132);
        claim(&target, 9132).unwrap();
        assert!(
            !scratch.read().contains(DISABLED),
            "a key inside a table is not a collision: {}",
            scratch.read()
        );
    }

    #[test]
    fn the_same_name_inside_a_table_is_not_a_collision() {
        let scratch = Scratch::new("nested");
        scratch.write("[profiles.work]\nmodel_provider = \"mine\"\n");
        let target = scratch.target(9129);
        assert_eq!(claim(&target, 9129).unwrap(), Claimed::Injected);
    }

    #[test]
    fn unwrap_strips_a_block_left_by_a_session_that_was_killed() {
        let scratch = Scratch::new("orphan");
        let target = scratch.target(9130);
        claim(&target, 9130).unwrap();
        // Lose the ownership record the way a `kill -9` would.
        let _ = std::fs::remove_file(target.owners_path());

        assert_eq!(release(&target, false).unwrap(), Released::NotOurs);
        assert_eq!(release(&target, true).unwrap(), Released::Restored);
        assert!(!scratch.read().contains("kompressor"));
    }

    #[test]
    fn a_marker_without_its_closing_pair_is_left_alone() {
        // Half a block means someone edited the file by hand. Guessing where
        // it ended could delete their content.
        let text = format!("{BEGIN}\nmodel_provider = \"x\"\nkeep = 1\n");
        assert_eq!(strip_blocks(&text), text);
    }

    #[test]
    fn a_subscription_install_gets_no_provider_block() {
        // Measured on codex 0.155.1: `model_provider` IS honoured under a
        // ChatGPT plan, so a provider block declaring `env_key` makes codex
        // refuse the session with "Missing environment variable:
        // OPENAI_API_KEY" before it sends anything. The override alone works.
        let scratch = Scratch::new("subauth");
        scratch.write("model = \"gpt-5.5\"\n");
        let target = scratch.subscription_target(9140);
        claim(&target, 9140).unwrap();

        let text = scratch.read();
        assert!(text.contains("openai_base_url = \"http://127.0.0.1:9140/v1\""));
        assert!(
            !text.contains("model_provider"),
            "selecting our provider under a subscription kills the session:\n{text}"
        );
        assert!(
            !text.contains("env_key"),
            "a subscription install has no API key to name:\n{text}"
        );
    }

    #[test]
    fn an_api_key_install_gets_the_provider_block_and_its_websocket_setting() {
        let scratch = Scratch::new("keyauth");
        scratch.write("model = \"gpt-5.5\"\n");
        let target = scratch.target(9141);
        claim(&target, 9141).unwrap();

        let text = scratch.read();
        assert!(text.contains("model_provider = \"kompressor\""));
        assert!(text.contains("[model_providers.kompressor]"));
        // The only place this can be set, and worth ~13s per session.
        assert!(text.contains("supports_websockets = false"));
    }

    #[test]
    fn the_auth_mode_comes_from_codexs_own_auth_file() {
        let scratch = Scratch::new("detect");
        scratch.write_auth("chatgpt");
        assert_eq!(codex_auth(&scratch.0), CodexAuth::Subscription);
        scratch.write_auth("apikey");
        assert_eq!(codex_auth(&scratch.0), CodexAuth::ApiKey);
    }

    #[test]
    fn an_unreadable_auth_file_is_treated_as_an_api_key() {
        // The mode whose block fails loudly when it is wrong. Guessing
        // subscription instead would route a key-holder past the proxy in
        // silence, which is this command's worst outcome.
        let scratch = Scratch::new("noauth");
        assert_eq!(codex_auth(&scratch.0), CodexAuth::ApiKey);
        std::fs::write(scratch.0.join("auth.json"), "not json at all").unwrap();
        assert_eq!(codex_auth(&scratch.0), CodexAuth::ApiKey);
    }

    /// Each of these parses before the claim. TOML has no override, so a
    /// claim that misses one of them yields a file codex refuses to load.
    #[test]
    fn a_valid_config_stays_valid_toml_while_claimed_and_comes_back_byte_for_byte() {
        let cases = [
            (
                "quoted-key",
                "\"model_provider\" = \"openai\"\nmodel = \"o3\"\n",
            ),
            (
                "literal-key",
                "'openai_base_url' = 'https://x.example/v1'\n",
            ),
            (
                "multiline-basic",
                "openai_base_url = \"\"\"\nhttps://api.openai.com/v1\"\"\"\nmodel = \"o3\"\n",
            ),
            (
                "multiline-literal",
                "openai_base_url = '''\n[not.a.table]\nhttps://x'''\nmodel = \"o3\"\n",
            ),
            (
                "multiline-array",
                "model_provider = [\n  \"a\",\n  \"b\",\n]\nmodel = \"o3\"\n",
            ),
            (
                "existing-table",
                "model = \"o3\"\n\n[model_providers.kompressor]\nname = \"Existing\"\n\
                 base_url = \"http://localhost:8000/v1\"\n\n[profiles.x]\nmodel = \"o4\"\n",
            ),
            (
                "array-of-tables-first",
                "[[mcp]]\nmodel_provider = \"inside\"\n",
            ),
            (
                "existing-table-spaced",
                "[ model_providers . \"kompressor\" ]\nname = \"Existing\"\n",
            ),
        ];
        for (tag, original) in cases {
            original
                .parse::<toml::Table>()
                .unwrap_or_else(|e| panic!("{tag}: the fixture itself: {e}"));
            let scratch = Scratch::new(&format!("valid-{tag}"));
            scratch.write(original);
            let target = scratch.target(9240);
            claim(&target, 9240).unwrap_or_else(|e| panic!("{tag}: {e}"));
            let claimed = scratch.read();
            let parsed = claimed
                .parse::<toml::Table>()
                .unwrap_or_else(|e| panic!("{tag}: claimed file does not parse: {e}\n{claimed}"));
            assert_eq!(
                parsed.get("model_provider").and_then(toml::Value::as_str),
                Some("kompressor"),
                "{tag}"
            );
            assert_eq!(
                parsed["model_providers"]["kompressor"]["base_url"].as_str(),
                Some("http://127.0.0.1:9240/v1"),
                "{tag}"
            );
            release(&target, false).unwrap();
            assert_eq!(scratch.read(), original, "{tag}");
        }
    }

    /// A shape this textual edit cannot override safely is refused, and the
    /// file is left as it was, rather than written in a form codex rejects.
    #[test]
    fn a_claim_that_would_break_a_valid_file_is_refused() {
        let scratch = Scratch::new("refused");
        let original = "model_providers.kompressor.name = \"dotted\"\n";
        original.parse::<toml::Table>().unwrap();
        scratch.write(original);
        let target = scratch.target(9241);
        let error = claim(&target, 9241).expect_err("would not parse");
        assert!(error.to_string().contains("config.toml"), "{error}");
        assert_eq!(scratch.read(), original);
        assert_eq!(release(&target, false).unwrap(), Released::NotOurs);
    }
}

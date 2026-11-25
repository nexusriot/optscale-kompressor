//! `kompressor setup` — point Kompressor and the agents on this machine at each
//! other, durably, in one command.
//!
//! `wrap` is per session and writes nothing that outlives it. `setup` is the
//! opposite trade, made on purpose: it leaves compression on for every session
//! of every agent — the CLI, the desktop apps, editors — with no terminal held
//! open. What makes that acceptable is that it is exactly reversible:
//!
//! ```text
//! setup ─ detect service ─▶ base config ─▶ render 50-setup.toml ─▶ plan agents
//!                                                                     │
//!          ┌───────── print plan (diffs with --dry-run), confirm ◀───┘
//!          ▼
//!   write overlay (sudo if /etc) ─▶ restart service ─▶ /healthz ok?
//!          │                                              │ no: stop, wire nothing
//!          ▼ yes
//!   write each agent's file ─▶ opencode reload / claude mcp add ─▶ manifest
//! ```
//!
//! * Kompressor's own settings go into one overlay file it owns
//!   (`<config>.d/50-setup.toml`, see [`kompressor_core::config::overlay_files`]),
//!   never into the configuration a user or a package manager owns.
//! * Each agent edit records what it replaced in a manifest
//!   (`~/.config/kompressor/setup.json`); `setup remove` puts it back.
//! * Agents are wired only once the service answers `/healthz` with the new
//!   settings, so a failed restart never leaves an agent pointed at nothing.

/// `eprintln!` that cannot panic on a closed console; the same as `wrap`'s.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

pub mod agents;
pub mod profile;
pub mod service;

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kompressor_core::Config;
use kompressor_core::config::{merge_toml, overlay_dir, overlay_files};
use serde::{Deserialize, Serialize};

use crate::error::{CliError, CliResult};
use agents::{ClaudeMode, Endpoints, Plan, Post, Record, Records, Undo};
use service::{Kind, Service, UserInstall};

/// Arguments of `kompressor setup`.
pub struct SetupArgs {
    /// Agents to wire; `None` is every installed one.
    pub agents: Option<Vec<String>>,
    pub config: Option<PathBuf>,
    pub claude_mode: ClaudeMode,
    pub with_mcp: bool,
    pub dry_run: bool,
    pub yes: bool,
    pub no_restart: bool,
    pub force: bool,
}

/// Arguments of `kompressor setup remove`.
pub struct RemoveArgs {
    pub agents: Option<Vec<String>>,
    pub dry_run: bool,
    pub yes: bool,
    pub keep_profile: bool,
}

/// What `setup` did, kept so `setup remove` can undo it.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kompressor: Option<KompressorRecord>,
    #[serde(default)]
    pub agents: Records,
}

/// The Kompressor side of the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KompressorRecord {
    /// The base configuration the overlay sits beside.
    pub config: PathBuf,
    /// The overlay `setup` owns.
    pub overlay: PathBuf,
    pub service: Kind,
    /// A user unit `setup` installed, which `remove` leaves in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_unit: Option<PathBuf>,
}

impl Manifest {
    pub fn path() -> PathBuf {
        service::config_home().join("kompressor").join("setup.json")
    }

    pub fn load() -> Result<Manifest, CliError> {
        let path = Manifest::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                CliError::Runtime(format!(
                    "{} is not a setup manifest ({e}); move it aside and run again",
                    path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest {
                version: 1,
                ..Manifest::default()
            }),
            Err(e) => Err(CliError::runtime(
                &format!("could not read {}", path.display()),
                e,
            )),
        }
    }

    pub fn save(&self) -> Result<(), CliError> {
        let path = Manifest::path();
        if self.kompressor.is_none() && self.agents.is_empty() {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                CliError::runtime(&format!("could not create {}", dir.display()), e)
            })?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| CliError::runtime("could not render the manifest", e))?;
        // Owner-only: it records the values it replaced, which may be URLs
        // carrying a token.
        crate::wrap::paths::write_private(&path, &(text + "\n"))
            .map_err(|e| CliError::runtime(&format!("could not write {}", path.display()), e))
    }
}

/// Where the base configuration is, and how the service runs it.
struct Base {
    service: Service,
    path: PathBuf,
    /// A user service this run will install first.
    install: Option<UserInstall>,
}

fn resolve_base(config: Option<&Path>, manifest: &Manifest) -> Result<Base, CliError> {
    let service = runs(Service::detect(), config);
    let from_manifest = manifest.kompressor.as_ref().map(|k| k.config.clone());
    let path = config
        .map(Path::to_path_buf)
        .or_else(|| service.config.clone())
        .or_else(|| match service.kind {
            Kind::System => Config::system_path(),
            Kind::User => Some(UserInstall::plan().config),
            Kind::None => None,
        })
        .or(from_manifest);
    if let Some(path) = path {
        return Ok(Base {
            service,
            path,
            install: None,
        });
    }
    if service::has_systemd() {
        let install = UserInstall::plan();
        return Ok(Base {
            service: Service {
                kind: Kind::User,
                active: false,
                config: Some(install.config.clone()),
                binary: std::env::current_exe().ok(),
            },
            path: install.config.clone(),
            install: Some(install),
        });
    }
    match Config::discover() {
        Some(path) => Ok(Base {
            service,
            path,
            install: None,
        }),
        None => Err(CliError::Usage(
            "no kompressor service was found and this system has no systemd to install one \
             under. Install the package for this platform (docs/MACOS.md, docs/WINDOWS.md), or \
             run `kompressor serve` under your service manager, then run setup again with \
             --config naming its configuration."
                .into(),
        )),
    }
}

/// `service`, unless `config` names a file other than the one it runs: then the
/// configuration being set up is not the service's, and restarting the
/// service would neither load it nor be asked for.
fn runs(service: Service, config: Option<&Path>) -> Service {
    let same = |a: &Path, b: &Path| {
        a == b || std::fs::canonicalize(a).ok() == std::fs::canonicalize(b).ok()
    };
    let named = |path: &Path| match (&service.config, service.kind) {
        (Some(runs), _) => same(path, runs),
        (None, Kind::System) => Config::system_path().is_some_and(|system| same(path, &system)),
        (None, _) => false,
    };
    match config {
        Some(path) if service.kind != Kind::None && !named(path) => Service {
            kind: Kind::None,
            active: false,
            config: None,
            binary: None,
        },
        _ => service,
    }
}

/// The base configuration as merged so far: the file (or the template a user
/// install will write) and every overlay but ours.
fn base_table(base: &Base, ours: &Path) -> Result<toml::Table, CliError> {
    let text = match (&base.install, std::fs::read_to_string(&base.path)) {
        (_, Ok(text)) => text,
        (Some(install), Err(_)) => service::template_config(&install.state),
        (None, Err(e)) => {
            return Err(CliError::runtime(
                &format!("could not read {}", base.path.display()),
                e,
            ));
        }
    };
    let mut table: toml::Table = text
        .parse()
        .map_err(|e| CliError::Usage(format!("{} does not parse: {e}", base.path.display())))?;
    for overlay in overlay_files(&base.path) {
        if overlay == ours {
            continue;
        }
        let text = std::fs::read_to_string(&overlay)
            .map_err(|e| CliError::runtime(&format!("could not read {}", overlay.display()), e))?;
        let parsed: toml::Table = text
            .parse()
            .map_err(|e| CliError::Usage(format!("{} does not parse: {e}", overlay.display())))?;
        merge_toml(&mut table, parsed);
    }
    Ok(table)
}

/// Where the intercept's CA goes if the profile adds the intercept.
fn state_dir(base: &Base, table: &toml::Table) -> PathBuf {
    if let Some(install) = &base.install {
        return install.state.clone();
    }
    profile::lookup(table, &["refs", "path"])
        .and_then(toml::Value::as_str)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| service::state_home().join("kompressor"))
}

/// The proxy port and intercept endpoint of an effective configuration.
fn endpoints(config: &Config) -> Result<Endpoints, CliError> {
    let port = config
        .server
        .listen
        .rsplit(':')
        .next()
        .and_then(|port| port.parse::<u16>().ok())
        .ok_or_else(|| {
            CliError::Usage(format!(
                "server.listen = {:?} has no port this command can read",
                config.server.listen
            ))
        })?;
    let intercept = config
        .proxy
        .intercept
        .listen
        .as_ref()
        .filter(|listen| !listen.is_empty())
        .map(|listen| {
            (
                listen.clone(),
                config.proxy.intercept.ca_path.join("ca.pem"),
            )
        });
    Ok(Endpoints { port, intercept })
}

/// The address `/healthz` is asked on: the listener, with a wildcard host
/// replaced by loopback.
fn health_address(listen: &str) -> String {
    match listen.rsplit_once(':') {
        Some(("0.0.0.0" | "[::]" | "", port)) => format!("127.0.0.1:{port}"),
        _ => listen.to_string(),
    }
}

/// The agents a run covers: those named, or every installed one.
fn selected(named: Option<&[String]>) -> Result<Vec<&'static str>, CliError> {
    match named {
        Some(names) => names
            .iter()
            .map(|name| {
                agents::AGENTS
                    .iter()
                    .copied()
                    .find(|agent| agent == name)
                    .ok_or_else(|| {
                        CliError::Usage(format!(
                            "unknown agent {name:?}; setup knows {}",
                            agents::AGENTS.join(", ")
                        ))
                    })
            })
            .collect(),
        None => Ok(agents::AGENTS
            .iter()
            .copied()
            .filter(|agent| agents::installed(agent))
            .collect()),
    }
}

/// Ask before changing anything, unless told not to.
fn confirm(yes: bool, question: &str) -> Result<bool, CliError> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(CliError::Usage(
            "standard input is not a terminal, so there is no one to confirm the plan; pass \
             --yes to apply it, or --dry-run to only print it"
                .into(),
        ));
    }
    eprint!("{question} [y/N] ");
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| CliError::runtime("could not read the answer", e))?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// `~` for the home directory, so the plan fits on a line.
fn short(path: &Path) -> String {
    let home = service::home();
    match path.strip_prefix(&home) {
        Ok(rest) if home != Path::new(".") => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

fn state_word(before: Option<&str>, after: &str) -> &'static str {
    match before {
        None => "create",
        Some(before) if before == after => "unchanged",
        Some(_) => "update",
    }
}

/// A line diff, changed lines only, with one line of context.
pub fn diff(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    if a.len() * b.len() > 4_000_000 {
        return "  (too large to diff)\n".into();
    }
    // Longest common subsequence, then walk it.
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            ops.push(('+', b[j]));
            j += 1;
        } else {
            ops.push(('-', a[i]));
            i += 1;
        }
    }
    let near = |k: usize| {
        ops[k.saturating_sub(1)..(k + 2).min(ops.len())]
            .iter()
            .any(|(op, _)| *op != ' ')
    };
    let mut out = String::new();
    let mut skipped = false;
    for (k, (op, line)) in ops.iter().enumerate() {
        if *op == ' ' && !near(k) {
            skipped = true;
            continue;
        }
        if skipped {
            out.push_str("    …\n");
            skipped = false;
        }
        out.push_str(&format!("    {op} {line}\n"));
    }
    out
}

/// Write the overlay, through `sudo` when the directory is not ours to write.
fn write_overlay(path: &Path, text: &str) -> Result<(), CliError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    if service::writable(dir) {
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::runtime(&format!("could not create {}", dir.display()), e))?;
        return std::fs::write(path, text)
            .map_err(|e| CliError::runtime(&format!("could not write {}", path.display()), e));
    }
    let staged = std::env::temp_dir().join(format!("kompressor-setup-{}.toml", std::process::id()));
    std::fs::write(&staged, text)
        .map_err(|e| CliError::runtime(&format!("could not write {}", staged.display()), e))?;
    let result = service::run_visible(&service::elevated(
        true,
        &[
            "install",
            "-D",
            "-m",
            "0644",
            &staged.display().to_string(),
            &path.display().to_string(),
        ],
    ));
    let _ = std::fs::remove_file(&staged);
    result
}

fn delete_overlay(path: &Path) -> Result<(), CliError> {
    if !path.exists() {
        return Ok(());
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    if service::writable(dir) {
        return std::fs::remove_file(path)
            .map_err(|e| CliError::runtime(&format!("could not remove {}", path.display()), e));
    }
    service::run_visible(&service::elevated(
        true,
        &["rm", "-f", &path.display().to_string()],
    ))
}

/// Restart the service so it reads the overlay, and wait for it.
fn restart(service: &Service, overlay_dir: &Path, listen: &str) -> Result<(), CliError> {
    let Some(command) = service.restart_command() else {
        say!(
            "  restart your kompressor yourself so it reads {}",
            overlay_dir.display()
        );
        return Ok(());
    };
    let privileged = service.kind == Kind::System && !service::writable(Path::new("/etc"));
    let parts: Vec<&str> = command.iter().map(String::as_str).collect();
    service::run_visible(&service::elevated(privileged, &parts))?;
    if service::wait_healthy(&health_address(listen), Duration::from_secs(20)) {
        Ok(())
    } else {
        let journal = if service.kind == Kind::User {
            "journalctl --user -u kompressor -n 30"
        } else {
            "journalctl -u kompressor -n 30"
        };
        Err(CliError::Runtime(format!(
            "the service did not answer /healthz on {listen} within 20s after the restart. \
             No agent was changed. Look at `{journal}`; `kompressor check-config --config …` \
             shows whether the configuration loads; `kompressor setup remove` takes the \
             overlay out again."
        )))
    }
}

/// `kompressor setup`.
pub fn run(args: SetupArgs) -> CliResult {
    let mut manifest = Manifest::load()?;
    let base = resolve_base(args.config.as_deref(), &manifest)?;
    let overlay_path = overlay_dir(&base.path).join(profile::OVERLAY_NAME);
    let table = base_table(&base, &overlay_path)?;
    let (overlay_text, decisions) = profile::render(&table, &state_dir(&base, &table));

    let mut merged = table.clone();
    merge_toml(
        &mut merged,
        overlay_text
            .parse()
            .map_err(|e| CliError::Runtime(format!("the rendered profile does not parse: {e}")))?,
    );
    let effective = Config::from_table(merged).map_err(|e| {
        CliError::Usage(format!(
            "the configuration with the profile does not load: {e}"
        ))
    })?;
    effective
        .validate()
        .map_err(|e| CliError::Usage(e.to_string()))?;
    let endpoints = endpoints(&effective)?;
    let overlay_before = std::fs::read_to_string(&overlay_path).ok();
    let overlay_changes = overlay_before.as_deref() != Some(overlay_text.as_str());
    let blocked = ignores_overlays(&base.service);

    // Plan every agent; one that cannot be wired is reported, not fatal.
    let chosen = selected(args.agents.as_deref())?;
    let mut plans: Vec<Plan> = Vec::new();
    let mut skipped: Vec<(&str, String)> = Vec::new();
    for agent in &chosen {
        let previous = manifest.agents.get(*agent);
        let planned = match *agent {
            "claude" => agents::plan_claude(
                args.claude_mode,
                args.with_mcp,
                args.force,
                &endpoints,
                previous,
            ),
            "codex" => agents::plan_codex(&endpoints, previous),
            "opencode" => agents::plan_opencode(&endpoints, previous),
            _ => continue,
        };
        match planned {
            Ok(plan) => plans.push(plan),
            Err(CliError::Usage(why) | CliError::Runtime(why)) => skipped.push((agent, why)),
            Err(other) => return Err(other),
        }
    }

    // The plan.
    say!("kompressor setup\n");
    let service_line = match (&base.install, base.service.kind) {
        (Some(install), _) => format!(
            "install a systemd user service ({}), config {}",
            short(&install.unit),
            short(&install.config)
        ),
        (None, Kind::System) => {
            format!("system unit {} ({})", service::UNIT, active(&base.service))
        }
        (None, Kind::User) => format!("user unit {} ({})", service::UNIT, active(&base.service)),
        (None, Kind::None) => "no unit found; restart it yourself after setup".into(),
    };
    say!("  service   {service_line}");
    say!("            config {}", short(&base.path));
    say!(
        "  profile   {}  [{}]{}",
        short(&overlay_path),
        state_word(overlay_before.as_deref(), &overlay_text),
        if service::writable(overlay_path.parent().unwrap_or(Path::new("/"))) {
            ""
        } else {
            "  (via sudo)"
        }
    );
    say!(
        "            proxy on :{}, Claude profile, keep-warm, websocket relay",
        endpoints.port
    );
    for kept in &decisions.kept {
        say!("            kept from your config: {kept}");
    }
    if !decisions.mounts_added.is_empty() {
        say!(
            "            mounts added: {}",
            decisions.mounts_added.join(", ")
        );
    }
    for plan in &plans {
        say!(
            "  {:<9} {}  [{}]",
            plan.agent,
            short(&plan.file),
            state_word(plan.before.as_deref(), &plan.after)
        );
        for note in &plan.notes {
            say!("            {note}");
        }
    }
    for (agent, why) in &skipped {
        say!("  {agent:<9} SKIPPED: {why}");
    }
    if chosen.is_empty() {
        say!("  agents    none found on PATH; name them with --agents claude,codex,opencode");
    }
    if let Some(why) = &blocked {
        say!("\n  BLOCKED: {why}");
    }
    say!("");

    if args.dry_run {
        if overlay_changes {
            say!("--- {}", overlay_path.display());
            say!(
                "{}",
                diff(overlay_before.as_deref().unwrap_or(""), &overlay_text)
            );
        }
        for plan in plans.iter().filter(|plan| plan.changes()) {
            say!("--- {}", plan.file.display());
            say!(
                "{}",
                diff(plan.before.as_deref().unwrap_or(""), &plan.after)
            );
        }
        say!("dry run: nothing was changed");
        return Ok(());
    }
    if let Some(why) = blocked {
        return Err(CliError::Usage(format!("{why}. Nothing was changed.")));
    }

    let nothing_to_do = !overlay_changes
        && base.install.is_none()
        && plans
            .iter()
            .all(|plan| !plan.changes() && plan.post == Post::None);
    if nothing_to_do {
        say!("everything is already set up; `kompressor setup status` checks it end to end");
        return Ok(());
    }
    if !confirm(args.yes, "Apply this plan?")? {
        say!("nothing was changed");
        return Ok(());
    }

    // 1. The service and its overlay.
    if let Some(install) = &base.install {
        say!("installing the user service");
        install.write()?;
    }
    if overlay_changes {
        say!("writing {}", overlay_path.display());
        write_overlay(&overlay_path, &overlay_text)?;
    }
    manifest.kompressor = Some(KompressorRecord {
        config: base.path.clone(),
        overlay: overlay_path.clone(),
        service: base.service.kind,
        installed_unit: base
            .install
            .as_ref()
            .map(|install| install.unit.clone())
            .or_else(|| {
                manifest
                    .kompressor
                    .as_ref()
                    .and_then(|k| k.installed_unit.clone())
            }),
    });
    manifest.save()?;

    let listen = effective.server.listen.clone();
    if (overlay_changes || base.install.is_some()) && !args.no_restart {
        say!("restarting the service");
        restart(&base.service, &overlay_dir(&base.path), &listen)?;
    }
    if !service::healthy(&health_address(&listen)) {
        if !args.force {
            return Err(CliError::Runtime(format!(
                "nothing answers /healthz on {listen}, so no agent was pointed at it — an \
                 agent wired to a dead proxy fails every turn. Start the service and run setup \
                 again (or pass --force to wire the agents anyway)."
            )));
        }
        say!("warning: nothing answers on {listen}; wiring the agents anyway (--force)");
    }
    if let Some((_, ca)) = &endpoints.intercept
        && plans
            .iter()
            .any(|plan| plan.record.mode == Some(ClaudeMode::Intercept))
        && std::fs::File::open(ca).is_err()
    {
        say!(
            "warning: {} is not readable yet; Claude Code needs it for the intercept. It is \
             written when the service first starts.",
            ca.display()
        );
    }

    // 2. The agents.
    for plan in &plans {
        if plan.changes() {
            agents::write(plan)?;
        }
        match plan.post {
            Post::None => {}
            Post::OpenCodeReload => match crate::wrap::opencode::reload() {
                Ok(_) => {}
                Err(why) => say!("  opencode: {why}; the next opencode to start reads the file"),
            },
            Post::ClaudeMcp => {
                if let Err(why) = agents::register_claude_mcp(&base.path) {
                    say!("  claude: MCP registration failed: {why}");
                }
            }
        }
        // A run without --with-mcp after one with it takes the server back out.
        let had_mcp = manifest
            .agents
            .get(plan.agent)
            .is_some_and(|record| record.mcp);
        if had_mcp && !plan.record.mcp {
            let _ = agents::unregister_claude_mcp();
        }
        manifest
            .agents
            .insert(plan.agent.to_string(), plan.record.clone());
        manifest.save()?;
        say!("  {:<9} done", plan.agent);
    }

    say!(
        "\ndone. Restart open agent sessions to pick it up; Claude Desktop needs a full quit.\n\
         Check it end to end:  kompressor setup status\n\
         Undo all of it:       kompressor setup remove"
    );
    if !skipped.is_empty() {
        return Err(CliError::Runtime(format!(
            "{} agent(s) were skipped; see above",
            skipped.len()
        )));
    }
    Ok(())
}

/// Why the service would not read the profile, if it would not: its binary
/// predates configuration overlays. Writing the overlay anyway would change
/// nothing the service does while `status`, run by this newer binary, reported
/// the profile in force.
fn ignores_overlays(service: &Service) -> Option<String> {
    let binary = service.binary.as_ref()?;
    if service.kind == Kind::None || service::reads_overlays(binary) {
        return None;
    }
    let version = std::process::Command::new(binary)
        .arg("version")
        .output()
        .ok()
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "an unknown version".into());
    Some(format!(
        "the service runs {} ({version}), a build that does not read configuration overlays, \
         so the profile would be written and silently ignored. Install a build that does — this \
         one is {}; `kompressor check-config` naming an overlay on stderr is the sign — then run \
         setup again",
        binary.display(),
        std::env::current_exe()
            .map(|exe| exe.display().to_string())
            .unwrap_or_else(|_| "this kompressor".into())
    ))
}

fn active(service: &Service) -> &'static str {
    if service.active {
        "active"
    } else {
        "not running"
    }
}

/// `kompressor setup status`.
pub fn status(config: Option<&Path>) -> CliResult {
    let manifest = Manifest::load()?;
    let base = resolve_base(config, &manifest)?;
    if base.install.is_some() {
        say!("no kompressor service is installed; `kompressor setup` installs one");
        return Ok(());
    }
    let effective = Config::load(Some(&base.path)).map_err(|e| CliError::Usage(e.to_string()))?;
    let endpoints = endpoints(&effective)?;
    let listen = effective.server.listen.clone();
    let ours = overlay_dir(&base.path).join(profile::OVERLAY_NAME);

    println!(
        "service   {}",
        match base.service.kind {
            Kind::System => format!("system unit, {}", active(&base.service)),
            Kind::User => format!("user unit, {}", active(&base.service)),
            Kind::None => "no unit found".into(),
        }
    );
    println!("config    {}", base.path.display());
    for overlay in &effective.overlays {
        println!("overlay   {}", overlay.display());
    }
    let profile_state = match std::fs::read_to_string(&ours) {
        Err(_) => "not written (run `kompressor setup`)".to_string(),
        Ok(text) => {
            let table = base_table(&base, &ours)?;
            let (expected, _) = profile::render(&table, &state_dir(&base, &table));
            if text == expected {
                "current".into()
            } else {
                "out of date (run `kompressor setup` again)".into()
            }
        }
    };
    let profile_state = match ignores_overlays(&base.service) {
        Some(_) if ours.exists() => format!(
            "{profile_state}, but IGNORED: the service binary does not read overlays (upgrade it)"
        ),
        _ => profile_state,
    };
    println!("profile   {profile_state}");
    let health = |ok: bool| if ok { "ok" } else { "NOT ANSWERING" };
    println!(
        "proxy     {listen}  /healthz {}",
        health(service::healthy(&health_address(&listen)))
    );
    if let Some((intercept, ca)) = &endpoints.intercept {
        let listening = std::net::ToSocketAddrs::to_socket_addrs(intercept.as_str())
            .ok()
            .and_then(|mut addrs| addrs.next())
            .is_some_and(|addr| {
                std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
            });
        println!(
            "intercept {intercept}  {}  CA {}",
            if listening {
                "listening"
            } else {
                "NOT LISTENING"
            },
            if ca.is_file() {
                ca.display().to_string()
            } else {
                format!("{} MISSING", ca.display())
            }
        );
    }

    let traffic = recent_traffic(&effective);
    println!();
    println!(
        "{:<9} {:<10} {:<46} last 24h",
        "agent", "installed", "wired"
    );
    for agent in agents::AGENTS {
        let installed = if agents::installed(agent) {
            "yes"
        } else {
            "no"
        };
        let wired = agents::wired(agent, &endpoints).unwrap_or_else(|| "no".into());
        let seen = match &traffic {
            Ok(rows) => {
                let (requests, saved, tokens) = rows
                    .iter()
                    .filter(|row| agents::mounts(agent).contains(&row.provider.as_str()))
                    .fold((0, 0, 0), |(r, s, t), row| {
                        (r + row.requests, s + row.saved, t + row.tokens_in)
                    });
                if requests == 0 {
                    "no requests".to_string()
                } else {
                    format!(
                        "{requests} requests, {:.1}% removed",
                        100.0 * saved as f64 / tokens.max(1) as f64
                    )
                }
            }
            Err(_) => "-".into(),
        };
        let wired = if wired.chars().count() > 46 {
            format!("{}…", wired.chars().take(45).collect::<String>())
        } else {
            wired
        };
        println!("{agent:<9} {installed:<10} {wired:<46} {seen}");
    }
    if let Err(why) = traffic {
        println!("\n(traffic unavailable: {why})");
    }
    if let Some(record) = manifest.agents.get("claude")
        && record.mcp
    {
        println!(
            "\nclaude: retrieval MCP server registered as `{}`",
            crate::wrap::mcp::SERVER_NAME
        );
    }
    Ok(())
}

/// Requests per upstream mount over the last day, from the service's own
/// statistics database.
fn recent_traffic(
    config: &Config,
) -> Result<Vec<kompressor_stats::reader::ProviderCoverage>, String> {
    let reader = kompressor_stats::StatsReader::open(&config.stats.path)
        .map_err(|e| format!("{}: {e}", config.stats.path.display()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    reader
        .by_provider("", now - 24 * 3600 * 1000, now)
        .map_err(|e| e.to_string())
}

/// `kompressor setup remove`.
pub fn remove(args: RemoveArgs) -> CliResult {
    let mut manifest = Manifest::load()?;
    let chosen: Vec<String> = match &args.agents {
        Some(named) => {
            selected(Some(named))?;
            named.clone()
        }
        None => manifest.agents.keys().cloned().collect(),
    };
    let remove_profile = args.agents.is_none() && !args.keep_profile;

    let mut undos: Vec<(String, Record, Undo, Vec<String>)> = Vec::new();
    for agent in &chosen {
        let Some(record) = manifest.agents.get(agent) else {
            say!("  {agent:<9} nothing recorded");
            continue;
        };
        let (undo, changed) = agents::undo(record)?;
        undos.push((agent.clone(), record.clone(), undo, changed));
    }
    let overlay = manifest
        .kompressor
        .as_ref()
        .filter(|_| remove_profile)
        .map(|k| k.overlay.clone());
    if undos.is_empty() && overlay.is_none() {
        say!(
            "nothing to remove: no setup is recorded in {}",
            Manifest::path().display()
        );
        return Ok(());
    }

    say!("kompressor setup remove\n");
    for (agent, record, undo, changed) in &undos {
        let what = match undo {
            Undo::Write(_) => "restore",
            Undo::Delete => "delete (setup created it)",
            Undo::Nothing => "nothing left to undo",
        };
        say!("  {agent:<9} {}  [{what}]", short(&record.file));
        for key in changed {
            say!("            {key}: changed since setup, left as it is");
        }
        if record.mcp {
            say!(
                "            unregister MCP server `{}`",
                crate::wrap::mcp::SERVER_NAME
            );
        }
    }
    if let Some(overlay) = &overlay {
        say!(
            "  profile   {}  [delete, then restart the service]",
            short(overlay)
        );
    }
    say!("");
    if args.dry_run {
        for (_, record, undo, _) in &undos {
            if let Undo::Write(next) = undo {
                let current = std::fs::read_to_string(&record.file).unwrap_or_default();
                say!("--- {}", record.file.display());
                say!("{}", diff(&current, next));
            }
        }
        say!("dry run: nothing was changed");
        return Ok(());
    }
    if !confirm(args.yes, "Undo this?")? {
        say!("nothing was changed");
        return Ok(());
    }

    // Agents first: none is left pointing at a proxy whose settings are going.
    for (agent, record, undo, _) in undos {
        match undo {
            Undo::Write(text) => std::fs::write(&record.file, text).map_err(|e| {
                CliError::runtime(&format!("could not write {}", record.file.display()), e)
            })?,
            Undo::Delete => {
                let _ = std::fs::remove_file(&record.file);
            }
            Undo::Nothing => {}
        }
        if record.mcp
            && let Err(why) = agents::unregister_claude_mcp()
        {
            say!("  claude: {why}");
        }
        if agent == "opencode"
            && let Err(why) = crate::wrap::opencode::reload()
        {
            say!("  opencode: {why}");
        }
        manifest.agents.remove(&agent);
        manifest.save()?;
        say!("  {agent:<9} undone");
    }

    if let (Some(overlay), Some(record)) = (overlay, manifest.kompressor.clone()) {
        delete_overlay(&overlay)?;
        let service = runs(Service::detect(), Some(&record.config));
        if service.kind != Kind::None {
            let effective =
                Config::load(Some(&record.config)).map_err(|e| CliError::Usage(e.to_string()))?;
            restart(
                &service,
                &overlay_dir(&record.config),
                &effective.server.listen,
            )?;
        }
        if let Some(unit) = &record.installed_unit {
            say!(
                "\nthe user service setup installed is still running ({}). To remove it too:\n  \
                 systemctl --user disable --now kompressor && rm {}",
                unit.display(),
                unit.display()
            );
        }
        manifest.kompressor = None;
        manifest.save()?;
    }
    say!("\ndone. Restart open agent sessions; Claude Desktop needs a full quit.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_diff_shows_changed_lines_with_context() {
        let before = "a\nb\nc\nd\ne\nf\n";
        let after = "a\nb\nc\nX\ne\nf\n";
        let shown = diff(before, after);
        assert!(shown.contains("- d"), "{shown}");
        assert!(shown.contains("+ X"), "{shown}");
        assert!(shown.contains("  c") && shown.contains("  e"), "{shown}");
        assert!(!shown.contains("  a"), "{shown}");
    }

    #[test]
    fn a_wildcard_listener_is_asked_on_loopback() {
        assert_eq!(health_address("0.0.0.0:8817"), "127.0.0.1:8817");
        assert_eq!(health_address("127.0.0.1:18817"), "127.0.0.1:18817");
    }

    #[test]
    fn an_unknown_agent_is_refused() {
        assert!(selected(Some(&["cursor".to_string()])).is_err());
        assert_eq!(
            selected(Some(&["codex".to_string()])).unwrap(),
            vec!["codex"]
        );
    }

    #[test]
    fn the_manifest_round_trips() {
        let mut manifest = Manifest {
            version: 1,
            ..Manifest::default()
        };
        manifest.agents.insert(
            "codex".into(),
            Record {
                file: "/x/config.toml".into(),
                existed: true,
                block: true,
                ..Record::default()
            },
        );
        let text = serde_json::to_string(&manifest).unwrap();
        let back: Manifest = serde_json::from_str(&text).unwrap();
        assert_eq!(back.agents["codex"], manifest.agents["codex"]);
    }
}

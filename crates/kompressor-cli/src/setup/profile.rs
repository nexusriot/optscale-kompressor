//! The recommended profile `kompressor setup` writes, as one overlay file.
//!
//! The profile is the packaged configuration's settings with the measured
//! Claude Code profile under `[proxy.providers.anthropic]`, plus keep-warm and
//! the WebSocket relay pinned explicitly — pinned rather than left to the
//! compiled-in defaults, so a later change of default does not silently
//! change a machine that was set up deliberately.
//!
//! Two kinds of setting are *not* forced, because the base configuration may
//! already have made a decision about them that the overlay would break:
//!
//! * **Addresses** (`server.listen`, `server.stats_listen`,
//!   `proxy.intercept`). Every agent setup wires points at the listener the
//!   service actually runs; moving it under them would point them at nothing.
//!   They are written only where the base names none.
//! * **Upstream mounts.** A mount the base already has, by name or by path,
//!   may be routed to the user's own gateway. Only missing mounts are added.

use std::path::Path;

/// The file name of the overlay this command owns, inside the config's
/// overlay directory. `50-` leaves room for overlays meant to come before and
/// after it.
pub const OVERLAY_NAME: &str = "50-setup.toml";

/// Default addresses, used only where the base configuration names none.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8817";
pub const DEFAULT_STATS_LISTEN: &str = "127.0.0.1:8818";
pub const DEFAULT_INTERCEPT_LISTEN: &str = "127.0.0.1:8840";

/// One `[[proxy.upstreams]]` entry of the packaged configuration.
struct Mount {
    name: &'static str,
    path_prefix: &'static str,
    /// The rest of the entry, rendered.
    body: &'static str,
}

/// Every mount the agents setup wires need, as the packaged configuration
/// defines them.
const MOUNTS: &[Mount] = &[
    Mount {
        name: "openai",
        path_prefix: "/v1",
        body: "base_url = \"https://api.openai.com\"\napi_family = \"openai\"\nforward_auth = true\n\n\
               [proxy.upstreams.subscription]\nbase_url = \"https://chatgpt.com\"\n\
               rewrite_prefix = \"/backend-api/codex\"\n",
    },
    Mount {
        name: "codex-desktop-chatgpt",
        path_prefix: "/backend-api/codex",
        body: "base_url = \"https://chatgpt.com\"\nstrip_prefix = false\napi_family = \"openai\"\n\
               forward_auth = true\n",
    },
    Mount {
        name: "chatgpt-rest",
        path_prefix: "/backend-api",
        body: "base_url = \"https://chatgpt.com\"\napi_family = \"passthrough\"\nforward_auth = true\n",
    },
    Mount {
        name: "anthropic",
        path_prefix: "/v1/messages",
        body: "base_url = \"https://api.anthropic.com\"\napi_family = \"anthropic\"\n\
               forward_auth = true\n",
    },
    Mount {
        name: "anthropic-api",
        path_prefix: "/api",
        body: "base_url = \"https://api.anthropic.com\"\napi_family = \"passthrough\"\n\
               forward_auth = true\n",
    },
    Mount {
        name: "opencode-zen",
        path_prefix: "/zen",
        body: "base_url = \"https://opencode.ai\"\napi_family = \"auto\"\nforward_auth = true\n",
    },
];

/// What the profile decided about the settings it does not force, for the
/// plan it prints.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Decisions {
    /// Addresses the base already names and the profile left alone.
    pub kept: Vec<String>,
    /// Mounts added because the base lacked them.
    pub mounts_added: Vec<&'static str>,
    /// Whether the intercept listener came from the profile.
    pub intercept_added: bool,
}

/// Render the overlay for a base configuration, as merged so far (the base
/// file and every overlay except this command's own).
///
/// `state_dir` is where the intercept's certificate authority goes when the
/// profile adds the intercept: it must be a directory the service may write,
/// which for the systemd user unit is its state directory and nothing else.
pub fn render(base: &toml::Table, state_dir: &Path) -> (String, Decisions) {
    let mut decisions = Decisions::default();
    let mut out = String::from(
        "# Managed by `kompressor setup`: rewritten on every run, deleted by\n\
         # `kompressor setup remove`. Edits here are lost; put your own settings in\n\
         # the main configuration file, or in another overlay named to sort after\n\
         # this one (for example 90-local.toml).\n\
         #\n\
         # The recommended profile: the packaged settings, with the measured Claude\n\
         # Code profile, keep-warm and the WebSocket relay pinned explicitly.\n\n",
    );

    let mut server = String::new();
    for (key, default) in [
        ("listen", DEFAULT_LISTEN),
        ("stats_listen", DEFAULT_STATS_LISTEN),
    ] {
        match lookup(base, &["server", key]).and_then(toml::Value::as_str) {
            Some(set) if !set.is_empty() => decisions.kept.push(format!("server.{key} = {set}")),
            _ => server.push_str(&format!("{key} = \"{default}\"\n")),
        }
    }
    if !server.is_empty() {
        out.push_str("[server]\n");
        out.push_str(&server);
        out.push('\n');
    }

    out.push_str(
        "[engine]\n\
         mode = \"recoverable\"\n\
         # Recoverability is dropped for a fragment larger than this.\n\
         max_ref_bytes = 1048576\n\n\
         [proxy]\n\
         retrieve_tool = \"auto\"\n\
         frozen_prefix = \"marker\"\n\
         cache_ttl = \"default\"\n\
         # Codex on a ChatGPT plan speaks WebSocket; relaying keeps it compressed.\n\
         websocket_relay = true\n\
         # Keep-warm: while a tool runs, re-read the cached prefix every 4.5 minutes\n\
         # so the five-minute cache entry does not lapse; at most 11 pings per gap.\n\
         cache_keep_warm_interval = \"270s\"\n\
         cache_keep_warm_max_pings = 11\n\
         cache_keep_warm_background_pings = 4\n\n\
         # Claude Code (docs/CLAUDE-CODE.md). `first_seen` is what turns compression on.\n\
         [proxy.providers.anthropic]\n\
         retrieve_tool = \"always\"\n\
         frozen_prefix = \"first_seen\"\n\
         cache_ttl = \"adaptive\"\n\
         cache_keep_warm = true\n\n\
         [proxy.providers.anthropic.engine]\n\
         enable_code_aware = true\n\
         min_elision_saving_chars = 4000\n",
    );

    match lookup(base, &["proxy", "intercept", "listen"]).and_then(toml::Value::as_str) {
        Some(set) if !set.is_empty() => decisions
            .kept
            .push(format!("proxy.intercept.listen = {set}")),
        _ => {
            decisions.intercept_added = true;
            let ca = state_dir.join("ca");
            out.push_str(&format!(
                "\n# The CONNECT listener Claude Code and Claude Desktop's Code tab are pointed\n\
                 # at (docs/CLAUDE-DESKTOP.md). TLS is terminated only for these hosts.\n\
                 [proxy.intercept]\n\
                 listen = \"{DEFAULT_INTERCEPT_LISTEN}\"\n\
                 intercept_hosts = [\"api.anthropic.com\"]\n\
                 ca_path = {}\n\
                 alpn = [\"h2\", \"http/1.1\"]\n",
                toml_string(&ca.display().to_string())
            ));
        }
    }

    let existing: Vec<(Option<&str>, Option<&str>)> = lookup(base, &["proxy", "upstreams"])
        .and_then(toml::Value::as_array)
        .map(|mounts| {
            mounts
                .iter()
                .map(|mount| {
                    (
                        mount.get("name").and_then(toml::Value::as_str),
                        mount.get("path_prefix").and_then(toml::Value::as_str),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    for mount in MOUNTS {
        let present = existing.iter().any(|(name, prefix)| {
            *name == Some(mount.name)
                || prefix.map(|p| p.trim_end_matches('/')) == Some(mount.path_prefix)
        });
        if present {
            continue;
        }
        decisions.mounts_added.push(mount.name);
        out.push_str(&format!(
            "\n[[proxy.upstreams]]\nname = \"{}\"\npath_prefix = \"{}\"\n{}",
            mount.name, mount.path_prefix, mount.body
        ));
    }
    (out, decisions)
}

/// The value at a key path, if every table on the way exists.
pub fn lookup<'a>(table: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (last, parents) = path.split_last()?;
    let mut current = table;
    for part in parents {
        current = current.get(*part)?.as_table()?;
    }
    current.get(*last)
}

/// A TOML basic string.
fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kompressor_core::Config;

    fn merged(base: &str) -> (Config, Decisions) {
        let mut table: toml::Table = base.parse().unwrap();
        let (overlay, decisions) = render(&table, Path::new("/state/kompressor"));
        let overlay: toml::Table = overlay.parse().expect("the overlay is valid TOML");
        kompressor_core::config::merge_toml(&mut table, overlay);
        let config = Config::from_table(table).expect("the merged config deserializes");
        config.validate().expect("and validates");
        (config, decisions)
    }

    #[test]
    fn an_empty_base_gets_the_whole_profile() {
        let (config, decisions) = merged("");
        assert_eq!(config.server.listen, DEFAULT_LISTEN);
        assert_eq!(config.server.stats_listen, DEFAULT_STATS_LISTEN);
        assert!(decisions.intercept_added);
        assert_eq!(decisions.mounts_added.len(), MOUNTS.len());
        assert_eq!(config.proxy.upstreams.len(), MOUNTS.len());
        let rendered = config.redacted_toml();
        assert!(rendered.contains("first_seen"), "{rendered}");
        assert!(rendered.contains("/state/kompressor/ca"), "{rendered}");
    }

    #[test]
    fn addresses_and_mounts_the_base_has_are_left_alone() {
        let (config, decisions) = merged(
            r#"
            [server]
            listen = "127.0.0.1:18817"
            stats_listen = "127.0.0.1:18818"
            [proxy.intercept]
            listen = "127.0.0.1:18840"
            ca_path = "/var/lib/kompressor/ca"
            [[proxy.upstreams]]
            name = "my-gateway"
            path_prefix = "/v1"
            base_url = "https://gateway.example"
            "#,
        );
        assert_eq!(config.server.listen, "127.0.0.1:18817");
        assert_eq!(config.server.stats_listen, "127.0.0.1:18818");
        assert!(!decisions.intercept_added);
        assert_eq!(decisions.kept.len(), 3);
        assert!(
            !decisions.mounts_added.contains(&"openai"),
            "/v1 is the user's"
        );
        let v1: Vec<_> = config
            .proxy
            .upstreams
            .iter()
            .filter(|mount| mount.path_prefix == "/v1")
            .collect();
        assert_eq!(v1.len(), 1);
        assert_eq!(v1[0].base_url, "https://gateway.example");
    }

    #[test]
    fn rendering_is_deterministic() {
        let base: toml::Table = "[server]\nlisten = \"127.0.0.1:9\"\n".parse().unwrap();
        let state = Path::new("/s");
        assert_eq!(render(&base, state).0, render(&base, state).0);
    }
}

# Setting up a machine: `kompressor setup`

`kompressor wrap` compresses one session and leaves nothing behind.
`kompressor setup` does the opposite on purpose: one command points the
Kompressor service and every agent on the machine at each other, durably — the
CLIs, Claude Desktop's Code tab, the Codex app, OpenCode Desktop — with no
terminal held open. One command takes it all back out again.

```bash
kompressor setup --dry-run        # the plan, and the diff of every file it would touch
kompressor setup                  # the same plan, then a confirmation, then the changes
kompressor setup status           # is it wired, does the proxy answer, is traffic arriving
kompressor setup remove           # put every file back as it was
```

## Contents

- [What it does](#what-it-does)
- [The profile](#the-profile)
- [The agents](#the-agents)
- [Status](#status)
- [Removing it](#removing-it)
- [Flags](#flags)
- [Setup and `wrap`](#setup-and-wrap)
- [Limits](#limits)

## What it does

1. **Finds the service.** The systemd unit `kompressor.service`, system first,
   then user. Its configuration file is read off the unit's own `ExecStart
   … --config PATH`, not guessed. With no unit at all, on a systemd machine,
   setup installs the same user service `scripts/service/install.sh` does.
2. **Writes the profile into an overlay** — `<config>.d/50-setup.toml`, beside
   the configuration (`/etc/kompressor/config.d/50-setup.toml` for the package).
   The configuration you, or dpkg, own is never edited. An overlay in
   `/etc` is written with `sudo`, which asks for your password on the terminal.
3. **Restarts the service and waits for `/healthz`.** If it does not answer
   within 20 seconds, setup stops there and no agent is touched: an agent
   pointed at a dead proxy fails every turn.
4. **Wires each agent** found on `PATH` (or named with `--agents`), and records
   what each edit replaced in `~/.config/kompressor/setup.json` (mode 0600).

Running it again is safe. It recomputes everything, shows `[unchanged]` where
nothing moved, and keeps the *first* run's originals, so `remove` always goes
back to the state before setup ever ran.

### Configuration overlays

Any configuration file `NAME.toml` is read together with every `*.toml` in
`NAME.d/` beside it, in file-name order, later files winning. Tables merge key
by key; `[[proxy.upstreams]]` and `[[mcp.servers]]` entries merge by `name`, so
an overlay can add a mount without repeating the others. `kompressor
check-config` lists the overlays it read. Setup owns only `50-setup.toml`; put
your own overrides in a file that sorts after it, such as `90-local.toml`.

## The profile

The packaged settings, the measured Claude Code profile under
`[proxy.providers.anthropic]`, and keep-warm and the WebSocket relay pinned
explicitly, so a later change of compiled-in default does not silently change a
machine that was set up on purpose:

| Setting | Value |
| --- | --- |
| `engine.mode` / `engine.max_ref_bytes` | `recoverable` / 1 MiB |
| `proxy.websocket_relay` | `true` |
| `proxy.cache_keep_warm_interval` / `_max_pings` / `_background_pings` | `270s` / 11 / 4 |
| `proxy.providers.anthropic.retrieve_tool` | `always` |
| `proxy.providers.anthropic.frozen_prefix` | `first_seen` — the one that turns compression on for Claude Code |
| `proxy.providers.anthropic.cache_ttl` | `adaptive` |
| `proxy.providers.anthropic.cache_keep_warm` | `true` |
| `proxy.providers.anthropic.engine.enable_code_aware` | `true` |
| `proxy.providers.anthropic.engine.min_elision_saving_chars` | 4000 |

Two kinds of setting are only filled in where your configuration has none,
because changing them would break something you already decided:

- **Addresses.** `server.listen` (default `127.0.0.1:8817`),
  `server.stats_listen` (`127.0.0.1:8818`) and the intercept listener
  (`127.0.0.1:8840`, its CA under the service's state directory). A service
  already on other ports keeps them, and the agents are pointed at those.
- **Upstream mounts.** `openai` (with its ChatGPT-plan route),
  `codex-desktop-chatgpt`, `chatgpt-rest`, `anthropic`, `anthropic-api` and
  `opencode-zen` are added only when no mount of that name or path exists, so a
  mount you routed to your own gateway stays yours.

## The agents

| Agent | File | What is set |
| --- | --- | --- |
| Claude Code | `~/.claude/settings.json` (`$CLAUDE_CONFIG_DIR`) | `env.HTTPS_PROXY` and `env.NODE_EXTRA_CA_CERTS` (`--claude-mode intercept`, the default), or `env.ANTHROPIC_BASE_URL` and `env.ENABLE_TOOL_SEARCH` (`--claude-mode base-url`) |
| Codex | `~/.codex/config.toml` (`$CODEX_HOME`) | a marker-delimited block; its content depends on the login in `auth.json` |
| opencode | `~/.config/opencode/opencode.json`, or `opencode.jsonc` when it exists | `providers.<id>.settings.baseURL` for `openai`, `anthropic`, `opencode`, `opencode-go`; `transport = "http"` for `openai`. Then `opencode reload` |

**Claude Code.** The intercept is the default because it is the only route that
also reaches Claude Desktop's Code tab, which overwrites `ANTHROPIC_BASE_URL`
when it starts a session ([CLAUDE-DESKTOP.md](CLAUDE-DESKTOP.md)).
`HTTPS_PROXY` reaches every command Claude Code runs as well; hosts other than
`api.anthropic.com` are tunnelled without being read, but they do go through
the service, so remove the setup before stopping the service for good. An
`HTTPS_PROXY` or `NODE_EXTRA_CA_CERTS` you already set to something else is
not replaced without `--force`. `--with-mcp` also registers the retrieval tools
as a user-scope MCP server (`claude mcp add --scope user optscale_kompressor`),
so a `ref:k_…` handle resolves outside a proxied request too. That
registration is durable, which is why it is opt-in.

**Codex.** On a ChatGPT plan the block is one key,
`openai_base_url = "http://127.0.0.1:PORT/backend-api/codex"` — the mount Codex
Desktop calls, forwarded to chatgpt.com with the plan's token. Observed on
codex 0.166.0: with that key in the file, plan traffic reached chatgpt.com
through this mount, compressed. The `/v1` URL in the file is what made codex
0.164.0 send an API key instead ([CODEX.md](CODEX.md#permanent-setup)). The
Codex app reads the same file. With an API key the block is the custom provider described in
[CODEX.md](CODEX.md#permanent-setup). A top-level key of your own with the same
name is commented out while the block is there and comes back on `remove`. A
file that already says exactly what the block would is left untouched.

**opencode.** A provider whose `baseURL` already points somewhere other than
this proxy is a gateway of yours and is left alone. A file with comments or
trailing commas is refused rather than rewritten without them. OpenCode Desktop
needs 2.2.4 or newer ([OPENCODE.md](OPENCODE.md#opencode-desktop-needs-224-or-newer));
setup warns about an older one.

Restart open agent sessions afterwards; Claude Desktop needs a full quit.

## Status

On a machine wired by hand before setup existed, with the package's service
moved to other ports:

```console
$ kompressor setup status
service   system unit, active
config    /etc/kompressor/config.toml
profile   not written (run `kompressor setup`)
proxy     127.0.0.1:18817  /healthz ok
intercept 127.0.0.1:18840  listening  CA /var/lib/kompressor/ca/ca.pem

agent     installed  wired                                          last 24h
claude    yes        intercept via HTTPS_PROXY=http://127.0.0.1:18… 725 requests, 6.2% removed
codex     yes        openai_base_url=http://127.0.0.1:18817/backen… no requests
opencode  yes        providers opencode, opencode-go                no requests
```

"Wired" is read from the agent's files as they are now, not from the manifest,
so a hand edit or a `wrap` session shows up as it is. The traffic column comes
from the service's statistics database, by upstream mount; an agent that is
wired and shows no requests after you used it is the silent bypass to look into.

## Removing it

```bash
kompressor setup remove                      # everything
kompressor setup remove --agents opencode    # one agent; the profile stays
kompressor setup remove --keep-profile       # every agent; the profile stays
```

Agents go first, so none is left pointing at a proxy whose settings are about
to change; then the overlay is deleted and the service restarted. Each JSON key
goes back to its value before the first setup run — but only while it still
holds what setup wrote; a key you have changed since is yours and is left as it
is. When nothing else changed, the file comes back byte for byte, formatting
included. A file setup created and that holds nothing else is deleted. A user
service setup installed is left running, with the two commands that remove it.

## Flags

| Flag | Effect |
| --- | --- |
| `--agents claude,codex,opencode` | Wire only these. Default: every one on `PATH` |
| `--config PATH` | The configuration to set up. Default: the one the unit runs. A file the running unit does not use is set up without restarting the unit |
| `--claude-mode intercept\|base-url` | How Claude Code reaches the proxy (above) |
| `--with-mcp` | Also register the retrieval tools with Claude Code. A later run without it takes them out again |
| `--dry-run` | Print the plan and a diff of every file; change nothing |
| `-y`, `--yes` | Do not ask. Required when standard input is not a terminal |
| `--no-restart` | Write the overlay but leave the service running as it is |
| `--force` | Replace an agent setting that points elsewhere, and wire the agents even if the service does not answer |

## Setup and `wrap`

They coexist. `wrap` strips and restores only its own blocks and keys, so a
`wrap codex` session on top of a setup leaves the setup's block as it found it,
and `setup remove` does not touch a block a running `wrap` holds. A wrapped
session goes through the proxy `wrap` points it at for its duration, and back to
the setup's afterwards.

## Limits

- **The service has to read overlays.** A service running a build from before
  overlays existed would ignore `50-setup.toml` without a word. Setup asks the
  service's own binary (the one its unit runs) whether it applies an overlay,
  and refuses to write the profile until it does: upgrade the installed
  package first.

- **Installing a service** is automated only for systemd. On macOS and Windows,
  install the package ([MACOS.md](MACOS.md), [WINDOWS.md](WINDOWS.md)) and run
  setup with `--config` naming its configuration; it writes the overlay and the
  agents, and tells you to restart the service yourself.
- **Claude Desktop's Chat tab** is not an API client and cannot be pointed at a
  proxy; [MCP-PROXY.md](MCP-PROXY.md) covers what can be done for it.
- **Cursor and aider** are not wired. Cursor needs a base URL typed into its
  settings (`kompressor wrap cursor` prints it), and aider is launched per
  session, which is what `wrap` is for.

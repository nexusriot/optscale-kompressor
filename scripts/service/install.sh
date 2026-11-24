#!/usr/bin/env bash
# Install kompressor as a systemd USER service — no root, no system-wide state.
#
#   scripts/service/install.sh              # install, enable, start
#   scripts/service/install.sh --no-start   # install and enable, start later
#   scripts/service/install.sh --force      # overwrite an existing config
#
# A user service is the right shape for a proxy an agent on this login talks
# to: it runs as you, reads and writes only your files, and dies with your
# session unless you ask otherwise (see `loginctl enable-linger` at the end).
# For a machine-wide daemon, install the .deb instead — `make deb`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# The templates live in the CLI crate, which embeds them for `kompressor setup`;
# one copy, so the script and the command cannot drift apart.
TEMPLATES="$ROOT/crates/kompressor-cli/src/setup/templates"

UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/kompressor"
CONF="$CONF_DIR/config.toml"
UNIT="$UNIT_DIR/kompressor.service"
# systemd's StateDirectory= for a user unit resolves here; the unit and the
# config have to agree about it, so it is computed once and substituted into
# both rather than written twice.
STATE="${XDG_STATE_HOME:-$HOME/.local/state}/kompressor"

start=1
force=0
for arg in "$@"; do
  case "$arg" in
    --no-start) start=0 ;;
    --force)    force=1 ;;
    -h|--help)  sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

command -v systemctl >/dev/null || { echo "systemd is required" >&2; exit 1; }

# The binary, in the order someone is likely to want it: an installed copy
# first, then this checkout's release build. Recorded as an absolute path,
# because a unit has no PATH worth relying on.
BIN="${KOMPRESSOR_BIN:-}"
if [ -z "$BIN" ]; then
  for candidate in "$HOME/.local/bin/kompressor" "$(command -v kompressor || true)" "$ROOT/target/release/kompressor"; do
    [ -n "$candidate" ] && [ -x "$candidate" ] && { BIN="$candidate"; break; }
  done
fi
[ -n "$BIN" ] && [ -x "$BIN" ] || {
  echo "no kompressor binary found." >&2
  echo "  build one with 'make release', install one, or set KOMPRESSOR_BIN=/path/to/kompressor" >&2
  exit 1
}
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

mkdir -p "$UNIT_DIR" "$CONF_DIR" "$STATE"

# --- configuration -----------------------------------------------------------
# Never clobbered without --force: this file is where an upstream key or a
# changed mount lives, and losing it to a reinstall is not recoverable.
if [ -e "$CONF" ] && [ "$force" -eq 0 ]; then
  echo "keeping existing $CONF (pass --force to replace it)"
else
  [ -e "$CONF" ] && cp -a "$CONF" "$CONF.bak.$(date +%Y%m%d%H%M%S)" && echo "backed up the previous config"
  sed "s|@STATE@|$STATE|g" "$TEMPLATES/config.toml.in" > "$CONF"
  chmod 600 "$CONF"   # it is where an API key would go
  echo "wrote $CONF"
fi

# --- unit --------------------------------------------------------------------
sed -e "s|@BIN@|$BIN|g" -e "s|@CONFIG@|$CONF|g" -e "s|@STATE@|$STATE|g" \
    "$TEMPLATES/kompressor.service.in" > "$UNIT"
echo "wrote $UNIT"

# --- verify before enabling --------------------------------------------------
# A unit that starts and then dies on a bad config is a worse outcome than a
# failure here, where the message is still on the terminal.
if ! "$BIN" check-config --config "$CONF" >/dev/null 2>&1; then
  echo >&2
  echo "the configuration does not load:" >&2
  "$BIN" check-config --config "$CONF" >&2 || true
  echo >&2
  echo "the unit was written but NOT enabled; fix $CONF and re-run." >&2
  exit 1
fi

systemctl --user daemon-reload
systemctl --user enable kompressor.service >/dev/null
echo "enabled kompressor.service (user)"

if [ "$start" -eq 1 ]; then
  systemctl --user restart kompressor.service
  sleep 1
  if systemctl --user is-active --quiet kompressor.service; then
    echo "started — listening on $(grep -m1 '^listen' "$CONF" | cut -d'"' -f2)"
  else
    echo "failed to start:" >&2
    systemctl --user status kompressor.service --no-pager -n 15 >&2 || true
    exit 1
  fi
fi

cat <<EOF

  binary   $BIN
  config   $CONF
  state    $STATE
  unit     $UNIT

  status   systemctl --user status kompressor
  logs     journalctl --user -u kompressor -f
  remove   scripts/service/uninstall.sh

A user service stops when your last session ends. To keep it running while you
are logged out:  loginctl enable-linger $USER
EOF

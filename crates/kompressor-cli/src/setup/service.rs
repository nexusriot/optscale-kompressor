//! Finding the kompressor service an agent will be pointed at, and the
//! configuration it runs under.
//!
//! The durable wiring `setup` writes is only as good as the service behind it:
//! an agent pointed at a port nobody listens on fails every turn. So the
//! service is found first, its configuration is read from its own command line
//! rather than guessed, and every address the agents get comes from that.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::CliError;

/// The unit name the packages and `scripts/service/install.sh` install.
pub const UNIT: &str = "kompressor.service";

/// Which supervisor runs the service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The packaged system unit (`/usr/lib/systemd/system`).
    System,
    /// A systemd user unit.
    User,
    /// No unit this command can see: launchd, a Windows service, or a
    /// `kompressor serve` in a terminal.
    None,
}

/// What was found.
#[derive(Clone, Debug)]
pub struct Service {
    pub kind: Kind,
    pub active: bool,
    /// The `--config` on the unit's command line, if it names one.
    pub config: Option<PathBuf>,
    /// The binary the unit runs, when it says.
    pub binary: Option<PathBuf>,
}

impl Service {
    /// The service, looking at the system manager first: when both exist the
    /// system one is the one the package maintains.
    pub fn detect() -> Service {
        for kind in [Kind::System, Kind::User] {
            if let Some(found) = show(kind) {
                return found;
            }
        }
        Service {
            kind: Kind::None,
            active: false,
            config: None,
            binary: None,
        }
    }

    /// The command that restarts it, for printing and for running.
    pub fn restart_command(&self) -> Option<Vec<String>> {
        let mut command = match self.kind {
            Kind::System => vec!["systemctl".to_string()],
            Kind::User => vec!["systemctl".into(), "--user".into()],
            Kind::None => return None,
        };
        command.push("restart".into());
        command.push(UNIT.into());
        Some(command)
    }
}

/// Whether `systemctl` is on this machine at all.
pub fn has_systemd() -> bool {
    cfg!(target_os = "linux")
        && Command::new("systemctl")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

/// `systemctl show` for the unit under one manager, if it is installed there.
fn show(kind: Kind) -> Option<Service> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let mut command = Command::new("systemctl");
    if kind == Kind::User {
        command.arg("--user");
    }
    let output = command
        .args([
            "show",
            UNIT,
            "--no-pager",
            "-p",
            "LoadState",
            "-p",
            "ActiveState",
            "-p",
            "ExecStart",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let value = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .unwrap_or("")
            .to_string()
    };
    if value("LoadState") != "loaded" {
        return None;
    }
    Some(Service {
        kind,
        active: value("ActiveState") == "active",
        config: config_from_exec_start(&value("ExecStart")),
        binary: binary_from_exec_start(&value("ExecStart")),
    })
}

/// The program out of systemd's rendering of `ExecStart`: `{ path=/usr/bin/k ; … }`.
pub fn binary_from_exec_start(exec_start: &str) -> Option<PathBuf> {
    let path = exec_start.split("path=").nth(1)?.split(" ;").next()?.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Whether `binary` reads configuration overlays (`NAME.d/*.toml`).
///
/// Asked of the binary itself, not inferred from a version number: an
/// installed service older than overlays would take the profile written for
/// it, ignore it without a word, and keep running on the settings it had —
/// while this binary, which does read overlays, reported the profile current.
/// The probe is a scratch configuration whose overlay changes one value;
/// `check-config` either shows the change or it does not.
pub fn reads_overlays(binary: &Path) -> bool {
    let dir = std::env::temp_dir().join(format!("kompressor-setup-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let probe = dir.join("probe.toml");
    let ok = std::fs::create_dir_all(dir.join("probe.d")).is_ok()
        && std::fs::write(&probe, "[engine]\nmode = \"aggressive\"\n").is_ok()
        && std::fs::write(
            dir.join("probe.d/50.toml"),
            "[engine]\nmode = \"lossless\"\n",
        )
        .is_ok();
    let answer = ok
        && Command::new(binary)
            .arg("check-config")
            .arg("--config")
            .arg(&probe)
            .env_remove("KOMPRESSOR_CONFIG")
            .env_remove("KOMPRESSOR__ENGINE__MODE")
            .env_remove("KOMPRESSOR_MODE")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("mode = \"lossless\"")
            });
    let _ = std::fs::remove_dir_all(&dir);
    answer
}

/// The `--config` path out of systemd's rendering of `ExecStart`:
/// `{ path=/usr/bin/kompressor ; argv[]=/usr/bin/kompressor serve --config /etc/k.toml ; … }`.
pub fn config_from_exec_start(exec_start: &str) -> Option<PathBuf> {
    let argv = exec_start.split("argv[]=").nth(1)?.split(" ;").next()?;
    let mut words = argv.split_whitespace();
    while let Some(word) = words.next() {
        if word == "--config" {
            return words.next().map(PathBuf::from);
        }
        if let Some(path) = word.strip_prefix("--config=") {
            return Some(PathBuf::from(path));
        }
    }
    None
}

/// Run a command with the terminal attached, so `sudo` can ask for a password.
pub fn run_visible(command: &[String]) -> Result<(), CliError> {
    say!("  $ {}", command.join(" "));
    let (program, args) = command
        .split_first()
        .ok_or_else(|| CliError::Runtime("empty command".into()))?;
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| CliError::runtime(&format!("could not run {program}"), e))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliError::Runtime(format!(
            "`{}` failed ({status})",
            command.join(" ")
        )))
    }
}

/// `command`, prefixed with `sudo` when `privileged`.
pub fn elevated(privileged: bool, command: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if privileged {
        out.push("sudo".into());
    }
    out.extend(command.iter().map(|part| part.to_string()));
    out
}

/// Whether this process can create files in `dir`, or in the nearest ancestor
/// that exists when `dir` does not yet. Tried rather than read off the mode
/// bits, which do not know about ACLs, read-only mounts or root.
pub fn writable(dir: &Path) -> bool {
    let mut probe_dir = dir;
    while !probe_dir.exists() {
        match probe_dir.parent() {
            Some(parent) => probe_dir = parent,
            None => return false,
        }
    }
    let probe = probe_dir.join(format!(".kompressor-setup-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Whether `/healthz` answers 200 at `listen` (`host:port`).
///
/// Plain HTTP/1.0 over a socket: the listener is loopback and the answer is one
/// status line, which is not worth an async runtime.
pub fn healthy(listen: &str) -> bool {
    let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(listen) else {
        return false;
    };
    for addr in addrs {
        let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1))
        else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        if stream
            .write_all(format!("GET /healthz HTTP/1.0\r\nHost: {listen}\r\n\r\n").as_bytes())
            .is_err()
        {
            continue;
        }
        let mut head = [0u8; 16];
        let mut read = 0;
        while read < head.len() {
            match stream.read(&mut head[read..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => read += n,
            }
        }
        let line = String::from_utf8_lossy(&head[..read]);
        if line.starts_with("HTTP/1.") && line.get(9..12) == Some("200") {
            return true;
        }
    }
    false
}

/// Wait for `/healthz` at `listen`, up to `within`.
pub fn wait_healthy(listen: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if healthy(listen) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The home directory: `HOME`, or the profile directory on Windows.
pub fn home() -> PathBuf {
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    let found = if cfg!(windows) {
        var("USERPROFILE").or_else(|| var("HOME"))
    } else {
        var("HOME")
    };
    found
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

/// `$XDG_STATE_HOME`, else `~/.local/state`.
pub fn state_home() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local").join("state"))
}

/// The user unit's template, shared with `scripts/service/install.sh`.
///
/// Kept inside this crate rather than under `scripts/`: `include_str!` needs
/// the file wherever the crate is built, and a build from the crate alone —
/// the e2e image copies only `crates/`, `cargo install` packages only the
/// crate — has no `scripts/` beside it.
const UNIT_TEMPLATE: &str = include_str!("templates/kompressor.service.in");
/// The user configuration's template, shared with the same script.
const CONFIG_TEMPLATE: &str = include_str!("templates/config.toml.in");

/// The configuration a user install writes, for a state directory.
pub fn template_config(state: &Path) -> String {
    CONFIG_TEMPLATE.replace("@STATE@", &state.display().to_string())
}

/// The files a user-service install writes.
pub struct UserInstall {
    pub config: PathBuf,
    pub unit: PathBuf,
    pub state: PathBuf,
    /// Whether the configuration is new (an existing one is kept).
    pub new_config: bool,
}

impl UserInstall {
    /// Where a user install puts its files, as `scripts/service/install.sh` does.
    pub fn plan() -> UserInstall {
        let config = config_home().join("kompressor").join("config.toml");
        UserInstall {
            new_config: !config.exists(),
            config,
            unit: config_home().join("systemd").join("user").join(UNIT),
            state: state_home().join("kompressor"),
        }
    }

    /// Write the configuration (unless one exists) and the unit, then enable
    /// the unit. Starting it is left to the caller, after the overlay is in.
    pub fn write(&self) -> Result<(), CliError> {
        let binary = std::env::current_exe()
            .map_err(|e| CliError::runtime("could not find this executable", e))?;
        let state = self.state.display().to_string();
        for dir in [
            self.config.parent(),
            self.unit.parent(),
            Some(self.state.as_path()),
        ]
        .into_iter()
        .flatten()
        {
            std::fs::create_dir_all(dir).map_err(|e| {
                CliError::runtime(&format!("could not create {}", dir.display()), e)
            })?;
        }
        if self.new_config {
            crate::wrap::paths::write_private(
                &self.config,
                &CONFIG_TEMPLATE.replace("@STATE@", &state),
            )
            .map_err(|e| {
                CliError::runtime(&format!("could not write {}", self.config.display()), e)
            })?;
        }
        let unit = UNIT_TEMPLATE
            .replace("@BIN@", &binary.display().to_string())
            .replace("@CONFIG@", &self.config.display().to_string())
            .replace("@STATE@", &state);
        std::fs::write(&self.unit, unit).map_err(|e| {
            CliError::runtime(&format!("could not write {}", self.unit.display()), e)
        })?;
        run_visible(&["systemctl".into(), "--user".into(), "daemon-reload".into()])?;
        run_visible(&[
            "systemctl".into(),
            "--user".into(),
            "enable".into(),
            UNIT.into(),
        ])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_is_read_off_systemds_exec_start() {
        let shown = "{ path=/usr/bin/kompressor ; argv[]=/usr/bin/kompressor serve --proxy --api \
                     --config /etc/kompressor/config.toml ; ignore_errors=no ; start_time=[n/a] }";
        assert_eq!(
            config_from_exec_start(shown),
            Some(PathBuf::from("/etc/kompressor/config.toml"))
        );
        assert_eq!(
            binary_from_exec_start(shown),
            Some(PathBuf::from("/usr/bin/kompressor"))
        );
        let equals = "{ path=/k ; argv[]=/k serve --config=/x.toml ; ignore_errors=no }";
        assert_eq!(
            config_from_exec_start(equals),
            Some(PathBuf::from("/x.toml"))
        );
        assert_eq!(
            config_from_exec_start("{ path=/k ; argv[]=/k serve ; }"),
            None
        );
    }

    #[test]
    fn a_binary_that_does_not_show_the_overlay_does_not_read_overlays() {
        // The positive case runs against the built CLI in tests/cli.rs.
        assert!(!reads_overlays(Path::new("/bin/true")));
        assert!(!reads_overlays(Path::new("/nonexistent/kompressor")));
    }

    #[test]
    fn a_closed_port_is_not_healthy() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        assert!(!healthy(&addr));
    }

    #[test]
    fn a_listener_answering_200_is_healthy() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .unwrap();
        });
        assert!(healthy(&addr));
        server.join().unwrap();
    }
}

//! `trade-bot service install | remove | status`: run one config as a
//! service the operating system starts and restarts.
//!
//! The service files are the templates in `deploy/service/` (embedded, so
//! a release binary carries them), filled in for one config: a systemd
//! user unit on Linux, a launchd agent on macOS. Both run as the user, not
//! root (the user's keys, repo and data). Restarted after a failure, not
//! after a clean exit; exit code 3 (the kill switch tripped, see
//! [`crate::NEEDS_A_PERSON`]) is not restarted by systemd, and is retried
//! and refused once a minute by launchd, which can't tell exit codes apart.
//!
//! Keys stay out of the service file (invariant 6): `--env-file` names a
//! file only the user can read, loaded into the environment at start; the
//! bot still reads its keys from the environment alone. Real money stays
//! behind the three locks (invariant 7): a config with `mode = "live"` or
//! `network = "live"` is installed only with `--live --i-understand` typed
//! on the install command, and the service then runs with them; anything
//! else runs `--paper`. Windows: not supported (use the container).

use anyhow::{Context, bail};
use std::path::{Path, PathBuf};

const SYSTEMD: &str = include_str!("../../../deploy/service/trade-bot.service");
const LAUNCHD: &str = include_str!("../../../deploy/service/trade-bot.plist");

#[derive(clap::Subcommand)]
pub(crate) enum ServiceCommand {
    /// Write the service file for a config and start it.
    Install(InstallArgs),
    /// Stop the service and delete its file.
    Remove(NameArgs),
    /// Show whether the service is loaded and running.
    Status(NameArgs),
}

#[derive(clap::Args)]
pub(crate) struct InstallArgs {
    /// The run's config file.
    #[arg(long)]
    config: PathBuf,
    /// File with the keys (`ETORO_PRIVATE_KEY=...` lines), readable only by
    /// you (chmod 600). Never put keys in the config or the service file.
    #[arg(long)]
    env_file: PathBuf,
    /// Service name; default `trade-bot-<config file stem>`.
    #[arg(long)]
    name: Option<String>,
    /// The binary to run; default this one. Use an installed or release
    /// binary: `cargo clean` deletes the ones under `target/`.
    #[arg(long)]
    bin: Option<PathBuf>,
    /// Real money (the config must say `mode = "live"` and
    /// `network = "live"`); needs `--i-understand` too.
    #[arg(long)]
    live: bool,
    #[arg(long, requires = "live")]
    i_understand: bool,
    /// Print the service file instead of installing it.
    #[arg(long)]
    dry_run: bool,
}

#[derive(clap::Args)]
pub(crate) struct NameArgs {
    /// The service name given at install (default `trade-bot-<config
    /// stem>`).
    name: String,
}

/// What a service file is filled in with.
#[derive(Debug, PartialEq, Eq)]
struct Spec {
    name: String,
    workdir: PathBuf,
    env_file: PathBuf,
    /// The bot's command line: binary, `run`, flags.
    args: Vec<String>,
    log: PathBuf,
}

pub(crate) fn run(cmd: ServiceCommand) -> anyhow::Result<()> {
    if !cfg!(unix) {
        bail!(
            "services are supported on Linux (systemd) and macOS (launchd); on Windows run the container"
        );
    }
    match cmd {
        ServiceCommand::Install(a) => install(a),
        ServiceCommand::Remove(a) => remove(&a.name),
        ServiceCommand::Status(a) => status(&a.name),
    }
}

/// `trade-bot-<stem>`, reduced to letters, digits, `-` and `_`.
fn default_name(config: &Path) -> String {
    let stem = config
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("run")
        .trim_end_matches(".local");
    let clean: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("trade-bot-{clean}")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Whether `cfg` would trade real money (either lock set in the config).
fn real_money(cfg: &crate::config::Config) -> bool {
    cfg.mode == crate::config::Mode::Live
        || cfg.exchange.network == exchange::binance::Network::Live
}

/// The run flags: `--paper`, or `--live --i-understand` when the config is
/// live (`real`) and the installer typed both; refused otherwise.
fn run_flags(real: bool, live: bool, i_understand: bool) -> anyhow::Result<Vec<String>> {
    match (real, live, i_understand) {
        (false, false, _) => Ok(vec!["--paper".into()]),
        (true, true, true) => Ok(vec!["--live".into(), "--i-understand".into()]),
        (true, _, _) => bail!(
            "this config trades real money (mode or network \"live\"): install it only with \
             --live --i-understand, typed here; the service will then trade live at every start"
        ),
        (false, true, _) => {
            bail!("--live with a paper config: drop --live, or make the config live")
        }
    }
}

/// Refuses an env file others can read: it holds keys.
#[cfg(unix)]
pub(crate) fn check_env_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).with_context(|| {
        format!(
            "env file {}: create it with your keys, chmod 600",
            path.display()
        )
    })?;
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        bail!(
            "env file {} is readable by others (mode {:o}); it holds keys: chmod 600 {}",
            path.display(),
            mode & 0o777,
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn check_env_file(_: &Path) -> anyhow::Result<()> {
    bail!("services are not supported on this platform")
}

fn install(a: InstallArgs) -> anyhow::Result<()> {
    let config = std::fs::canonicalize(&a.config)
        .with_context(|| format!("config {}", a.config.display()))?;
    // Loads and validates it, as a run would at start.
    let cfg = crate::config::Config::load(Some(&config))?;
    let flags = run_flags(real_money(&cfg), a.live, a.i_understand)?;
    let env_file = std::fs::canonicalize(&a.env_file).with_context(|| {
        format!(
            "env file {}: create it with your keys, chmod 600",
            a.env_file.display()
        )
    })?;
    check_env_file(&env_file)?;
    let name = a.name.unwrap_or_else(|| default_name(&config));
    if !valid_name(&name) {
        bail!("service name {name:?}: letters, digits, '-', '_' and '.' only (at most 64)");
    }
    let bin = match a.bin {
        Some(b) => std::fs::canonicalize(&b).with_context(|| format!("binary {}", b.display()))?,
        None => std::env::current_exe().context("finding this binary")?,
    };
    if bin.components().any(|c| c.as_os_str() == "target") {
        eprintln!(
            "warning: {} is a build output: `cargo clean` deletes it and the service stops \
             starting; prefer a release binary (--bin)",
            bin.display()
        );
    }
    let workdir = std::env::current_dir().context("working directory")?;
    let mut args = vec![bin.display().to_string(), "run".into()];
    args.extend(flags.iter().cloned());
    args.extend(["--config".into(), config.display().to_string()]);
    let spec = Spec {
        log: workdir.join("data/logs").join(format!("{name}.log")),
        name,
        workdir,
        env_file,
        args,
    };
    let (text, path) = if cfg!(target_os = "macos") {
        (
            render_launchd(&spec),
            home()?
                .join("Library/LaunchAgents")
                .join(format!("{}.plist", spec.name)),
        )
    } else {
        (
            render_systemd(&spec),
            home()?
                .join(".config/systemd/user")
                .join(format!("{}.service", spec.name)),
        )
    };
    if a.dry_run {
        println!("# would write {}\n{text}", path.display());
        return Ok(());
    }
    if flags.iter().any(|f| f == "--live") {
        eprintln!("LIVE: this service trades real money at every start, without asking again");
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::create_dir_all(spec.workdir.join("data/logs"))?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    if cfg!(target_os = "macos") {
        let domain = format!("gui/{}", uid()?);
        // Replacing an older copy: unload it first (absent = fine).
        let _ = sh(
            "launchctl",
            &["bootout", &format!("{domain}/{}", spec.name)],
        );
        sh(
            "launchctl",
            &["bootstrap", &domain, &path.display().to_string()],
        )?;
        println!(
            "installed {} ({}): starts at login and after a failure; log {}",
            spec.name,
            path.display(),
            spec.log.display()
        );
    } else {
        sh("systemctl", &["--user", "daemon-reload"])?;
        sh("systemctl", &["--user", "enable", "--now", &spec.name])?;
        println!(
            "installed {} ({}): log with `journalctl --user -u {}`. To start at boot rather than \
             at login: loginctl enable-linger $USER",
            spec.name,
            path.display(),
            spec.name
        );
    }
    Ok(())
}

fn remove(name: &str) -> anyhow::Result<()> {
    if !valid_name(name) {
        bail!("not a service name: {name:?}");
    }
    if cfg!(target_os = "macos") {
        let path = home()?
            .join("Library/LaunchAgents")
            .join(format!("{name}.plist"));
        // bootout sends SIGTERM: the bot closes out before it exits.
        let _ = sh("launchctl", &["bootout", &format!("gui/{}/{name}", uid()?)]);
        remove_file(&path)?;
    } else {
        let path = home()?
            .join(".config/systemd/user")
            .join(format!("{name}.service"));
        let _ = sh("systemctl", &["--user", "disable", "--now", name]);
        remove_file(&path)?;
        sh("systemctl", &["--user", "daemon-reload"])?;
    }
    println!("removed {name}: stopped (open orders cancelled on the way out; positions stay)");
    Ok(())
}

fn status(name: &str) -> anyhow::Result<()> {
    if !valid_name(name) {
        bail!("not a service name: {name:?}");
    }
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("launchctl")
            .args(["print", &format!("gui/{}/{name}", uid()?)])
            .output()?;
        if !out.status.success() {
            println!("{name}: not loaded");
            return Ok(());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines().map(str::trim) {
            if ["state =", "pid =", "last exit code ="]
                .iter()
                .any(|k| line.starts_with(k))
            {
                println!("{name}: {line}");
            }
        }
    } else {
        std::process::Command::new("systemctl")
            .args(["--user", "status", name, "--no-pager"])
            .status()?;
    }
    Ok(())
}

fn remove_file(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("no service file at {}", path.display())
        }
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn uid() -> anyhow::Result<String> {
    let out = std::process::Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Runs `cmd args`; an error carries its stderr.
fn sh(cmd: &str, args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        bail!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// systemd quoting: each argument in double quotes, `"`, `\` and `%`
/// escaped (`%` is a specifier, `%%` a literal one).
fn systemd_arg(s: &str) -> String {
    let inner = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{inner}\"")
}

fn render_systemd(s: &Spec) -> String {
    let exec: Vec<String> = s.args.iter().map(|a| systemd_arg(a)).collect();
    SYSTEMD
        .replace("{{NAME}}", &s.name)
        .replace(
            "{{WORKDIR}}",
            &systemd_arg(&s.workdir.display().to_string()),
        )
        .replace(
            "{{ENV_FILE}}",
            &systemd_arg(&s.env_file.display().to_string()),
        )
        .replace("{{EXEC}}", &exec.join(" "))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn render_launchd(s: &Spec) -> String {
    let args: Vec<String> = s
        .args
        .iter()
        .map(|a| format!("    <string>{}</string>", xml(a)))
        .collect();
    LAUNCHD
        .replace("{{NAME}}", &xml(&s.name))
        .replace("{{WORKDIR}}", &xml(&s.workdir.display().to_string()))
        .replace("{{ENV_FILE}}", &xml(&s.env_file.display().to_string()))
        .replace("{{LOG}}", &xml(&s.log.display().to_string()))
        .replace("{{ARGS}}", &args.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            name: "trade-bot-etoro-demo".into(),
            workdir: "/home/me/trade bot".into(),
            env_file: "/home/me/.trade-bot.env".into(),
            args: vec![
                "/usr/local/bin/trade-bot".into(),
                "run".into(),
                "--paper".into(),
                "--config".into(),
                "/home/me/trade bot/config/50%.toml".into(),
            ],
            log: "/home/me/trade bot/data/logs/trade-bot-etoro-demo.log".into(),
        }
    }

    /// None of the placeholders the command fills is left.
    fn no_placeholders(text: &str) -> bool {
        [
            "{{NAME}}",
            "{{WORKDIR}}",
            "{{ENV_FILE}}",
            "{{EXEC}}",
            "{{ARGS}}",
            "{{LOG}}",
        ]
        .iter()
        .all(|p| !text.contains(p))
    }

    #[test]
    fn names_come_from_the_config_file() {
        assert_eq!(
            default_name(Path::new("/x/config/etoro-demo.local.toml")),
            "trade-bot-etoro-demo"
        );
        assert_eq!(default_name(Path::new("a b.toml")), "trade-bot-a-b");
        assert!(valid_name("trade-bot-x_1.2"));
        for bad in ["", "a/b", "../x", "a b", &"x".repeat(65)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    /// Every placeholder is filled; paths with spaces are quoted and `%`
    /// doubled (a systemd specifier otherwise); the restart rules are in.
    #[test]
    fn the_systemd_unit_is_filled_in_and_quoted() {
        let u = render_systemd(&spec());
        assert!(no_placeholders(&u), "{u}");
        assert!(u.contains("WorkingDirectory=\"/home/me/trade bot\""));
        assert!(u.contains("EnvironmentFile=\"/home/me/.trade-bot.env\""));
        assert!(u.contains(
            "ExecStart=\"/usr/local/bin/trade-bot\" \"run\" \"--paper\" \"--config\" \
             \"/home/me/trade bot/config/50%%.toml\""
        ));
        assert!(u.contains("Restart=on-failure") && u.contains("RestartPreventExitStatus=3"));
        assert!(!u.contains("\nEnvironment="), "keys only via the env file");
    }

    /// One <string> per argument after the env file; XML escaped.
    #[test]
    fn the_launchd_agent_is_filled_in_and_escaped() {
        let mut s = spec();
        s.args[4] = "/a&b/<c>.toml".into();
        let p = render_launchd(&s);
        assert!(no_placeholders(&p), "{p}");
        assert!(p.contains("<string>trade-bot-etoro-demo</string>"));
        assert!(p.contains(
            "<string>/home/me/.trade-bot.env</string>\n    <string>/usr/local/bin/trade-bot</string>\n    <string>run</string>"
        ));
        assert!(p.contains("<string>/a&amp;b/&lt;c&gt;.toml</string>"));
        assert!(p.contains("<key>SuccessfulExit</key>"));
    }

    #[cfg(unix)]
    #[test]
    fn an_env_file_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("tb-env-{}", std::process::id()));
        std::fs::write(&path, "ETORO_PRIVATE_KEY=x\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = check_env_file(&path).unwrap_err().to_string();
        assert!(err.contains("chmod 600"), "{err}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_env_file(&path).is_ok());
        std::fs::remove_file(&path).unwrap();
    }

    /// Paper config → `--paper`. A live config needs both flags typed at
    /// install; `--live` on a paper config is refused too.
    #[test]
    fn real_money_needs_both_flags_at_install() {
        assert_eq!(run_flags(false, false, false).unwrap(), vec!["--paper"]);
        assert_eq!(
            run_flags(true, true, true).unwrap(),
            vec!["--live", "--i-understand"]
        );
        assert!(run_flags(true, false, false).is_err());
        assert!(run_flags(true, true, false).is_err());
        assert!(run_flags(false, true, true).is_err());
    }
}

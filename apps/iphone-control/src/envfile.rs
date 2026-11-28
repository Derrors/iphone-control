//! The settings file `~/.config/iphone-control/env`: `KEY=value` lines, the format systemd's `EnvironmentFile=`,
//! a POSIX shell (`set -a; . file`) and this program all read. The service (launchd, systemd, `brew services`) loads
//! it for the daemon; every other command loads it at start-up ([`load_into_process`]), so `login`, `notify-test` and
//! the first-run guide see the same settings.
//!
//! A named instance (`--instance`, docs/dev/multi-instance-plan.md 3.1) adds its own `instances/<name>.env` over it:
//! [`ictl_daemon::instance::resolve`] works out what the process environment gets.
//!
//! The first-run guide writes it with [`EnvFile::set`]: a changed key is replaced in place, a new one appended, comments
//! and other keys are kept. A new file starts from `packaging/env.example`, so it documents itself. Mode 0600: it holds
//! notification secrets.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use ictl_daemon::envfmt::{last_values, line_key, parse};
use ictl_daemon::instance;

const TEMPLATE: &str = include_str!("../../../packaging/env.example");

pub fn config_dir() -> PathBuf {
    instance::config_dir()
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn path() -> PathBuf {
    config_dir().join("env")
}

pub struct EnvFile {
    path: PathBuf,
    text: String,
}

impl EnvFile {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => TEMPLATE.to_owned(),
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        Ok(Self { path: path.to_owned(), text })
    }

    /// The value of `key` (last assignment wins, as when the shell or systemd reads it); empty counts as unset
    pub fn get(&self, key: &str) -> Option<String> {
        parse(&self.text).into_iter().rev().find(|(k, _)| k == key).map(|(_, v)| v).filter(|v| !v.is_empty())
    }

    /// Sets `key` (or clears it with `None`/empty). Returns whether anything changed
    pub fn set(&mut self, key: &str, value: Option<&str>) -> anyhow::Result<bool> {
        let value = value.map(str::trim).unwrap_or_default();
        if self.get(key).as_deref().unwrap_or_default() == value {
            return Ok(false);
        }
        let line = format!("{key}={}", quote(value)?);
        let mut out = Vec::new();
        let mut done = false;
        for l in self.text.lines() {
            if !done && line_key(l) == Some(key) {
                out.push(line.clone());
                done = true;
            } else if line_key(l) == Some(key) {
                // a later duplicate would win over the one just set
                continue;
            } else {
                out.push(l.to_owned());
            }
        }
        if !done {
            out.push(line);
        }
        self.text = out.join("\n") + "\n";
        Ok(true)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // a name of its own: two commands saving at once must not write into the same temporary file
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = self.path.with_extension(format!("{}.{n}.tmp", std::process::id()));
        let written = (|| {
            let mut f = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&tmp)?;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            f.write_all(self.text.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(written?)
    }
}

/// Bare when that is safe for both the shell and systemd, single-quoted otherwise. A value with a single quote, a
/// backslash or a newline would mean different things to the two readers, so it is refused
fn quote(v: &str) -> anyhow::Result<String> {
    let Some(q) = ictl_daemon::envfmt::quote(v) else {
        bail!(
            "{}",
            ictl_l10n::tr!(
                "这个值里不能有单引号、反斜杠或换行",
                "The value can't contain a single quote, a backslash or a newline"
            )
        );
    };
    Ok(q)
}

/// Works out the instance this command is for and puts its settings into this process's environment
/// ([`instance::resolve`]): what is already set there wins (a variable on the command line), the managed keys of a
/// named instance excepted. Returns the named instance. Called once at start-up, before any thread is started
pub fn load_into_process(inv: &instance::Invocation) -> anyhow::Result<Option<String>> {
    let shared = match std::fs::read_to_string(path()) {
        Ok(text) => last_values(parse(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        // as before: a settings file that can't be read is no settings, but say so
        Err(e) => {
            eprintln!("{}: {e}", path().display());
            Vec::new()
        }
    };
    let r = instance::resolve(inv, &shared, &|k| std::env::var(k).ok(), &instance::registry_dir)
        .map_err(anyhow::Error::msg)?;
    for k in &r.unset {
        // SAFETY: single-threaded: main() calls this before building the async runtime
        unsafe { std::env::remove_var(k) };
    }
    for (k, v) in &r.set {
        // SAFETY: as above
        unsafe { std::env::set_var(k, v) };
    }
    Ok(r.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key set twice takes its last value, as the shell and systemd read the file; a last empty one unsets it
    #[test]
    fn the_last_value_of_a_key_wins() {
        let v = last_values(parse("A=1\nB=x\nA=2\nexport B=\nC='3'\n"));
        assert_eq!(v, vec![("A".into(), "2".into()), ("B".into(), String::new()), ("C".into(), "3".into())]);
    }

    #[test]
    fn set_replaces_in_place_and_keeps_comments() {
        let dir = std::env::temp_dir().join(format!("ictl-env-{}", std::process::id()));
        let p = dir.join("env");
        let mut f = EnvFile { path: p.clone(), text: "# proxy\nHTTP_PROXY=\nBARK_KEY=old\nBARK_KEY=older\n".into() };
        assert_eq!(f.get(ictl_daemon::protocol::env::BARK_KEY).as_deref(), Some("older"), "the last one wins");
        assert!(f.set(ictl_daemon::protocol::env::BARK_KEY, Some("new")).unwrap());
        assert!(!f.set(ictl_daemon::protocol::env::BARK_KEY, Some("new")).unwrap(), "unchanged");
        assert!(f.set("HTTP_PROXY", Some("http://127.0.0.1:7890")).unwrap());
        assert!(f.set(ictl_daemon::protocol::env::TG_CHAT_ID, Some("-100 1")).unwrap());
        assert_eq!(f.text, "# proxy\nHTTP_PROXY=http://127.0.0.1:7890\nBARK_KEY=new\nTG_CHAT_ID='-100 1'\n");
        assert_eq!(f.get(ictl_daemon::protocol::env::TG_CHAT_ID).as_deref(), Some("-100 1"));
        assert!(f.set("HTTP_PROXY", None).unwrap());
        assert_eq!(f.get("HTTP_PROXY"), None);
        assert!(f.set("X", Some("it's")).is_err());
        f.save().unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_file_starts_from_the_example() {
        let f = EnvFile::open(Path::new("/nonexistent/ictl/env")).unwrap();
        assert!(f.text.contains("IPHONE_CONTROL_LANG="));
        assert_eq!(f.get(ictl_daemon::protocol::env::LANG), None);
        assert_eq!(f.get(ictl_daemon::protocol::env::BARK_SERVER).as_deref(), Some("https://api.day.app"));
    }

    #[test]
    fn envfile_last_values_surfaces_only_non_empty_removed_vars() {
        let first = ictl_daemon::protocol::env::REMOVED[0].name;
        let second = ictl_daemon::protocol::env::REMOVED[1].name;
        let text = format!("{first}=0\n{second}=1\n{second}=\n");
        let map: std::collections::HashMap<String, String> = last_values(parse(&text)).into_iter().collect();
        let hit = ictl_daemon::protocol::env::active_removed_with(|k| map.get(k).cloned());
        let names: Vec<&str> = hit.iter().map(|v| v.name).collect();
        assert_eq!(names, vec![first]);
    }
}

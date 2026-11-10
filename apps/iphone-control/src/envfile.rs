//! The settings file `~/.config/iphone-control/env`: `KEY=value` lines, the format systemd's `EnvironmentFile=`,
//! a POSIX shell (`set -a; . file`) and this program all read. The service (launchd, systemd, `brew services`) loads
//! it for the daemon; every other command loads it at start-up ([`load_into_process`]), so `login`, `notify-test` and
//! the first-run guide see the same settings.
//!
//! The first-run guide writes it with [`EnvFile::set`]: a changed key is replaced in place, a new one appended, comments
//! and other keys are kept. A new file starts from `packaging/env.example`, so it documents itself. Mode 0600: it holds
//! notification secrets.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

const TEMPLATE: &str = include_str!("../../../packaging/env.example");

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("iphone-control")
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
        let tmp = self.path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(self.text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// The key a line assigns, if it is an assignment (`KEY=…`, `export KEY=…`)
fn line_key(l: &str) -> Option<&str> {
    let l = l.trim_start();
    let l = l.strip_prefix("export ").unwrap_or(l);
    let (k, _) = l.split_once('=')?;
    (!k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')).then_some(k)
}

pub fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let k = line_key(l)?;
            let v = l.trim_start().strip_prefix("export ").unwrap_or(l.trim_start())[k.len() + 1..].trim();
            Some((k.to_owned(), unquote(v)))
        })
        .collect()
}

fn unquote(v: &str) -> String {
    for q in ['\'', '"'] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return v[1..v.len() - 1].to_owned();
        }
    }
    v.to_owned()
}

/// Bare when that is safe for both the shell and systemd, single-quoted otherwise. A value with a single quote, a
/// backslash or a newline would mean different things to the two readers, so it is refused
fn quote(v: &str) -> anyhow::Result<String> {
    if v.bytes().all(|b| b.is_ascii_alphanumeric() || b"_./:@%+,=-".contains(&b)) {
        return Ok(v.to_owned());
    }
    if v.contains(['\'', '\\', '\n', '\r']) {
        bail!(
            "{}",
            ictl_l10n::tr!(
                "这个值里不能有单引号、反斜杠或换行",
                "The value can't contain a single quote, a backslash or a newline"
            )
        );
    }
    Ok(format!("'{v}'"))
}

/// Puts the file's settings into this process's environment, without overriding what is already set (a variable on
/// the command line wins). Called once at start-up, before any thread is started
pub fn load_into_process() {
    let Ok(text) = std::fs::read_to_string(path()) else { return };
    for (k, v) in parse(&text) {
        if v.is_empty() || std::env::var_os(&k).is_some() {
            continue;
        }
        // SAFETY: single-threaded: main() calls this before building the async runtime
        unsafe { std::env::set_var(&k, &v) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_replaces_in_place_and_keeps_comments() {
        let dir = std::env::temp_dir().join(format!("ictl-env-{}", std::process::id()));
        let p = dir.join("env");
        let mut f = EnvFile { path: p.clone(), text: "# proxy\nHTTP_PROXY=\nBARK_KEY=old\nBARK_KEY=older\n".into() };
        assert_eq!(f.get("BARK_KEY").as_deref(), Some("older"), "the last one wins");
        assert!(f.set("BARK_KEY", Some("new")).unwrap());
        assert!(!f.set("BARK_KEY", Some("new")).unwrap(), "unchanged");
        assert!(f.set("HTTP_PROXY", Some("http://127.0.0.1:7890")).unwrap());
        assert!(f.set("TG_CHAT_ID", Some("-100 1")).unwrap());
        assert_eq!(f.text, "# proxy\nHTTP_PROXY=http://127.0.0.1:7890\nBARK_KEY=new\nTG_CHAT_ID='-100 1'\n");
        assert_eq!(f.get("TG_CHAT_ID").as_deref(), Some("-100 1"));
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
        assert_eq!(f.get("IPHONE_CONTROL_LANG"), None);
        assert_eq!(f.get("BARK_SERVER").as_deref(), Some("https://api.day.app"));
    }
}

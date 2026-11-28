//! Instances: one daemon per device on one host (docs/dev/multi-instance-plan.md 3).
//!
//! The default instance is what there always was: the settings file `~/.config/iphone-control/env`, the data
//! directory, the socket and the services of before, unchanged. A named instance is registered in the registry
//! directory (`~/.config/iphone-control/instances/`) as `<name>.env`: the keys it manages ([`Managed`]: its socket,
//! data directory, device, control page port and id), written by `instance add`, plus settings of its own. The
//! registry is where every command, service and MCP server of a named instance finds the same absolute socket.
//!
//! `<name>.pending` is a reservation while `instance add` is under way ([`Pending`]): it already keeps the default
//! instance from choosing that device, but nothing starts from it.
//!
//! What instances share (the Apple ID account, passcodes, the records of auto-unlock disabled, the device leases, the
//! viewer's tokens) is in the data root: the default instance's data directory. The anchor
//! (`registry-anchor.json` in the host directory of the maintenance lock, which doesn't depend on `XDG_*`) records
//! the data root and the registry directory the first instance was added with, so a daemon or a command started with
//! other `XDG_*` settings still finds the same registry, or refuses ([`check_anchor`]).

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::protocol::env;

/// The instance without a name
pub const DEFAULT: &str = "default";
/// The default instance's id (a named instance gets a random one when it is added)
pub const DEFAULT_ID: &str = "default";
const ANCHOR_FILE: &str = "registry-anchor.json";

/// The keys of `<name>.env` that make the instance what it is: written by `instance add`, never taken from elsewhere
/// (a different value from the command line, the environment or the shared settings file is an error)
pub const MANAGED: [&str; 5] = [env::SOCKET, env::DATA_DIR, env::UDID, env::VIEWER_PORT, env::INSTANCE_ID];

/// Whether `name` can name an instance: `[a-z0-9][a-z0-9-]{0,31}`, not `default`
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && name != DEFAULT
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// `~/.config/iphone-control` (`XDG_CONFIG_HOME` if set)
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into())).join(".config"))
        .join("iphone-control")
}

/// Where the registry and the data root are, once an instance was added
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub registry: PathBuf,
    pub root: PathBuf,
}

fn anchor_path() -> io::Result<PathBuf> {
    Ok(crate::maintenance::host_dir()?.join(ANCHOR_FILE))
}

/// The anchor, if an instance was ever added. One that can't be read is an error (taken for none, the registry
/// could be missed)
pub fn anchor() -> io::Result<Option<Anchor>> {
    match std::fs::read(anchor_path()?) {
        Ok(b) => serde_json::from_slice(&b).map(Some).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Writes the anchor (`instance add`, the first time)
pub fn write_anchor(a: &Anchor) -> io::Result<()> {
    let p = anchor_path()?;
    let dir = p.parent().unwrap_or(Path::new("."));
    ictl_unlock::durable::write_atomic(
        dir,
        ANCHOR_FILE,
        &serde_json::to_vec_pretty(a).map_err(io::Error::other)?,
        0o600,
    )
}

/// `name` in the registry directory `dir` replaced whole, private, on disk (`<n>.env`, `<n>.pending`)
pub fn write_file(dir: &Path, name: &str, data: &[u8]) -> io::Result<()> {
    ictl_unlock::durable::write_atomic(dir, name, data, 0o600)
}

/// The registry directory: the anchor's, or the one next to the settings file. An anchor that can't be read is an
/// error: the registry it names could be another one
pub fn registry_dir() -> io::Result<PathBuf> {
    Ok(match anchor()? {
        Some(a) => a.registry,
        None => config_dir().join("instances"),
    })
}

/// With an anchor, the registry and data root this process works out for itself must be its: a process whose
/// `XDG_CONFIG_HOME` or `XDG_DATA_HOME` differs would miss the registry and the shared records. `root`: the data
/// directory of the default instance as this process sees it
pub fn check_anchor(root: &Path) -> Result<Option<Anchor>, String> {
    let a = anchor().map_err(|e| e.to_string())?;
    if let Some(a) = &a {
        let mine = config_dir().join("instances");
        if !same_dir(&a.registry, &mine) {
            return Err(ictl_l10n::trf!(
                "实例登记在 {}，这个进程的设置目录却是 {}（XDG_CONFIG_HOME 不同）：有命名实例时所有进程要用同一个设置目录",
                "The instances are registered in {}, but this process's settings directory is {} (another XDG_CONFIG_HOME): with named instances every process must use the same settings directory",
                a.registry.display(),
                mine.display()
            ));
        }
        if !same_dir(&a.root, root) {
            return Err(ictl_l10n::trf!(
                "实例共用的数据目录是 {}，这个进程的却是 {}（XDG_DATA_HOME 或 ICTL_DATA_DIR 不同）：有命名实例时不支持别的数据目录",
                "The instances share the data directory {}, but this process's is {} (another XDG_DATA_HOME or ICTL_DATA_DIR): with named instances no other data directory is supported",
                a.root.display(),
                root.display()
            ));
        }
    }
    Ok(a)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// A named instance's managed keys ([`MANAGED`])
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Managed {
    pub socket: PathBuf,
    pub data_dir: PathBuf,
    pub udid: String,
    pub port: u16,
    pub id: String,
}

/// A registered named instance
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub name: String,
    pub managed: Managed,
    /// Its own settings (keys other than the managed ones), in order; an empty value clears the shared one
    pub settings: Vec<(String, String)>,
}

/// A reservation made while `instance add` is under way
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub name: String,
    pub udid: String,
    pub port: u16,
    /// Unix seconds
    pub started: u64,
}

/// What the registry holds
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    pub registered: Vec<Registered>,
    pub pending: Vec<Pending>,
}

impl Scan {
    /// Whether there is any named instance, registered or reserved
    pub fn any(&self) -> bool {
        !self.registered.is_empty() || !self.pending.is_empty()
    }

    /// The named instance that has `udid`, or is getting it
    pub fn reserved_by(&self, udid: &str) -> Option<&str> {
        let r = self.registered.iter().find(|r| r.managed.udid.eq_ignore_ascii_case(udid)).map(|r| r.name.as_str());
        r.or_else(|| self.pending.iter().find(|p| p.udid.eq_ignore_ascii_case(udid)).map(|p| p.name.as_str()))
    }

    /// The devices named instances have or are getting (upper case)
    pub fn reserved_udids(&self) -> Vec<String> {
        self.registered
            .iter()
            .map(|r| r.managed.udid.to_ascii_uppercase())
            .chain(self.pending.iter().map(|p| p.udid.to_ascii_uppercase()))
            .collect()
    }
}

/// Reads `<dir>/<name>.env`; `None` when there is none
pub fn read(dir: &Path, name: &str) -> io::Result<Option<Registered>> {
    let text = match std::fs::read_to_string(dir.join(format!("{name}.env"))) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    parse_registered(name, &text).map(Some)
}

fn parse_registered(name: &str, text: &str) -> io::Result<Registered> {
    let values = crate::envfmt::last_values(crate::envfmt::parse(text));
    let get = |k: &str| values.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    let missing = |k: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{name}.env has no {k}"));
    let managed = Managed {
        socket: get(env::SOCKET).map(PathBuf::from).ok_or_else(|| missing(env::SOCKET))?,
        data_dir: get(env::DATA_DIR).map(PathBuf::from).ok_or_else(|| missing(env::DATA_DIR))?,
        udid: get(env::UDID).ok_or_else(|| missing(env::UDID))?,
        port: get(env::VIEWER_PORT).and_then(|p| p.parse().ok()).ok_or_else(|| missing(env::VIEWER_PORT))?,
        id: get(env::INSTANCE_ID).ok_or_else(|| missing(env::INSTANCE_ID))?,
    };
    if !managed.socket.is_absolute() || !managed.data_dir.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{name}.env: paths must be absolute")));
    }
    let settings = values.into_iter().filter(|(k, _)| !MANAGED.contains(&k.as_str())).collect();
    Ok(Registered { name: name.to_owned(), managed, settings })
}

/// The text of `<name>.env` for `r`: a header saying not to edit it, the managed keys, then its own settings.
/// `None`: a value the format can't hold
pub fn render(r: &Registered) -> Option<String> {
    let m = &r.managed;
    let mut out = String::from(
        "# Written by iphone-control instance add: one named instance (docs/dev/multi-instance-plan.md).\n\
         # Don't edit the keys above the blank line; `instance remove` and `instance add` do. Settings of this\n\
         # instance's own may follow it (an empty value clears the one in ../env).\n",
    );
    for (k, v) in [
        (env::SOCKET, m.socket.to_string_lossy().into_owned()),
        (env::DATA_DIR, m.data_dir.to_string_lossy().into_owned()),
        (env::UDID, m.udid.clone()),
        (env::VIEWER_PORT, m.port.to_string()),
        (env::INSTANCE_ID, m.id.clone()),
    ] {
        out += &format!("{k}={}\n", crate::envfmt::quote(&v)?);
    }
    out += "\n";
    for (k, v) in &r.settings {
        out += &format!("{k}={}\n", crate::envfmt::quote(v)?);
    }
    Some(out)
}

/// Everything in the registry directory. An entry that can't be read is an error: a registry read in part would let
/// the default instance pick a device a named one has
pub fn scan(dir: &Path) -> io::Result<Scan> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Scan::default()),
        Err(e) => return Err(e),
    };
    let mut scan = Scan::default();
    for e in entries {
        let e = e?;
        let file = e.file_name();
        let Some(file) = file.to_str() else { continue };
        if let Some(name) = file.strip_suffix(".env").filter(|n| valid_name(n)) {
            if let Some(r) = read(dir, name)? {
                scan.registered.push(r);
            }
        } else if let Some(name) = file.strip_suffix(".pending").filter(|n| valid_name(n)) {
            let p: Pending = serde_json::from_slice(&std::fs::read(e.path())?)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            if p.name == name {
                scan.pending.push(p);
            }
        }
    }
    scan.registered.sort_by(|a, b| a.name.cmp(&b.name));
    scan.pending.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(scan)
}

/// What a command was started with, read before the settings are ([`resolve`])
#[derive(Debug, Default, Clone)]
pub struct Invocation {
    /// `--instance` on the command line
    pub instance: Option<String>,
    /// `--socket` on the command line
    pub socket: Option<PathBuf>,
    /// `--data-dir` on the command line
    pub data_dir: Option<PathBuf>,
    /// `mcp --local` (or the control page's `mcp --task-token-file`): connect to `--socket`, whatever the settings say
    pub local: bool,
}

/// The settings `mcp --local` never takes, from the files or the environment: they would send it elsewhere
pub const NOT_LOCAL: [&str; 3] = [env::URL, env::CA, env::INSTANCE];

/// What to change in the process environment before anything reads it ([`resolve`]): first [`Self::unset`], then
/// [`Self::set`]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The named instance, `None` for the default one
    pub name: Option<String>,
    /// Removed
    pub unset: Vec<String>,
    /// Set, in order
    pub set: Vec<(String, String)>,
}

/// Works out the settings of the instance a command is for (docs/dev/multi-instance-plan.md 3.1).
///
/// - The instance: `--instance`; without it, and without an explicit `--socket` (which wins) or `--local`,
///   `IPHONE_CONTROL_INSTANCE` from the environment, then from the settings file. `default` is the default instance.
/// - The default instance: the settings file's values where the environment has none, as always.
/// - A named one: the settings file, then the user keys of its `<name>.env` (an empty one clears the shared value),
///   where the environment has none; then its managed keys. A `--socket`, `--data-dir`, `ICTL_SOCKET`,
///   `ICTL_DATA_DIR` or `IPHONE_UDID` from the command line, the environment or the settings file that differs from
///   them is an error naming where it came from.
///
/// `shared`: the settings file's last values; `real`: the process environment (a variable set there, even empty,
/// wins over the files, as it always has); `registry`: the registry directory, looked up only for a named instance
pub fn resolve(
    inv: &Invocation,
    shared: &[(String, String)],
    real: &dyn Fn(&str) -> Option<String>,
    registry: &dyn Fn() -> io::Result<PathBuf>,
) -> Result<Resolved, String> {
    let skip: &[&str] = if inv.local { &NOT_LOCAL } else { &[] };
    let is_set = |k: &str| !skip.contains(&k) && real(k).is_some();
    let real = |k: &str| if skip.contains(&k) { None } else { real(k).filter(|v| !v.is_empty()) };
    let shared: Vec<(String, String)> = shared.iter().filter(|(k, _)| !skip.contains(&k.as_str())).cloned().collect();
    let shared_get = |k: &str| shared.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    let name = match &inv.instance {
        Some(n) => Some(n.clone()),
        None if inv.socket.is_some() => None,
        None => real(env::INSTANCE).or_else(|| shared_get(env::INSTANCE)),
    };
    let name = match name {
        Some(n) if n == DEFAULT => None,
        Some(n) if !valid_name(&n) => {
            return Err(ictl_l10n::trf!(
                "实例名 {:?} 不对：只能用小写字母、数字和 -，以字母或数字开头，最长 32 个字符",
                "The instance name {:?} isn't valid: lower-case letters, digits and -, starting with a letter or digit, 32 at most",
                n
            ));
        }
        n => n,
    };
    let mut out = Resolved { name: name.clone(), ..Default::default() };
    // which instance this is: what this resolution says, never a value left in the environment
    let identity = [env::INSTANCE, env::INSTANCE_ID];
    out.unset.extend(identity.iter().chain(skip.iter().filter(|k| !identity.contains(k))).map(|k| k.to_string()));
    let Some(name) = name else {
        out.set = shared
            .into_iter()
            .filter(|(k, v)| !v.is_empty() && !identity.contains(&k.as_str()) && !is_set(k))
            .collect();
        return Ok(out);
    };
    let registry = registry().map_err(|e| e.to_string())?;
    let registry = registry.as_path();
    let Some(reg) =
        read(registry, &name).map_err(|e| format!("{}: {e}", registry.join(format!("{name}.env")).display()))?
    else {
        let pending = registry.join(format!("{name}.pending")).exists();
        return Err(if pending {
            ictl_l10n::trf!(
                "实例 {} 还在添加中（只有 {}.pending）：等 instance add 做完，或者 instance add --resume {} 继续",
                "The instance {} is still being added (only {}.pending): let instance add finish, or continue it with instance add --resume {}",
                name,
                name,
                name
            )
        } else {
            ictl_l10n::trf!(
                "没有名叫 {} 的实例（登记目录 {}）；instance list 列出已有的",
                "There is no instance named {} (registry {}); instance list shows the ones there are",
                name,
                registry.display()
            )
        });
    };
    let m = &reg.managed;
    // the managed keys come from the registration only
    let managed: [(&str, String); 3] = [
        (env::SOCKET, m.socket.to_string_lossy().into_owned()),
        (env::DATA_DIR, m.data_dir.to_string_lossy().into_owned()),
        (env::UDID, m.udid.clone()),
    ];
    let same = |k: &str, a: &str, b: &str| {
        if k == env::UDID { a.eq_ignore_ascii_case(b) } else { Path::new(a) == Path::new(b) }
    };
    let conflict = |k: &str, from: String, got: &str, want: &str| {
        ictl_l10n::trf!(
            "实例 {} 的 {} 是 {}，{}却给了 {}：命名实例的这一项只取自它的登记，去掉别处的设置",
            "The instance {}'s {} is {}, but {} gives {}: a named instance takes it from its registration only, remove the other setting",
            name,
            k,
            want,
            from,
            got
        )
    };
    let cli = [(env::SOCKET, "--socket", &inv.socket), (env::DATA_DIR, "--data-dir", &inv.data_dir)];
    for (k, flag, v) in cli {
        let want = &managed.iter().find(|(n, _)| *n == k).expect("managed").1;
        if let Some(v) = v.as_ref().map(|p| p.to_string_lossy().into_owned()).filter(|v| !same(k, v, want)) {
            return Err(conflict(k, ictl_l10n::trf!("命令行的 {}", "the command line's {}", flag), &v, want));
        }
    }
    for (k, want) in &managed {
        if let Some(v) = real(k).filter(|v| !same(k, v, want)) {
            return Err(conflict(k, ictl_l10n::tr!("环境变量", "the environment").to_owned(), &v, want));
        }
        if let Some(v) = shared_get(k).filter(|v| !same(k, v, want)) {
            return Err(conflict(
                k,
                ictl_l10n::tr!("共用的设置文件 env", "the shared settings file env").to_owned(),
                &v,
                want,
            ));
        }
    }
    // the settings file, then the instance's own keys over it (empty: cleared)
    let mut files = shared;
    for (k, v) in &reg.settings {
        files.retain(|(n, _)| n != k);
        files.push((k.clone(), v.clone()));
    }
    out.set = files
        .into_iter()
        .filter(|(k, v)| {
            !v.is_empty() && !MANAGED.contains(&k.as_str()) && !identity.contains(&k.as_str()) && !is_set(k)
        })
        .collect();
    out.set.extend(managed.into_iter().map(|(k, v)| (k.to_owned(), v)));
    out.set.push((env::VIEWER_PORT.into(), m.port.to_string()));
    out.set.push((env::INSTANCE_ID.into(), m.id.clone()));
    out.set.push((env::INSTANCE.into(), name));
    Ok(out)
}

/// The paths of a named instance, worked out when it is added: its data directory under the data root, its socket
/// under `$XDG_RUNTIME_DIR/iphone-control/instances/<name>/` (or under its data directory without one)
pub fn paths_for(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let data = root.join("instances").join(name);
    let socket = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(run) if !crate::protocol::running_as_root() => {
            PathBuf::from(run).join("iphone-control/instances").join(name).join("ctl.sock")
        }
        _ if crate::protocol::running_as_root() => {
            PathBuf::from("/run/iphone-control/instances").join(name).join("ctl.sock")
        }
        _ => data.join("ctl.sock"),
    };
    (data, socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names() {
        for ok in ["ipad", "a", "phone-2", "0", &"a".repeat(32)] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "default", "-a", "iPad", "a_b", "a.b", "../x", &"a".repeat(33), "a b"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    /// The managed keys come out as they went in; other settings follow; missing managed keys or relative paths are
    /// refused
    #[test]
    fn a_registration_reads_back_what_was_written() {
        let r = Registered {
            name: "ipad".into(),
            managed: Managed {
                socket: "/run/user/1000/iphone-control/instances/ipad/ctl.sock".into(),
                data_dir: "/home/u/.local/share/iphone-control/instances/ipad".into(),
                udid: "00008020-000925043499002E".into(),
                port: 8203,
                id: "3f2a".into(),
            },
            settings: vec![(env::BARK_KEY.into(), "k".into()), (env::TG_CHAT_ID.into(), String::new())],
        };
        let text = render(&r).unwrap();
        assert_eq!(parse_registered("ipad", &text).unwrap(), r);
        let without = text.replace("IPHONE_UDID=00008020-000925043499002E\n", "");
        assert!(parse_registered("ipad", &without).is_err());
        let relative = text.replace("ICTL_DATA_DIR=/home/u", "ICTL_DATA_DIR=home/u");
        assert!(parse_registered("ipad", &relative).is_err());
    }

    /// A scan finds registrations and reservations; a reservation alone already counts, and an unreadable entry is
    /// an error rather than nothing
    #[test]
    fn a_scan_counts_reservations_and_refuses_what_it_cant_read() {
        let dir = std::env::temp_dir().join(format!("ictl-instances-{}", uuid::Uuid::new_v4()));
        assert!(!scan(&dir).unwrap().any(), "no directory: nothing");
        std::fs::create_dir_all(&dir).unwrap();
        let p = Pending { name: "ipad".into(), udid: "aa".into(), port: 8203, started: 1 };
        std::fs::write(dir.join("ipad.pending"), serde_json::to_vec(&p).unwrap()).unwrap();
        std::fs::write(dir.join("notes.txt"), b"ignored").unwrap();
        let s = scan(&dir).unwrap();
        assert!(s.any() && s.registered.is_empty());
        assert_eq!(s.reserved_udids(), ["AA"]);
        std::fs::write(dir.join("ipad.pending"), b"half").unwrap();
        assert!(scan(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn registry_with(name: &str, settings: &[(&str, &str)]) -> (PathBuf, Registered) {
        let dir = std::env::temp_dir().join(format!("ictl-resolve-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = Registered {
            name: name.into(),
            managed: Managed {
                socket: "/run/u/iphone-control/instances/ipad/ctl.sock".into(),
                data_dir: "/home/u/.local/share/iphone-control/instances/ipad".into(),
                udid: "00008020-000925043499002E".into(),
                port: 8203,
                id: "3f2a".into(),
            },
            settings: settings.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        };
        std::fs::write(dir.join(format!("{name}.env")), render(&r).unwrap()).unwrap();
        (dir, r)
    }

    fn run(inv: &Invocation, shared: &[(&str, &str)], real: &[(&str, &str)], dir: &Path) -> Result<Resolved, String> {
        let shared: Vec<_> = shared.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let real: Vec<(String, String)> = real.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let dir = dir.to_owned();
        resolve(inv, &shared, &|k| real.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()), &move || Ok(dir.clone()))
    }

    fn get<'a>(r: &'a Resolved, k: &str) -> Option<&'a str> {
        r.set.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    /// Without an instance, the settings file fills in what the environment doesn't have (set but empty counts as
    /// set), exactly as before; a stray instance id or name never stays behind
    #[test]
    fn the_default_instance_reads_its_settings_as_before() {
        let none = Path::new("/nonexistent");
        let r = run(
            &Invocation::default(),
            &[(env::BARK_KEY, "k"), (env::UDID, "AA"), ("HTTP_PROXY", "p"), (env::INSTANCE_ID, "x")],
            &[("HTTP_PROXY", "")],
            none,
        )
        .unwrap();
        assert_eq!(r.name, None);
        assert_eq!(r.set, [(env::BARK_KEY.to_string(), "k".to_string()), (env::UDID.into(), "AA".into())]);
        assert!(r.unset.contains(&env::INSTANCE.to_string()) && r.unset.contains(&env::INSTANCE_ID.to_string()));
        let named_default = Invocation { instance: Some("default".into()), ..Default::default() };
        assert_eq!(run(&named_default, &[], &[], none).unwrap().name, None, "`default` is the default instance");
    }

    /// A named instance: the shared file, its own keys over it (empty clears), the environment over both, then its
    /// managed keys; the instance comes from the command line, else the environment, else the shared file
    #[test]
    fn a_named_instance_layers_its_settings() {
        let (dir, r) = registry_with("ipad", &[(env::BARK_KEY, "ipad-key"), (env::TG_CHAT_ID, "")]);
        let shared =
            [(env::BARK_KEY, "shared"), (env::TG_CHAT_ID, "1"), ("HTTP_PROXY", "p"), (env::VIEWER_PORT, "8200")];
        let by_env =
            run(&Invocation::default(), &shared, &[(env::INSTANCE, "ipad"), ("HTTP_PROXY", "q")], &dir).unwrap();
        assert_eq!(by_env.name.as_deref(), Some("ipad"));
        assert_eq!(get(&by_env, env::BARK_KEY), Some("ipad-key"));
        assert_eq!(get(&by_env, env::TG_CHAT_ID), None, "cleared by the instance");
        assert_eq!(get(&by_env, "HTTP_PROXY"), None, "the environment's own value stays");
        assert_eq!(get(&by_env, env::VIEWER_PORT), Some("8203"));
        assert_eq!(get(&by_env, env::SOCKET), r.managed.socket.to_str());
        assert_eq!(get(&by_env, env::DATA_DIR), r.managed.data_dir.to_str());
        assert_eq!(get(&by_env, env::INSTANCE_ID), Some("3f2a"));
        assert_eq!(get(&by_env, env::INSTANCE), Some("ipad"));
        let by_file = run(&Invocation::default(), &[(env::INSTANCE, "ipad")], &[], &dir).unwrap();
        assert_eq!(by_file.name.as_deref(), Some("ipad"));
        assert!(run(&Invocation::default(), &[(env::INSTANCE, "phone")], &[], &dir).is_err(), "not registered");
        std::fs::write(dir.join("phone.pending"), b"{}").unwrap();
        let pending = run(&Invocation::default(), &[(env::INSTANCE, "phone")], &[], &dir).unwrap_err();
        assert!(pending.contains("phone.pending"), "{pending}");
        assert!(run(&Invocation { instance: Some("iPad".into()), ..Default::default() }, &[], &[], &dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A managed key from anywhere else must agree: the command line, the environment, the shared file. A UDID is
    /// the same in either case; an empty one in the shared file is cleared, not a conflict
    #[test]
    fn a_managed_key_from_elsewhere_must_agree() {
        let (dir, r) = registry_with("ipad", &[]);
        let ipad = Invocation { instance: Some("ipad".into()), ..Default::default() };
        let sock = r.managed.socket.to_str().unwrap();
        assert!(run(&ipad, &[(env::UDID, "00008130-AAAA")], &[], &dir).unwrap_err().contains(env::UDID));
        run(&ipad, &[(env::UDID, "")], &[], &dir).unwrap();
        run(&ipad, &[], &[(env::UDID, "00008020-000925043499002e")], &dir).unwrap();
        assert!(run(&ipad, &[], &[(env::SOCKET, "/tmp/other.sock")], &dir).unwrap_err().contains(env::SOCKET));
        run(&ipad, &[], &[(env::SOCKET, sock)], &dir).unwrap();
        let other = Invocation { socket: Some("/tmp/other.sock".into()), ..ipad.clone() };
        assert!(run(&other, &[], &[], &dir).unwrap_err().contains("--socket"));
        let same = Invocation { socket: Some(sock.into()), ..ipad.clone() };
        assert_eq!(run(&same, &[], &[], &dir).unwrap().name.as_deref(), Some("ipad"));
        let dd = Invocation { data_dir: Some("/elsewhere".into()), ..ipad.clone() };
        assert!(run(&dd, &[], &[], &dir).unwrap_err().contains("--data-dir"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An explicit `--socket` wins over an instance named in the environment or the settings file; `--local` ignores
    /// the settings that would send it elsewhere, wherever they come from
    #[test]
    fn an_explicit_socket_and_local_mcp_keep_to_their_socket() {
        let (dir, _) = registry_with("ipad", &[]);
        let sock = Invocation { socket: Some("/run/u/iphone-control/ctl.sock".into()), ..Default::default() };
        let r = run(&sock, &[(env::INSTANCE, "ipad")], &[(env::INSTANCE, "ipad")], &dir).unwrap();
        assert_eq!(r.name, None);
        assert!(r.unset.contains(&env::INSTANCE.to_string()));
        let local = Invocation { local: true, ..sock.clone() };
        let shared = [(env::URL, "https://h:8200"), (env::CA, "/ca.pem"), (env::BARK_KEY, "k")];
        let r = run(&local, &shared, &[(env::URL, "https://x:1"), (env::INSTANCE, "ipad")], &dir).unwrap();
        assert_eq!(r.name, None);
        assert_eq!(r.set, [(env::BARK_KEY.to_string(), "k".to_string())]);
        for k in NOT_LOCAL {
            assert!(r.unset.contains(&k.to_string()), "{k}");
        }
        let local_named = Invocation { instance: Some("ipad".into()), local: true, ..Default::default() };
        let r = run(&local_named, &shared, &[(env::URL, "https://x:1")], &dir).unwrap();
        assert_eq!(get(&r, env::URL), None);
        assert!(r.unset.contains(&env::URL.to_string()));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

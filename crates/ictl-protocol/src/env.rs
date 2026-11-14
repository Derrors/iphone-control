//! Every environment variable of this project, in one place: the code reads them through these names, and the
//! tests below keep the list, the code and the documentation in step (a name only in the code, or a setting the
//! configuration page doesn't mention, fails them).
//!
//! The names stay as they are, whatever their prefix: they are in people's settings files
//! (`~/.config/iphone-control/env`), service units and scripts. `IPHONE_CONTROL_*` is for new ones; the unprefixed
//! ones (`IPHONE_UDID`, `SCREEN_W`, `BARK_KEY`…) and `ICTL_*` come from earlier versions.
//! System and other programs' variables (`HOME`, `PATH`, `XDG_*`, `CODEX_HOME`…) aren't listed.

/// What a variable is for, which says where it is documented
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The user's setting: on the configuration page (website/guide/configuration.md, and the English one)
    Setting,
    /// Where notifications go: on the notifications page
    Notify,
    /// An experiment that is off by default: in docs/architecture.md
    Experiment,
    /// Set by this program for a command it starts (`task run`, the control page's agent), read by
    /// `iphone-control` inside it
    Given,
    /// Read when building (`option_env!`): who publishes releases and their signing keys
    /// (docs/dev/release-updates.md)
    Build,
    /// For diagnostics and tests only
    Dev,
}

/// One variable: its name, what it is for, and whether `packaging/env.example` (the settings file's template)
/// lists it
#[derive(Debug, Clone, Copy)]
pub struct Var {
    pub name: &'static str,
    pub kind: Kind,
    pub in_settings_file: bool,
}

// the device and the daemon
pub const UDID: &str = "IPHONE_UDID";
pub const SCREEN_W: &str = "SCREEN_W";
pub const SCREEN_H: &str = "SCREEN_H";
pub const RESIGN_CHECK_HOURS: &str = "RESIGN_CHECK_HOURS";
pub const RESIGN_MIN_DAYS: &str = "RESIGN_MIN_DAYS";
pub const OFFLINE_ALERT_MIN: &str = "OFFLINE_ALERT_MIN";
pub const DATA_DIR: &str = "ICTL_DATA_DIR";
pub const SOCKET: &str = "ICTL_SOCKET";
pub const LOG: &str = "ICTL_LOG";
pub const ANISETTE_URL: &str = "ICTL_ANISETTE_URL";
pub const DDI_URL: &str = "ICTL_DDI_URL";
pub const DDI_DIR: &str = "ICTL_DDI_DIR";
pub const FFMPEG: &str = "ICTL_FFMPEG";
pub const WDA_UNSIGNED: &str = "ICTL_WDA_UNSIGNED";
pub const WDA_DETACHED: &str = "ICTL_WDA_DETACHED";
/// Read by `ictl_l10n` (below this crate), so its name is kept there
pub const LANG: &str = ictl_l10n::ENV;
pub const DEV_TOOLS: &str = "IPHONE_CONTROL_DEV_TOOLS";

// notifications
pub const TG_BOT_TOKEN: &str = "TG_BOT_TOKEN";
pub const TG_CHAT_ID: &str = "TG_CHAT_ID";
pub const BARK_SERVER: &str = "BARK_SERVER";
pub const BARK_KEY: &str = "BARK_KEY";

// the control page
pub const VIEWER_PORT: &str = "IPHONE_CONTROL_VIEWER_PORT";
pub const VIEWER_LAN: &str = "IPHONE_CONTROL_VIEWER_LAN";
pub const VIEWER_LAN_HOSTS: &str = "IPHONE_CONTROL_VIEWER_LAN_HOSTS";
pub const VIEWER_MCP: &str = "IPHONE_CONTROL_VIEWER_MCP";
pub const VIEWER_TLS: &str = "IPHONE_CONTROL_VIEWER_TLS";
pub const VIEWER_TLS_CERT: &str = "IPHONE_CONTROL_VIEWER_TLS_CERT";
pub const VIEWER_TLS_KEY: &str = "IPHONE_CONTROL_VIEWER_TLS_KEY";
pub const VIEWER_CODEC: &str = "IPHONE_CONTROL_VIEWER_CODEC";
pub const VIEWER_QP: &str = "IPHONE_CONTROL_VIEWER_QP";

// another machine's control page (`--url`)
pub const URL: &str = "IPHONE_CONTROL_URL";
pub const CA: &str = "IPHONE_CONTROL_CA";
pub const VIEWER_TOKEN: &str = "IPHONE_CONTROL_VIEWER_TOKEN";

// given to a task's command
pub const TASK_TOKEN: &str = "IPHONE_CONTROL_TASK_TOKEN";
pub const TASK_ID: &str = "IPHONE_CONTROL_TASK_ID";
pub const MCP_URL: &str = "IPHONE_CONTROL_MCP_URL";

// diagnostics (`cargo test … -- --ignored`)
pub const RECORDING: &str = "ICTL_RECORDING";
pub const QPS: &str = "ICTL_QPS";

const fn var(name: &'static str, kind: Kind, in_settings_file: bool) -> Var {
    Var { name, kind, in_settings_file }
}

/// All of them
pub const ALL: &[Var] = &[
    var(UDID, Kind::Setting, true),
    var(SCREEN_W, Kind::Setting, true),
    var(SCREEN_H, Kind::Setting, true),
    var(RESIGN_CHECK_HOURS, Kind::Setting, true),
    var(RESIGN_MIN_DAYS, Kind::Setting, true),
    var(OFFLINE_ALERT_MIN, Kind::Setting, true),
    var(DATA_DIR, Kind::Setting, true),
    var(SOCKET, Kind::Setting, true),
    var(LOG, Kind::Setting, true),
    var(ANISETTE_URL, Kind::Setting, true),
    var(DDI_URL, Kind::Setting, true),
    var(DDI_DIR, Kind::Setting, true),
    var(FFMPEG, Kind::Setting, false),
    var(WDA_UNSIGNED, Kind::Setting, true),
    var(WDA_DETACHED, Kind::Experiment, true),
    var(LANG, Kind::Setting, true),
    var(DEV_TOOLS, Kind::Setting, true),
    var(TG_BOT_TOKEN, Kind::Notify, true),
    var(TG_CHAT_ID, Kind::Notify, true),
    var(BARK_SERVER, Kind::Notify, true),
    var(BARK_KEY, Kind::Notify, true),
    var(VIEWER_PORT, Kind::Setting, true),
    var(VIEWER_LAN, Kind::Setting, true),
    var(VIEWER_LAN_HOSTS, Kind::Setting, true),
    var(VIEWER_MCP, Kind::Setting, true),
    var(VIEWER_TLS, Kind::Setting, true),
    var(VIEWER_TLS_CERT, Kind::Setting, true),
    var(VIEWER_TLS_KEY, Kind::Setting, true),
    var(VIEWER_CODEC, Kind::Setting, true),
    var(VIEWER_QP, Kind::Setting, true),
    var(URL, Kind::Setting, false),
    var(CA, Kind::Setting, false),
    var(VIEWER_TOKEN, Kind::Setting, false),
    var(TASK_TOKEN, Kind::Given, false),
    var(TASK_ID, Kind::Given, false),
    var(MCP_URL, Kind::Given, false),
    var("ICTL_UPDATE_PUBLISHER", Kind::Build, false),
    var("ICTL_UPDATE_PUBLIC_KEYS", Kind::Build, false),
    var("ICTL_UPDATE_TAP", Kind::Build, false),
    var(RECORDING, Kind::Dev, false),
    var(QPS, Kind::Dev, false),
];

/// A variable's value, trimmed; `None` when it isn't set or is empty
pub fn get(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn read(rel: &str) -> String {
        std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn known() -> BTreeSet<&'static str> {
        ALL.iter().map(|v| v.name).collect()
    }

    /// Every name a text mentions that looks like one of ours (`IPHONE_CONTROL_…`, `ICTL_…` and the earlier
    /// unprefixed ones)
    fn mentioned(text: &str) -> BTreeSet<String> {
        let unprefixed: BTreeSet<&str> = ALL
            .iter()
            .map(|v| v.name)
            .filter(|n| !n.starts_with("IPHONE_CONTROL_") && !n.starts_with("ICTL_"))
            .collect();
        let mut out = BTreeSet::new();
        for word in text.split(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')) {
            let word = word.trim_matches('_');
            if ((word.starts_with("IPHONE_CONTROL_") || word.starts_with("ICTL_")) && !word.ends_with('_'))
                || unprefixed.contains(word)
            {
                out.insert(word.to_owned());
            }
        }
        out
    }

    #[test]
    fn each_name_once() {
        assert_eq!(known().len(), ALL.len());
    }

    /// The pages describe what there is, and nothing that isn't there any more
    #[test]
    fn the_documentation_matches_the_list() {
        let check = |page: &str, kinds: &[Kind]| {
            let text = read(page);
            let on_page = mentioned(&text);
            for v in ALL.iter().filter(|v| kinds.contains(&v.kind)) {
                assert!(on_page.contains(v.name), "{page} doesn't mention {}", v.name);
            }
            on_page
        };
        for page in ["website/guide/configuration.md", "website/en/guide/configuration.md"] {
            let on_page = check(page, &[Kind::Setting]);
            // a mention of the task's variables, given to `task run`'s command, is fine on this page too
            for name in on_page {
                assert!(known().contains(name.as_str()), "{page} mentions {name}, which isn't in env::ALL");
            }
        }
        for page in ["website/guide/notifications.md", "website/en/guide/notifications.md"] {
            check(page, &[Kind::Notify]);
        }
        check("docs/architecture.md", &[Kind::Experiment]);
        check("docs/dev/release-updates.md", &[Kind::Build]);
        let template = read("packaging/env.example");
        let in_template = mentioned(&template);
        for v in ALL {
            assert_eq!(in_template.contains(v.name), v.in_settings_file, "packaging/env.example and {}", v.name);
        }
    }

    fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                sources(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    /// The code reads them through the names here (`env::VIEWER_PORT`), so this list is all there is: a name in a
    /// string elsewhere fails this. Build-time ones (`option_env!` takes only a literal) and the test harnesses'
    /// own (`ICTL_TEST_…`, integration tests) are the exceptions
    #[test]
    fn the_code_uses_the_names_from_here() {
        let mut files = Vec::new();
        for dir in ["crates", "apps"] {
            sources(&root().join(dir), &mut files);
        }
        let this = Path::new(file!()).file_name().unwrap();
        let mut found = Vec::new();
        for f in files {
            let rel = f.strip_prefix(root()).unwrap().display().to_string();
            // ictl-l10n is below this crate: it keeps the language's name itself, and LANG is that one
            if f.file_name() == Some(this) && rel.contains("ictl-protocol")
                || rel.contains("/tests/")
                || rel.contains("ictl-l10n")
            {
                continue;
            }
            let text = std::fs::read_to_string(&f).unwrap();
            for (n, l) in text.lines().enumerate() {
                if l.contains("option_env!") || l.trim_start().starts_with("//") {
                    continue;
                }
                for quoted in l.split('"').skip(1).step_by(2) {
                    let ours = known().contains(quoted)
                        || (quoted.starts_with("IPHONE_CONTROL_") || quoted.starts_with("ICTL_"))
                            && quoted.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                            && !quoted.starts_with("ICTL_TEST_");
                    if ours {
                        found.push(format!("{rel}:{}: \"{quoted}\"", n + 1));
                    }
                }
            }
        }
        assert!(found.is_empty(), "use the names in ictl_protocol::env instead:\n{}", found.join("\n"));
    }
}

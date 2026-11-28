//! The `KEY=value` format of the settings files (`~/.config/iphone-control/env` and the instance registry's
//! `instances/<n>.env`), as systemd's `EnvironmentFile=`, a POSIX shell (`set -a; . file`) and this program all read
//! it. Shared by the CLI (which writes the settings file) and the daemon (which reads the registry).

/// The key a line assigns, if it is an assignment (`KEY=…`, `export KEY=…`)
pub fn line_key(l: &str) -> Option<&str> {
    let l = l.trim_start();
    let l = l.strip_prefix("export ").unwrap_or(l);
    let (k, _) = l.split_once('=')?;
    (!k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')).then_some(k)
}

/// Every assignment, in order
pub fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let k = line_key(l)?;
            let v = l.trim_start().strip_prefix("export ").unwrap_or(l.trim_start())[k.len() + 1..].trim();
            Some((k.to_owned(), unquote(v)))
        })
        .collect()
}

/// Each key once, with its last value (an empty last one too: it unsets the key, as in the shell)
pub fn last_values(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in pairs {
        out.retain(|(o, _)| *o != k);
        out.push((k, v));
    }
    out
}

fn unquote(v: &str) -> String {
    for q in ['\'', '"'] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return v[1..v.len() - 1].to_owned();
        }
    }
    v.to_owned()
}

/// Bare when that is safe for both the shell and systemd, single-quoted otherwise. `None`: a value with a single
/// quote, a backslash or a newline, which would mean different things to the two readers
pub fn quote(v: &str) -> Option<String> {
    if v.bytes().all(|b| b.is_ascii_alphanumeric() || b"_./:@%+,=-".contains(&b)) {
        return Some(v.to_owned());
    }
    if v.contains(['\'', '\\', '\n', '\r']) {
        return None;
    }
    Some(format!("'{v}'"))
}

//! Puts the control page together: `web/index.html` with each `@include <path>` line replaced by that file (the
//! stylesheet, then the scripts in order), written to `$OUT_DIR/viewer.html`, which `viewer.rs` embeds. The scripts
//! share one scope, as they did in the single file, so the order in `index.html` is the order they run in. No other
//! tool is needed (tests/viewer/page.mjs does the same for the page tests).

use std::path::Path;

fn main() {
    let web = Path::new("web");
    let index = web.join("index.html");
    println!("cargo:rerun-if-changed={}", index.display());
    let text = std::fs::read_to_string(&index).unwrap_or_else(|e| panic!("{}: {e}", index.display()));
    let mut page = String::with_capacity(256 * 1024);
    for line in text.split_inclusive('\n') {
        let Some(rel) = line.strip_prefix("@include ") else {
            page.push_str(line);
            continue;
        };
        let part = web.join(rel.trim());
        println!("cargo:rerun-if-changed={}", part.display());
        let body = std::fs::read_to_string(&part).unwrap_or_else(|e| panic!("{}: {e}", part.display()));
        assert!(body.ends_with('\n'), "{} must end with a line break", part.display());
        assert!(!body.lines().any(|l| l.starts_with("@include ")), "{}: includes don't nest", part.display());
        page.push_str(&body);
    }
    let out = Path::new(&std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("viewer.html");
    std::fs::write(&out, page).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
}

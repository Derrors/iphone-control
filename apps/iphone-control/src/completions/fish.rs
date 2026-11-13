use std::io::{self, Write};

use clap::Command;

use super::{enum_values, visit};

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub(super) fn generate(root: &Command, script: &mut Vec<u8>) -> io::Result<()> {
    // Parse each command's own options before deciding which child was selected.
    // The condition also accepts a positional index to stop offering a consumed enum.
    script.extend_from_slice(
        br#"function __fish_iphone_control_at
    set -l expected $argv[1]
    set -l index $argv[2]
    set -l words (commandline -opc)
    set -e words[1]
    set -l cmd iphone-control
    set -l children
    set -l specs
    while true
        switch $cmd
"#,
    );
    visit(root, root.get_name(), &mut |cmd, path| {
        let mut specs = Vec::new();
        for arg in cmd.get_arguments().filter(|arg| !arg.is_positional()) {
            let mut names = Vec::new();
            if let Some(short) = arg.get_short() {
                names.push(short.to_string());
            }
            if let Some(long) = arg.get_long() {
                names.push(long.to_owned());
            }
            let mut spec = names.join("/");
            if arg.get_action().takes_values() {
                spec.push('=');
            }
            specs.push(quote(&spec));
        }
        let children = cmd
            .get_subcommands()
            .flat_map(|child| child.get_name_and_visible_aliases())
            .map(quote)
            .collect::<Vec<_>>()
            .join(" ");
        writeln!(
            script,
            "            case {}\n                set children {children}\n                set specs {}",
            quote(path),
            specs.join(" ")
        )
    })?;
    script.extend_from_slice(
        br#"        end
        if set -q specs[1]
            argparse -s $specs -- $words 2>/dev/null; or return 1
            set words $argv
        end
        if not set -q words[1]; or not contains -- "$words[1]" $children
            test "$cmd" = "$expected"; and begin
                test -z "$index"; or test (count $words) -eq "$index"
            end
            return $status
        end
        set cmd "$cmd $words[1]"
        set -e words[1]
    end
end
"#,
    );
    visit(root, root.get_name(), &mut |cmd, path| {
        let cond = format!("__fish_iphone_control_at {}", quote(path));
        let prefix = format!("complete -c iphone-control -n {}", quote(&cond));
        for arg in cmd.get_arguments() {
            if arg.is_positional() {
                let values = enum_values(arg);
                if !values.is_empty() {
                    let at = quote(&format!("{cond} {}", arg.get_index().unwrap_or(1) - 1));
                    writeln!(script, "complete -c iphone-control -n {at} -f -a {}", quote(&values))?;
                }
                continue;
            }
            write!(script, "{prefix}")?;
            for short in arg.get_short_and_visible_aliases().unwrap_or_default() {
                write!(script, " -s {short}")?;
            }
            for long in arg.get_long_and_visible_aliases().unwrap_or_default() {
                write!(script, " -l {long}")?;
            }
            if arg.get_action().takes_values() {
                write!(script, " -r")?;
            }
            if let Some(help) = arg.get_help() {
                write!(script, " -d {}", quote(&help.to_string()))?;
            }
            writeln!(script)?;
        }
        for sub in cmd.get_subcommands() {
            for name in sub.get_name_and_visible_aliases() {
                write!(script, "{prefix} -f -a {}", quote(name))?;
                if let Some(help) = sub.get_about() {
                    write!(script, " -d {}", quote(&help.to_string()))?;
                }
                writeln!(script)?;
            }
        }
        Ok(())
    })
}

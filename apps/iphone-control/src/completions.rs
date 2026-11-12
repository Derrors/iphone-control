//! 静态 shell 补全：复用实际 Clap 命令树，不读取配置、获取维护锁或接触设备。

use std::ffi::OsString;
use std::io::{self, Write};

use clap::{CommandFactory, ValueEnum};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

/// 只识别入口，不解析普通命令：构建 Clap 会提前缓存语言，必须留到 envfile 加载后。
/// 根选项的值可以恰好是 `completions`，所以不能只搜索这个单词。
pub fn requested(args: impl IntoIterator<Item = OsString>) -> bool {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--data-dir" || arg == "--socket" {
            if args.next().is_none() {
                return false;
            }
        } else if arg.as_encoded_bytes().starts_with(b"--data-dir=") || arg.as_encoded_bytes().starts_with(b"--socket=")
        {
            continue;
        } else if arg == "help" {
            return args.next().is_some_and(|arg| arg == "completions");
        } else {
            return arg == "completions";
        }
    }
    false
}

pub fn write(shell: Shell, output: &mut dyn Write) -> anyhow::Result<()> {
    let shell = match shell {
        Shell::Bash => clap_complete::Shell::Bash,
        Shell::Zsh => clap_complete::Shell::Zsh,
        Shell::Fish => clap_complete::Shell::Fish,
    };
    // clap_complete expects writes to succeed. Buffer first so a closed stdout is an ordinary
    // I/O error (or a successful early pipe close), rather than a panic inside the generator.
    let mut script = Vec::new();
    let mut command = super::Cli::command();
    clap_complete::generate(shell, &mut command, "iphone-control", &mut script);
    if shell == clap_complete::Shell::Fish {
        fish_positionals(&command, &mut Vec::new(), &mut 0, &mut script)?;
    }
    match output.write_all(&script) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => Ok(result?),
    }
}

// clap_complete 4.6.11 deliberately omits Fish positional arguments. Add only the
// static enum values it misses, still derived from the same built Clap tree.
fn fish_positionals<'a>(
    command: &'a clap::Command,
    ancestors: &mut Vec<&'a clap::Command>,
    next_id: &mut usize,
    script: &mut Vec<u8>,
) -> io::Result<()> {
    ancestors.push(command);
    for arg in command.get_positionals().filter(|arg| !arg.is_hide_set()) {
        let Some(values) = arg.get_value_parser().possible_values() else { continue };
        let values: Vec<_> = values.filter(|value| !value.is_hide_set()).collect();
        if values.is_empty() {
            continue;
        }
        let helper = format!("__fish_iphone_control_positional_{next_id}");
        *next_id += 1;
        writeln!(script, "\nfunction {helper}\n    set -l words (commandline -opc)\n    set -e words[1]")?;
        for (depth, node) in ancestors.iter().enumerate() {
            if depth > 0 {
                writeln!(script, "    test (count $words) -gt 0; or return 1")?;
                writeln!(script, "    test \"$words[1]\" = {}; or return 1", fish_quote(node.get_name()))?;
                writeln!(script, "    set -e words[1]")?;
            }
            // `argparse -s` stops at the first positional, so an option value named
            // `policy`/`completions` can never be mistaken for a subcommand.
            let specs: Vec<_> = node
                .get_arguments()
                .filter(|arg| !arg.is_positional())
                .map(|arg| {
                    let mut spec = String::new();
                    if let Some(short) = arg.get_short() {
                        spec.push(short);
                        if arg.get_long().is_some() {
                            spec.push('/');
                        }
                    }
                    if let Some(long) = arg.get_long() {
                        spec.push_str(long);
                    }
                    if arg.get_num_args().is_some_and(|range| range.takes_values()) {
                        spec.push('=');
                    }
                    fish_quote(&spec)
                })
                .collect();
            writeln!(script, "    argparse -s {} -- $words 2>/dev/null; or return 1", specs.join(" "))?;
            writeln!(script, "    set words $argv")?;
        }
        writeln!(script, "    test (count $words) -eq {}\nend", arg.get_index().unwrap_or(1) - 1)?;
        for value in values {
            writeln!(script, "complete -c iphone-control -n {helper} -f -a {}", fish_quote(value.get_name()))?;
        }
    }
    for subcommand in command.get_subcommands().filter(|cmd| !cmd.is_hide_set()) {
        fish_positionals(subcommand, ancestors, next_id, script)?;
    }
    ancestors.pop();
    Ok(())
}

fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn early_route_recognizes_only_the_completion_command() {
        for args in [
            vec!["completions", "bash"],
            vec!["completions", "--help"],
            vec!["completions", "invalid"],
            vec!["help", "completions"],
            vec!["--data-dir", "somewhere", "--socket=elsewhere", "completions", "fish"],
            vec!["--data-dir=completions", "--socket", "completions", "completions", "zsh"],
        ] {
            assert!(requested(args.iter().map(OsString::from)), "{args:?}");
        }
        for args in [
            vec![],
            vec!["--help"],
            vec!["--data-dir", "completions"],
            vec!["--socket=completions", "status"],
            vec!["login", "completions"],
            vec!["select", "completions"],
            vec!["help", "update"],
            vec!["--unknown", "completions", "bash"],
            vec!["--data-dir"],
        ] {
            assert!(!requested(args.iter().map(OsString::from)), "{args:?}");
        }
    }

    #[test]
    fn all_shells_include_current_commands_options_and_enum_values() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let mut script = Vec::new();
            write(shell, &mut script).unwrap();
            let script = String::from_utf8(script).unwrap();
            for word in [
                "iphone-control",
                "completions",
                "update",
                "policy",
                "check",
                "rollback",
                "recover",
                "service",
                "passcode",
                "viewer",
                "data-dir",
                "socket",
                "reconfigure",
                "force",
                "port",
                "json",
                "manual",
                "notify",
                "auto-patch",
                "bash",
                "zsh",
                "fish",
            ] {
                assert!(script.contains(word), "{shell:?} missing {word}");
            }
        }
    }

    #[test]
    fn completion_argument_is_required_and_restricted_to_supported_shells() {
        for name in ["bash", "zsh", "fish"] {
            assert!(matches!(
                super::super::Cli::try_parse_from(["iphone-control", "completions", name]).unwrap().command,
                Some(super::super::Command::Completions { .. })
            ));
        }
        for args in [
            vec!["iphone-control", "completions"],
            vec!["iphone-control", "completions", "powershell"],
            vec!["iphone-control", "completions", "bash", "extra"],
        ] {
            assert!(super::super::Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn help_and_descriptions_follow_the_selected_language() {
        for (lang, expected) in [(ictl_l10n::Lang::En, "Generate Bash"), (ictl_l10n::Lang::Zh, "生成 Bash")] {
            ictl_l10n::with_lang(lang, || {
                let mut cmd = super::super::Cli::command();
                let help = cmd.find_subcommand_mut("completions").unwrap().render_long_help().to_string();
                assert!(help.contains(expected));
                assert!(help.contains("IPHONE_CONTROL_LANG"));
            });
        }
    }
}

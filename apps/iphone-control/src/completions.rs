//! 静态 shell 补全：复用实际 Clap 命令树，不读取配置、获取维护锁或接触设备。

use std::ffi::OsString;
use std::io::{self, Write};

use clap::{CommandFactory, ValueEnum};

mod bash;
mod fish;

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
    // Generate in memory: closing a downstream pipe should never panic inside a generator.
    let mut command = public_command(&super::Cli::command());
    command.build();
    let mut script = Vec::new();
    // clap_complete 4.6.11 builds inconsistent Bash subcommand names for a
    // hyphenated binary. Its Fish nested-command checks also match option
    // values (e.g. the directory in `--data-dir check update`).
    match shell {
        Shell::Zsh => clap_complete::generate(clap_complete::Shell::Zsh, &mut command, "iphone-control", &mut script),
        Shell::Bash => bash::generate(&command, &mut script)?,
        Shell::Fish => fish::generate(&command, &mut script)?,
    }
    match output.write_all(&script) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => Ok(result?),
    }
}

// Project the public schema before Clap builds its help subcommands. Its stock
// generators otherwise include private scheduler commands and their flags.
fn public_command(source: &clap::Command) -> clap::Command {
    let mut cmd = clap::Command::new(source.get_name().to_owned())
        .visible_aliases(source.get_visible_aliases().map(str::to_owned))
        .args(source.get_arguments().filter(|arg| !arg.is_hide_set()).cloned())
        .subcommands(source.get_subcommands().filter(|cmd| !cmd.is_hide_set()).map(public_command));
    if let Some(about) = source.get_about() {
        cmd = cmd.about(about.clone());
    }
    if let Some(version) = source.get_version() {
        cmd = cmd.version(version.to_owned());
    }
    cmd
}

fn visit(
    cmd: &clap::Command,
    path: &str,
    emit: &mut impl FnMut(&clap::Command, &str) -> io::Result<()>,
) -> io::Result<()> {
    emit(cmd, path)?;
    for child in cmd.get_subcommands() {
        for name in child.get_name_and_visible_aliases() {
            visit(child, &format!("{path} {name}"), emit)?;
        }
    }
    Ok(())
}

fn enum_values(arg: &clap::Arg) -> String {
    arg.get_possible_values().iter().filter(|v| !v.is_hide_set()).map(|v| v.get_name()).collect::<Vec<_>>().join(" ")
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
                "unpin",
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
            assert!(!script.contains("--scheduled"), "{shell:?} exposed a private recovery flag");
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

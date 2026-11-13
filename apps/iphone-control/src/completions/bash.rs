use std::io::{self, Write};

use clap::Command;

use super::{enum_values, visit};

/// Each row describes a public command. Consume option values before recognizing
/// subcommands; neither directory names nor hyphens in the binary name change the path.
pub(super) fn generate(root: &Command, script: &mut Vec<u8>) -> io::Result<()> {
    script.extend_from_slice(
        br#"_iphone_control() {
    local path=iphone-control word cur options children takes position=0 pending=0 end=0 i
    local -a values
    COMPREPLY=()
    for ((i=1; i<=COMP_CWORD; i++)); do
        case "$path" in
"#,
    );
    visit(root, root.get_name(), &mut |cmd, path| {
        let mut flags = Vec::new();
        let mut takes = Vec::new();
        for arg in cmd.get_arguments().filter(|arg| !arg.is_positional()) {
            let mut names: Vec<_> =
                arg.get_short_and_visible_aliases().unwrap_or_default().into_iter().map(|s| format!("-{s}")).collect();
            names.extend(arg.get_long_and_visible_aliases().unwrap_or_default().into_iter().map(|s| format!("--{s}")));
            if arg.get_action().takes_values() {
                takes.extend_from_slice(&names);
            }
            flags.extend(names);
        }
        let children =
            cmd.get_subcommands().flat_map(|child| child.get_name_and_visible_aliases()).collect::<Vec<_>>().join(" ");
        let values = cmd
            .get_positionals()
            .map(|arg| format!("[{}]='{}'", arg.get_index().unwrap_or(1) - 1, enum_values(arg)))
            .collect::<Vec<_>>()
            .join(" ");
        writeln!(
            script,
            "            '{path}') options='{}'; takes='{}'; children='{children}'; values=({values}) ;;",
            flags.join(" "),
            takes.join(" ")
        )
    })?;
    script.extend_from_slice(br#"        esac
        word=${COMP_WORDS[i]}
        if (( i == COMP_CWORD )); then break; fi
        if (( pending )); then pending=0; continue; fi
        if (( !end )); then
            case "$word" in
                --) end=1; continue ;;
                -*) case " $takes " in *" $word "*) pending=1 ;; esac; continue ;;
            esac
            case " $children " in
                *" $word "*) path="$path $word"; position=0; continue ;;
            esac
        fi
        ((position++))
    done
    cur=${COMP_WORDS[COMP_CWORD]}
    # Bash's default completer supplies file names for option values.
    if (( pending )) || [[ $cur == --*=* ]]; then return 0; fi
    if (( end )); then options=""; children=""; fi
    while IFS= read -r word; do COMPREPLY+=("$word"); done < <(compgen -W "$options $children ${values[position]-}" -- "$cur")
    return 0
}
complete -F _iphone_control -o bashdefault -o default iphone-control
"#);
    Ok(())
}

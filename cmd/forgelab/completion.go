package main

import (
	"fmt"
	"os"
)

// completionScript is one bash completion function. zsh runs it through bashcompinit, which
// is how a single small script serves both shells.
//
// Commands and flags are static. Sandbox names are not: they are read from the config by
// asking the forgelab being completed (`__sandboxes`), honouring any --fleet/--config
// already typed on the line.
//
// Those names come from a file that may have arrived with a cloned repository, so they are
// data and are never handed to `compgen -W`, which expands its word list -- command
// substitutions included. They are matched by hand instead, and `__sandboxes` only ever
// prints names made of safe characters in the first place.
const completionScript = `_forgelab() {
    local cur prev cmd i w fleet config name args
    local IFS=$'\n'
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    cmd="${COMP_WORDS[1]}"
    COMPREPLY=()

    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=($(compgen -W "plan${IFS}apply${IFS}verify${IFS}reset${IFS}destroy${IFS}version${IFS}completion" -- "$cur"))
        return
    fi
    case "$cmd" in
        completion)
            [ "$COMP_CWORD" -eq 2 ] && COMPREPLY=($(compgen -W "bash${IFS}zsh" -- "$cur"))
            return ;;
        version|--version) return ;;
    esac

    # bash splits --flag=value into "--flag" "=" "value"; step over the "="
    [ "$prev" = "=" ] && prev="${COMP_WORDS[COMP_CWORD-2]}"
    [ "$cur" = "=" ] && cur=""

    case "$prev" in
        --sandbox|-sandbox)
            fleet="."
            config=""
            for ((i = 2; i < COMP_CWORD; i++)); do
                w="${COMP_WORDS[i]}"
                name="${COMP_WORDS[i+1]}"
                [ "$name" = "=" ] && name="${COMP_WORDS[i+2]}"
                case "$w" in
                    --fleet|-fleet) fleet="$name" ;;
                    --config|-config) config="$name" ;;
                    --fleet=*|-fleet=*) fleet="${w#*=}" ;;
                    --config=*|-config=*) config="${w#*=}" ;;
                esac
            done
            fleet="${fleet/#\~/$HOME}"
            config="${config/#\~/$HOME}"
            w="${COMP_WORDS[0]/#\~/$HOME}"
            args=(__sandboxes --fleet "$fleet")
            [ -n "$config" ] && args+=(--config "$config")
            while IFS= read -r name; do
                case "$name" in
                    "$cur"*) COMPREPLY+=("$name") ;;
                esac
            done < <("$w" "${args[@]}" 2>/dev/null)
            return ;;
        --fleet|-fleet)
            type compopt >/dev/null 2>&1 && compopt -o filenames
            COMPREPLY=($(compgen -d -- "$cur"))
            return ;;
        --config|-config)
            type compopt >/dev/null 2>&1 && compopt -o filenames
            COMPREPLY=($(compgen -f -- "$cur"))
            return ;;
    esac

    COMPREPLY=($(compgen -W "--sandbox${IFS}--fleet${IFS}--config${IFS}--yes${IFS}-v" -- "$cur"))
}
complete -F _forgelab forgelab
`

// zshPreamble leaves an already-initialised completion system alone: running compinit a
// second time costs start-up time in every shell and can drop earlier compdef registrations.
const zshPreamble = `(( $+functions[compdef] )) || { autoload -U compinit && compinit -i; }
autoload -U +X bashcompinit && bashcompinit
`

// completion prints the script for a shell. Meant for: eval "$(forgelab completion zsh)"
func completion(shell string) int {
	switch shell {
	case "bash":
		fmt.Print(completionScript)
	case "zsh":
		fmt.Print(zshPreamble + completionScript)
	default:
		fmt.Fprintln(os.Stderr, `forgelab: usage: forgelab completion bash|zsh    e.g.  eval "$(forgelab completion zsh)"`)
		return 2
	}
	return 0
}

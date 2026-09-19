package main

import "fmt"

// completionScript is one bash completion function. zsh runs it through bashcompinit, which
// is how a single small script serves both shells.
//
// Commands and flags are static. Sandbox names are not: they are read from the config by
// asking forgelab itself (`forgelab __sandboxes`), honouring any --fleet/--config already
// typed on the line.
const completionScript = `_forgelab() {
    local cur prev i fleet config
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"

    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=($(compgen -W "plan apply verify reset destroy version completion" -- "$cur"))
        return
    fi

    case "$prev" in
        --sandbox|-sandbox)
            fleet="."
            config=""
            for ((i = 1; i < COMP_CWORD; i++)); do
                case "${COMP_WORDS[i]}" in
                    --fleet|-fleet) fleet="${COMP_WORDS[i+1]}" ;;
                    --config|-config) config="${COMP_WORDS[i+1]}" ;;
                esac
            done
            fleet="${fleet/#\~/$HOME}"
            config="${config/#\~/$HOME}"
            COMPREPLY=($(compgen -W "$(forgelab __sandboxes --fleet "$fleet" ${config:+--config "$config"} 2>/dev/null)" -- "$cur"))
            return ;;
        --fleet|-fleet)
            COMPREPLY=($(compgen -d -- "$cur"))
            return ;;
        --config|-config)
            COMPREPLY=($(compgen -f -- "$cur"))
            return ;;
        completion)
            COMPREPLY=($(compgen -W "bash zsh" -- "$cur"))
            return ;;
    esac

    COMPREPLY=($(compgen -W "--sandbox --fleet --config --yes -v" -- "$cur"))
}
complete -o filenames -F _forgelab forgelab
`

const zshPreamble = `autoload -U +X compinit 2>/dev/null && compinit -i 2>/dev/null
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
		fmt.Fprintln(stderr, `forgelab: usage: forgelab completion bash|zsh    e.g.  eval "$(forgelab completion zsh)"`)
		return 2
	}
	return 0
}

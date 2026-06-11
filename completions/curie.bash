# Bash completion for curie — the Curie build tool
#
# Install (user):
#   source completions/curie.bash
#   # Or to persist:
#   cp completions/curie.bash ~/.local/share/bash-completion/completions/curie
#
# Install (system-wide):
#   cp completions/curie.bash /usr/share/bash-completion/completions/curie

_curie() {
    local cur prev words cword
    _init_completion || return

    local subcommands="build test run dev clean native list fmt deps publish update audit add remove inspect setup new init maven"

    # Find the subcommand already typed (first non-flag word after 'curie')
    local subcommand=""
    local i
    for (( i = 1; i < cword; i++ )); do
        if [[ "${words[$i]}" != -* ]]; then
            subcommand="${words[$i]}"
            break
        fi
    done

    case "$subcommand" in
        "")
            # Before any subcommand: offer --project and subcommand names
            case "$prev" in
                --project)
                    _filedir -d
                    return
                    ;;
            esac
            if [[ "$cur" == -* ]]; then
                COMPREPLY=( $(compgen -W "--project" -- "$cur") )
            else
                COMPREPLY=( $(compgen -W "$subcommands" -- "$cur") )
            fi
            ;;
        build)
            case "$prev" in
                -j|--jobs) COMPREPLY=(); return ;;
            esac
            COMPREPLY=( $(compgen -W "--no-docker --no-native --offline -j --jobs" -- "$cur") )
            ;;
        test)
            case "$prev" in
                --filter) COMPREPLY=(); return ;;
                -j|--jobs) COMPREPLY=(); return ;;
            esac
            COMPREPLY=( $(compgen -W "--filter --offline -j --jobs" -- "$cur") )
            ;;
        run)
            COMPREPLY=( $(compgen -W "--no-docker --offline" -- "$cur") )
            ;;
        dev)
            COMPREPLY=( $(compgen -W "--offline" -- "$cur") )
            ;;
        clean)
            case "$prev" in
                -j|--jobs) COMPREPLY=(); return ;;
            esac
            COMPREPLY=( $(compgen -W "-j --jobs" -- "$cur") )
            ;;
        native)
            COMPREPLY=( $(compgen -W "--offline" -- "$cur") )
            ;;
        list)
            COMPREPLY=( $(compgen -W "--all" -- "$cur") )
            ;;
        fmt)
            case "$prev" in
                -j|--jobs) COMPREPLY=(); return ;;
            esac
            COMPREPLY=( $(compgen -W "--check --offline -j --jobs" -- "$cur") )
            ;;
        deps)
            case "$prev" in
                --why) COMPREPLY=(); return ;;
            esac
            COMPREPLY=( $(compgen -W "--why --tests --offline" -- "$cur") )
            ;;
        publish)
            case "$prev" in
                --repo) COMPREPLY=(); return ;;
            esac
            COMPREPLY=( $(compgen -W "--repo --no-sign --no-javadoc --dry-run" -- "$cur") )
            ;;
        update)
            COMPREPLY=( $(compgen -W "--check --offline --no-test" -- "$cur") )
            ;;
        audit)
            case "$prev" in
                --severity) COMPREPLY=(); return ;;
                --output) _filedir; return ;;
            esac
            COMPREPLY=( $(compgen -W "--include-test --offline --short --severity --output" -- "$cur") )
            ;;
        add)
            COMPREPLY=( $(compgen -W "--test --annotation-processor --bom --offline" -- "$cur") )
            ;;
        remove)
            COMPREPLY=( $(compgen -W "--test --annotation-processor --bom" -- "$cur") )
            ;;
        inspect)
            COMPREPLY=()
            ;;
        setup)
            case "$prev" in
                --shell) COMPREPLY=( $(compgen -W "fish bash zsh" -- "$cur") ); return ;;
            esac
            COMPREPLY=( $(compgen -W "--shell" -- "$cur") )
            ;;
        maven)
            # Find the maven subcommand (e.g. "sync") already typed
            local maven_cmd=""
            local j
            for (( j = i + 1; j < cword; j++ )); do
                if [[ "${words[$j]}" != -* ]]; then
                    maven_cmd="${words[$j]}"
                    break
                fi
            done
            case "$maven_cmd" in
                "")
                    if [[ "$cur" != -* ]]; then
                        COMPREPLY=( $(compgen -W "sync" -- "$cur") )
                    fi
                    ;;
                sync)
                    COMPREPLY=( $(compgen -W "--check --force" -- "$cur") )
                    ;;
            esac
            ;;
        new|init)
            case "$prev" in
                --package) COMPREPLY=(); return ;;
            esac
            local project_kinds="app lib workspace bom"
            # If no project kind has been given yet, offer the kinds
            local has_kind=0
            for (( i = 1; i < cword; i++ )); do
                if [[ "${words[$i]}" == @(app|lib|workspace|bom) ]]; then
                    has_kind=1
                    break
                fi
            done
            if [[ "$cur" == -* ]] || [[ "$has_kind" == 1 ]]; then
                COMPREPLY=( $(compgen -W "--package" -- "$cur") )
            else
                COMPREPLY=( $(compgen -W "$project_kinds" -- "$cur") )
            fi
            ;;
    esac
}

complete -F _curie curie

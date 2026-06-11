#compdef curie
# Zsh completion for curie — the Curie build tool
#
# Install (user):
#   mkdir -p ~/.zsh/completions
#   cp completions/curie.zsh ~/.zsh/completions/_curie
#   # Ensure ~/.zsh/completions is on fpath (add to ~/.zshrc):
#   #   fpath=(~/.zsh/completions $fpath)
#   #   autoload -Uz compinit && compinit
#
# Install (system-wide):
#   cp completions/curie.zsh /usr/share/zsh/site-functions/_curie

_curie() {
    local context state state_descr line
    typeset -A opt_args

    _arguments -C \
        '--project[Path to project root (default: .)]:project root:_files -/' \
        '1:subcommand:->subcommand' \
        '*::args:->args'

    case $state in
        subcommand)
            local -a subcommands
            subcommands=(
                'build:Compile, test, package JAR, and build Docker image'
                'test:Compile and run tests (no JAR or Docker)'
                'run:Build and run the project'
                'dev:Run in development mode: exploded form, watch and restart on changes'
                'clean:Remove the target/ build directory'
                'native:Compile a GraalVM native binary'
                'list:Show the workspace tree'
                'fmt:Format Java source files'
                'deps:Print the dependency tree'
                'publish:Build, sign, and upload artifacts to Maven'
                'update:Check for newer dependency versions'
                'audit:Emit SBOM and scan for vulnerabilities'
                'add:Add a dependency to Curie.toml'
                'remove:Remove a dependency from Curie.toml'
                'inspect:Inspect last build logs in an interactive TUI'
                'setup:Download and install shell completions'
                'new:Scaffold a new Curie project in a subdirectory'
                'init:Initialise a Curie project in the current directory'
                'maven:Maven interop: generate pom.xml from Curie.toml'
            )
            _describe 'subcommand' subcommands
            ;;
        args)
            case $line[1] in
                build)
                    _arguments \
                        '--no-docker[Skip Docker build]' \
                        '--no-native[Skip native-image compilation]' \
                        '--offline[Use only locally cached artifacts]' \
                        {-j,--jobs}'[Max parallel members (default: CPU count)]:jobs'
                    ;;
                test)
                    _arguments \
                        '--filter[Run only tests matching this class-name pattern]:pattern' \
                        '--offline[Use only locally cached artifacts]' \
                        {-j,--jobs}'[Max parallel members (default: CPU count)]:jobs'
                    ;;
                run)
                    _arguments \
                        '--no-docker[Run directly with java -jar (skip Docker)]' \
                        '--offline[Use only locally cached artifacts]'
                    ;;
                dev)
                    _arguments \
                        '--offline[Use only locally cached artifacts]'
                    ;;
                clean)
                    _arguments \
                        {-j,--jobs}'[Max parallel members (default: CPU count)]:jobs'
                    ;;
                native)
                    _arguments \
                        '--offline[Use only locally cached artifacts]'
                    ;;
                list)
                    _arguments \
                        '--all[Show full workspace tree including unrelated siblings]'
                    ;;
                fmt)
                    _arguments \
                        '--check[Check only; exit non-zero if any file would change]' \
                        '--offline[Do not download formatter JARs]' \
                        {-j,--jobs}'[Max parallel members (default: CPU count)]:jobs'
                    ;;
                deps)
                    _arguments \
                        '--why[Explain why this artifact was selected]:artifact (e.g. org.foo:bar)' \
                        '--tests[Show test-dependencies instead of dependencies]' \
                        '--offline[Use only locally cached POMs]'
                    ;;
                publish)
                    _arguments \
                        '--repo[Override repository URL]:URL' \
                        '--no-sign[Skip GPG signing]' \
                        '--no-javadoc[Skip building the javadoc JAR]' \
                        '--dry-run[Prepare artifacts but do not upload]'
                    ;;
                update)
                    _arguments \
                        '--check[Report updates but do not rewrite Curie.toml]' \
                        '--offline[Skip the network update check]' \
                        '--no-test[Skip test-dependencies and test-bom-imports]'
                    ;;
                audit)
                    _arguments \
                        '--include-test[Include test-scope dependencies in the scan]' \
                        '--offline[Skip the OSV network call; emit SBOM only]' \
                        '--short[Show vulnerability IDs only; exit 1 on any finding]' \
                        '--severity[CVSS score threshold for non-zero exit (default: 7.0)]:score' \
                        '--output[SBOM output path (default: target/sbom.cdx.json)]:file:_files'
                    ;;
                add)
                    _arguments \
                        '--test[Add to [test-dependencies]]' \
                        '--annotation-processor[Add to [annotation-processors]]' \
                        '--bom[Add to [bom-imports] (requires @version)]' \
                        '--offline[Do not access the network]'
                    ;;
                remove)
                    _arguments \
                        '--test[Remove from [test-dependencies]]' \
                        '--annotation-processor[Remove from [annotation-processors]]' \
                        '--bom[Remove from [bom-imports]]'
                    ;;
                inspect)
                    ;;
                setup)
                    _arguments \
                        '--shell[Override shell detection]:shell:(fish bash zsh)'
                    ;;
                new|init)
                    local -a project_kinds
                    project_kinds=(
                        'app:Executable application'
                        'lib:Reusable library'
                        'workspace:Multi-module workspace'
                        'bom:Bill of materials'
                    )
                    _arguments \
                        '1:project kind:->kind' \
                        '--package[Root Java package (e.g. com.example.myapp)]:package'
                    case $state in
                        kind) _describe 'project kind' project_kinds ;;
                    esac
                    ;;
                maven)
                    local -a maven_subcommands
                    maven_subcommands=(
                        'sync:Generate or refresh pom.xml from Curie.toml'
                    )
                    _arguments \
                        '1:maven subcommand:->maven_subcommand' \
                        '*::maven_args:->maven_args'
                    case $state in
                        maven_subcommand) _describe 'maven subcommand' maven_subcommands ;;
                        maven_args)
                            case $line[1] in
                                sync)
                                    _arguments \
                                        '--check[Write nothing; exit 1 if any pom.xml is missing or stale]' \
                                        '--force[Overwrite a pom.xml without the generated-by-Curie marker]'
                                    ;;
                            esac
                            ;;
                    esac
                    ;;
            esac
            ;;
    esac
}

_curie "$@"

# Fish shell completions for curie — the Curie build tool
#
# Install:
#   cp completions/curie.fish ~/.config/fish/completions/
#
# Or for a system-wide install:
#   cp completions/curie.fish /usr/share/fish/vendor_completions.d/

# True when no subcommand has been given on the current command line yet.
function __curie_needs_subcommand
    for i in (commandline -opc)
        if contains -- $i \
                build test run dev clean native list fmt deps \
                publish update audit add remove inspect setup new init maven
            return 1
        end
    end
    return 0
end

# ── Global flag ───────────────────────────────────────────────────────────────
# --project accepts a path, so file completion is left enabled (no -f).
complete -c curie -l project -d "Path to project root (default: .)" -r

# ── Subcommands ───────────────────────────────────────────────────────────────
complete -c curie -f -n __curie_needs_subcommand -a build   -d "Compile, test, package JAR, and build Docker image"
complete -c curie -f -n __curie_needs_subcommand -a test    -d "Compile and run tests (no JAR or Docker)"
complete -c curie -f -n __curie_needs_subcommand -a run     -d "Build and run the project"
complete -c curie -f -n __curie_needs_subcommand -a dev     -d "Run in development mode: exploded form, watch and restart on changes"
complete -c curie -f -n __curie_needs_subcommand -a clean   -d "Remove the target/ build directory"
complete -c curie -f -n __curie_needs_subcommand -a native  -d "Compile a GraalVM native binary"
complete -c curie -f -n __curie_needs_subcommand -a list    -d "Show the workspace tree"
complete -c curie -f -n __curie_needs_subcommand -a fmt     -d "Format Java source files"
complete -c curie -f -n __curie_needs_subcommand -a deps    -d "Print the dependency tree"
complete -c curie -f -n __curie_needs_subcommand -a publish -d "Build, sign, and upload artifacts to Maven"
complete -c curie -f -n __curie_needs_subcommand -a update  -d "Check for newer dependency versions"
complete -c curie -f -n __curie_needs_subcommand -a audit   -d "Emit SBOM and scan for vulnerabilities"
complete -c curie -f -n __curie_needs_subcommand -a add     -d "Add a dependency to Curie.toml"
complete -c curie -f -n __curie_needs_subcommand -a remove  -d "Remove a dependency from Curie.toml"
complete -c curie -f -n __curie_needs_subcommand -a inspect -d "Inspect last build logs in an interactive TUI"
complete -c curie -f -n __curie_needs_subcommand -a setup   -d "Download and install shell completions"
complete -c curie -f -n __curie_needs_subcommand -a new     -d "Scaffold a new Curie project in a subdirectory"
complete -c curie -f -n __curie_needs_subcommand -a init    -d "Initialise a Curie project in the current directory"
complete -c curie -f -n __curie_needs_subcommand -a maven   -d "Maven interop: generate pom.xml from Curie.toml"

# ── build ─────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from build" -l no-docker -d "Skip Docker build"
complete -c curie -f -n "__fish_seen_subcommand_from build" -l no-native -d "Skip native-image compilation"
complete -c curie -f -n "__fish_seen_subcommand_from build" -l offline   -d "Use only locally cached artifacts"
complete -c curie -f -n "__fish_seen_subcommand_from build" -s j -l jobs -d "Max parallel members (default: CPU count)" -r

# ── test ──────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from test" -l filter  -d "Run only tests matching this class-name pattern" -r
complete -c curie -f -n "__fish_seen_subcommand_from test" -l offline -d "Use only locally cached artifacts"
complete -c curie -f -n "__fish_seen_subcommand_from test" -s j -l jobs -d "Max parallel members (default: CPU count)" -r

# ── run ───────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from run" -l no-docker -d "Run directly with java -jar (skip Docker)"
complete -c curie -f -n "__fish_seen_subcommand_from run" -l offline   -d "Use only locally cached artifacts"

# ── dev ───────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from dev" -l offline -d "Use only locally cached artifacts"

# ── clean ─────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from clean" -s j -l jobs -d "Max parallel members (default: CPU count)" -r

# ── native ────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from native" -l offline -d "Use only locally cached artifacts"

# ── list ──────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from list" -l all -d "Show full workspace tree including unrelated siblings"

# ── fmt ───────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from fmt" -l check   -d "Check only; exit non-zero if any file would change"
complete -c curie -f -n "__fish_seen_subcommand_from fmt" -l offline -d "Do not download formatter JARs"

# ── deps ──────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from deps" -l why     -d "Explain why this artifact was selected (e.g. org.foo:bar)" -r
complete -c curie -f -n "__fish_seen_subcommand_from deps" -l tests   -d "Show test-dependencies instead of dependencies"
complete -c curie -f -n "__fish_seen_subcommand_from deps" -l offline -d "Use only locally cached POMs"

# ── publish ───────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from publish" -l repo       -d "Override repository URL" -r
complete -c curie -f -n "__fish_seen_subcommand_from publish" -l no-sign    -d "Skip GPG signing"
complete -c curie -f -n "__fish_seen_subcommand_from publish" -l no-javadoc -d "Skip building the javadoc JAR"
complete -c curie -f -n "__fish_seen_subcommand_from publish" -l dry-run    -d "Prepare artifacts but do not upload"

# ── update ────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from update" -l check   -d "Report updates but do not rewrite Curie.toml"
complete -c curie -f -n "__fish_seen_subcommand_from update" -l offline -d "Skip the network update check"
complete -c curie -f -n "__fish_seen_subcommand_from update" -l no-test -d "Skip test-dependencies and test-bom-imports"

# ── audit ─────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from audit" -l include-test -d "Include test-scope dependencies in the scan"
complete -c curie -f -n "__fish_seen_subcommand_from audit" -l offline      -d "Skip the OSV network call; emit SBOM only"
complete -c curie -f -n "__fish_seen_subcommand_from audit" -l short        -d "Show vulnerability IDs only; exit 1 on any finding"
complete -c curie -f -n "__fish_seen_subcommand_from audit" -l severity     -d "CVSS score threshold for non-zero exit (default: 7.0)" -r
# --output takes a file path: leave file completion enabled (no -f)
complete -c curie    -n "__fish_seen_subcommand_from audit" -l output       -d "SBOM output path (default: target/sbom.cdx.json)" -r

# ── add ───────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from add" -l test                 -d "Add to [test-dependencies]"
complete -c curie -f -n "__fish_seen_subcommand_from add" -l annotation-processor -d "Add to [annotation-processors]"
complete -c curie -f -n "__fish_seen_subcommand_from add" -l bom                  -d "Add to [bom-imports] (requires @version)"
complete -c curie -f -n "__fish_seen_subcommand_from add" -l offline              -d "Do not access the network"

# ── remove ────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from remove" -l test                 -d "Remove from [test-dependencies]"
complete -c curie -f -n "__fish_seen_subcommand_from remove" -l annotation-processor -d "Remove from [annotation-processors]"
complete -c curie -f -n "__fish_seen_subcommand_from remove" -l bom                  -d "Remove from [bom-imports]"

# ── setup ─────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from setup" -l shell -d "Override shell detection: fish, bash, or zsh" -r

# ── new ───────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from new" -a app       -d "Executable application"
complete -c curie -f -n "__fish_seen_subcommand_from new" -a lib       -d "Reusable library"
complete -c curie -f -n "__fish_seen_subcommand_from new" -a workspace -d "Multi-module workspace"
complete -c curie -f -n "__fish_seen_subcommand_from new" -a bom       -d "Bill of materials"
complete -c curie -f -n "__fish_seen_subcommand_from new" -l package   -d "Root Java package (e.g. com.example.myapp)" -r

# ── init ──────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from init" -a app       -d "Executable application"
complete -c curie -f -n "__fish_seen_subcommand_from init" -a lib       -d "Reusable library"
complete -c curie -f -n "__fish_seen_subcommand_from init" -a workspace -d "Multi-module workspace"
complete -c curie -f -n "__fish_seen_subcommand_from init" -a bom       -d "Bill of materials"
complete -c curie -f -n "__fish_seen_subcommand_from init" -l package   -d "Root Java package (e.g. com.example.myapp)" -r

# ── maven ─────────────────────────────────────────────────────────────────────
complete -c curie -f -n "__fish_seen_subcommand_from maven; and not __fish_seen_subcommand_from sync" -a sync -d "Generate or refresh pom.xml from Curie.toml"
complete -c curie -f -n "__fish_seen_subcommand_from maven; and __fish_seen_subcommand_from sync" -l check -d "Write nothing; exit 1 if any pom.xml is missing or stale"
complete -c curie -f -n "__fish_seen_subcommand_from maven; and __fish_seen_subcommand_from sync" -l force -d "Overwrite a pom.xml without the generated-by-Curie marker"

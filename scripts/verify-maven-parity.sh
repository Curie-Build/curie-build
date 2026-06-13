#!/usr/bin/env bash
#
# verify-maven-parity.sh — verify that `mvn verify` and `curie build`
# produce equivalent results for the examples, per the parity contract in
# _design/MAVEN-SYNC.md ("Parity contract — what '1:1' means").
#
# For every module in the matrix below this script:
#
#   1. Resolution parity — `curie deps` (+ `--tests`) vs `mvn dependency:tree`
#      (compile and test scopes), normalized to sorted
#      `group:artifact:version` lists. Any difference is reported first and
#      is most likely a Curie resolver bug (see _design/MAVEN-SYNC.md,
#      "Known divergences" #1).
#
#      Four expected, by-design differences are filtered out before
#      comparing:
#        - `org.junit.platform:junit-platform-console-standalone` (+ its
#          transitive closure): materialized into the generated pom.xml only
#          when no test engine is otherwise declared, but never appears in
#          `curie deps` since Curie bundles it rather than resolving it.
#        - intra-workspace dependencies (e.g. `string-utils-cli` ->
#          `string-utils`): materialized as a real Maven `<dependency>` in
#          the generated pom.xml, but `curie deps` only lists external Maven
#          coordinates.
#        - `org.projectlombok:lombok`: an `on-compile-classpath = true`
#          annotation processor (lombok-greeter) is additionally emitted as a
#          `<scope>provided</scope>` dependency so javac's `-cp` sees it (see
#          MAVEN-SYNC.md "Annotation processors"), but `curie deps` lists
#          resolved [dependencies]/[test-dependencies], not annotation
#          processors.
#        - `org.apache.groovy:groovy`, `org.jetbrains.kotlin:kotlin-stdlib`
#          (+ its `org.jetbrains:annotations:13.0` dep), and
#          `org.spockframework:spock-core`'s transitive closure (pinned by
#          `[spock].version()` = 2.4-groovy-5.0): Groovy/Kotlin stdlib are
#          always part of Curie's internal `effective_dep_jars`/`target/libs`
#          (build.rs) and spock-core is resolved internally for `[spock]`
#          (test.rs's `spock_jars`) — none of these appear in `curie deps`,
#          but the generated pom.xml declares them as real `<dependency>`
#          entries so `mvn` can compile/run. Filtered symmetrically from both
#          sides (BY_DESIGN_GAV_PATTERNS below), so modules that *do*
#          declare one of these explicitly (e.g. spring-boot-demo's
#          `org.spockframework:spock-spring` test dependency) still compare
#          their other GAVs normally.
#
#   2. Build-output parity:
#        - JAR entry name set + per-entry SHA-256 (`META-INF/MANIFEST.MF` is
#          compared separately below; `META-INF/build-info.properties` is
#          Curie-only — _design/MAVEN-SYNC.md divergence #4 — and excluded)
#        - `META-INF/MANIFEST.MF`: `Main-Class` (exact) and `Class-Path`
#          (as a set of `libs/*.jar` names — order may differ)
#        - `target/libs/*.jar` name set
#        - fat-jar entry name set, when `[fat-jar]` is enabled
#        - test class names, per-run total/failed counts
#
# Usage:
#   scripts/verify-maven-parity.sh
#
# Exits non-zero if any module fails any check.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXAMPLES_DIR="$ROOT_DIR/examples"
CURIE_BIN="$ROOT_DIR/target/debug/curie"

# ---------------------------------------------------------------------------
# Module matrix — see _design/MAVEN-SYNC.md "Expected parity matrix".
#
# REACTOR_MODULES are members of the examples/ workspace (incl. the nested
# `nested-workspace-demo` workspace); they are built once via a single `mvn
# verify` reactor build. my-platform-bom ([bom]: no jar/tests, the generated
# pom.xml *is* the artifact) and the pure aggregator directories
# (nested-workspace-demo, .../services, .../services/apps) are not buildable
# jars and are not part of this matrix.
#
# STANDALONE_MODULES each get their own scratch copy + `mvn verify`.
#
# Excluded entirely: proto-greeter, openapi-greeter ([plugin.*]-generated
# sources have no pom.xml equivalent — see maven::print_plugin_source_warning
# and _design/MAVEN-SYNC.md divergence #7).
# ---------------------------------------------------------------------------

REACTOR_MODULES=(
  hello-world
  jackson-bom-greeter
  json-greeter
  string-utils
  string-utils-cli
  auto-value-demo
  lombok-greeter
  maven-string-utils
  hello-kotlin
  hello-mixed
  graalvm-hello
  verbose-test-demo
  fat-jar-demo
  nested-workspace-demo/core-lib
  nested-workspace-demo/services/greeter-lib
  nested-workspace-demo/services/apps/hello-app
)

STANDALONE_MODULES=(
  add-remove-demo
  exclusion-demo
  version-range-demo
  publish-demo
  groovy-greeter
  spock-demo
  spring-boot-demo
  shibboleth-demo
  google-mirror-demo
  simplified-main
)

SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT

FAILURES=0
WORKSPACE_MEMBER_GAS=""   # "group:artifact" of every reactor module, newline-separated

# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------

ok()   { echo "OK:   $1"; }
fail() { echo "FAIL: $1"; FAILURES=$((FAILURES + 1)); }

pom_field() {
  # $1 = pom.xml path, $2 = direct-child element name (e.g. artifactId)
  xmllint --xpath "string(/*[local-name()='project']/*[local-name()='$2'])" "$1" 2>/dev/null
}

diff_sets() {
  # $1 = label, $2 = expected (curie) set, $3 = actual (maven) set
  # both newline-separated, already sorted
  local label=$1 curie_set=$2 maven_set=$3
  if [[ "$curie_set" == "$maven_set" ]]; then
    ok "$label"
  else
    fail "$label differ"
    diff <(echo "$curie_set") <(echo "$maven_set") | sed 's/^/    /' || true
  fi
}

# ---------------------------------------------------------------------------
# Resolution parity: `curie deps` vs `mvn dependency:tree`
# ---------------------------------------------------------------------------

# By-design GAVs (or group:artifact prefixes) filtered from BOTH
# curie_gavs() and mvn_gavs() output before comparing — see file header,
# 4th bullet. Each pattern is matched as a substring (grep -F), so entries
# ending in `:` match any version while exact `group:artifact:version`
# entries (the spock-core 2.4-groovy-5.0 closure) only match that pinned
# version. Filtering both sides symmetrically is a no-op where the GAV
# legitimately appears on both (e.g. spring-boot-demo's explicit
# `org.spockframework:spock-spring` test dependency pulls in the same
# spock-core closure).
BY_DESIGN_GAV_PATTERNS=(
  'org.apache.groovy:groovy:'
  'org.jetbrains.kotlin:kotlin-stdlib:'
  'org.jetbrains:annotations:13.0'
  'org.spockframework:spock-core:2.4-groovy-5.0'
  'org.junit.platform:junit-platform-engine:1.14.1'
  'org.junit.platform:junit-platform-commons:1.14.1'
  'org.opentest4j:opentest4j:1.3.0'
  'org.apiguardian:apiguardian-api:1.1.2'
  'org.hamcrest:hamcrest:3.0'
  'io.leangen.geantyref:geantyref:1.3.16'
)

filter_by_design_gavs() {
  grep -vFf <(printf '%s\n' "${BY_DESIGN_GAV_PATTERNS[@]}")
}

# Normalize a `curie deps` / `curie deps --tests` tree to sorted
# "group:artifact:version" lines.
curie_gavs() {
  local dir=$1 include_tests=$2
  (
    cd "$dir"
    "$CURIE_BIN" deps --offline 2>/dev/null
    if [[ "$include_tests" == "1" ]]; then
      "$CURIE_BIN" deps --tests --offline 2>/dev/null
    fi
  ) | sed -E 's/^[^a-zA-Z0-9]+//' | grep -E '^[^:]+:[^:]+:[^:]+$' | sort -u | filter_by_design_gavs
}

# Normalize an `mvn dependency:tree` output to sorted "group:artifact:version"
# lines. junit-platform-console-standalone's subtree, the lombok
# on-compile-classpath `provided` dependency, and intra-workspace
# dependencies are pruned (see file header).
mvn_gavs() {
  local dir=$1 scope=$2
  local out="$SCRATCH/tree.txt"
  (
    cd "$dir"
    mvn -q dependency:tree -Dscope="$scope" \
      -Dexcludes=org.junit.platform:junit-platform-console-standalone,org.projectlombok:lombok \
      -DoutputFile="$out" >/dev/null
  )
  tail -n +2 "$out" \
    | sed -E 's/^[^a-zA-Z0-9]+//' \
    | awk -F: '{ if (NF==5) print $1":"$2":"$4; else if (NF==6) print $1":"$2":"$5 }' \
    | grep -vFf <(echo "$WORKSPACE_MEMBER_GAS" | sed -E '/^$/d; s/$/:/') \
    | sort -u | filter_by_design_gavs
}

check_resolution_parity() {
  local curie_dir=$1 maven_dir=$2 label=$3

  diff_sets "$label: compile-scope deps" \
    "$(curie_gavs "$curie_dir" 0)" \
    "$(mvn_gavs "$maven_dir" compile)"

  diff_sets "$label: test-scope deps" \
    "$(curie_gavs "$curie_dir" 1)" \
    "$(mvn_gavs "$maven_dir" test)"
}

# ---------------------------------------------------------------------------
# Build-output parity
# ---------------------------------------------------------------------------

unwrap_manifest() {
  # Join MANIFEST.MF continuation lines (leading single space) back onto the
  # logical line they continue.
  unzip -p "$1" META-INF/MANIFEST.MF | tr -d '\r' | awk '
    /^ / { line = line substr($0, 2); next }
    { if (line != "") print line; line = $0 }
    END { if (line != "") print line }
  '
}

jar_entry_names() {
  unzip -Z1 "$1" | grep -vE '^META-INF/(MANIFEST\.MF|build-info\.properties)$' | sort
}

# Entry name set only, without printing ok/fail — callers report the
# overall result themselves. Returns 1 (and prints a diff) on mismatch.
jar_entry_sets_match() {
  local label=$1 curie_jar=$2 maven_jar=$3

  local curie_entries maven_entries
  curie_entries=$(jar_entry_names "$curie_jar")
  maven_entries=$(jar_entry_names "$maven_jar")

  if [[ "$curie_entries" != "$maven_entries" ]]; then
    fail "$label: jar entry sets differ"
    diff <(echo "$curie_entries") <(echo "$maven_entries") | sed 's/^/    /' || true
    return 1
  fi
}

# Entry name set only — no digests. Used for the fat jar: per
# _design/MAVEN-SYNC.md divergence #9, relocated `.class` entries are not
# byte-identical between Curie's in-place constant-pool rewrite and
# maven-shade-plugin's ASM-based re-serialization, so the fat-jar contract
# ("Fat JAR" row) only requires the same merged entry set.
compare_jar_entry_names() {
  local label=$1 curie_jar=$2 maven_jar=$3
  jar_entry_sets_match "$label" "$curie_jar" "$maven_jar" && ok "$label: entry sets match"
}

# Entry name set + per-entry SHA-256, for regular (non-shaded) jars.
compare_jar_entries() {
  local label=$1 curie_jar=$2 maven_jar=$3

  local curie_entries
  curie_entries=$(jar_entry_names "$curie_jar")
  jar_entry_sets_match "$label" "$curie_jar" "$maven_jar" || return

  local mismatch=0
  while IFS= read -r entry; do
    [[ -z "$entry" || "$entry" == */ ]] && continue
    local c m
    c=$(unzip -p "$curie_jar" "$entry" | sha256sum | cut -d' ' -f1)
    m=$(unzip -p "$maven_jar" "$entry" | sha256sum | cut -d' ' -f1)
    if [[ "$c" != "$m" ]]; then
      echo "    entry '$entry' digest differs"
      mismatch=1
    fi
  done <<< "$curie_entries"

  if [[ $mismatch -eq 0 ]]; then
    ok "$label: entries + digests match"
  else
    fail "$label: entry digests differ"
  fi
}

compare_manifest() {
  local label=$1 curie_jar=$2 maven_jar=$3

  local curie_main maven_main curie_cp maven_cp
  curie_main=$(unwrap_manifest "$curie_jar" | grep '^Main-Class:' || true)
  maven_main=$(unwrap_manifest "$maven_jar" | grep '^Main-Class:' || true)
  curie_cp=$( (unwrap_manifest "$curie_jar" | grep '^Class-Path:' || true) | sed 's/^Class-Path: *//' | tr ' ' '\n' | sort)
  maven_cp=$( (unwrap_manifest "$maven_jar" | grep '^Class-Path:' || true) | sed 's/^Class-Path: *//' | tr ' ' '\n' | sort)

  if [[ "$curie_main" != "$maven_main" ]]; then
    fail "$label: Main-Class differs ('$curie_main' vs '$maven_main')"
  elif [[ "$curie_cp" != "$maven_cp" ]]; then
    fail "$label: Class-Path entries differ"
    diff <(echo "$curie_cp") <(echo "$maven_cp") | sed 's/^/    /' || true
  else
    ok "$label: Main-Class/Class-Path match"
  fi
}

check_jar_parity() {
  local curie_dir=$1 maven_dir=$2 label=$3

  local artifact version
  artifact=$(pom_field "$curie_dir/pom.xml" artifactId)
  version=$(pom_field "$curie_dir/pom.xml" version)

  local curie_jar="$curie_dir/target/$artifact-$version.jar"
  local maven_jar="$maven_dir/target/$artifact-$version.jar"

  if [[ ! -f "$curie_jar" ]]; then
    echo "SKIP: $label: no curie jar at target/$artifact-$version.jar"
    return
  fi
  if [[ ! -f "$maven_jar" ]]; then
    fail "$label: maven jar missing at target/$artifact-$version.jar"
    return
  fi

  # `compare_jar_entries`/`compare_jar_entry_names` return non-zero when the
  # entry sets differ (after already reporting FAIL + a diff); `|| true`
  # keeps that from tripping `set -e` and aborting the whole script.
  compare_jar_entries "$label: jar" "$curie_jar" "$maven_jar" || true
  compare_manifest "$label: jar" "$curie_jar" "$maven_jar"

  local curie_fat="$curie_dir/target/$artifact-$version-fat.jar"
  local maven_fat="$maven_dir/target/$artifact-$version-fat.jar"
  if [[ -f "$curie_fat" ]]; then
    if [[ -f "$maven_fat" ]]; then
      compare_jar_entry_names "$label: fat jar" "$curie_fat" "$maven_fat" || true
    else
      fail "$label: fat jar missing from maven output"
    fi
  fi
}

check_libs_parity() {
  local curie_dir=$1 maven_dir=$2 label=$3

  local curie_libs maven_libs
  curie_libs=$(ls "$curie_dir/target/libs" 2>/dev/null | sort || true)
  maven_libs=$(ls "$maven_dir/target/libs" 2>/dev/null | sort || true)

  diff_sets "$label: target/libs/" "$curie_libs" "$maven_libs"
}

check_test_parity() {
  local curie_dir=$1 maven_dir=$2 label=$3

  local curie_json="$curie_dir/target/build.tests.json"
  local maven_reports="$maven_dir/target/surefire-reports"

  local curie_classes curie_total curie_failed
  if [[ -f "$curie_json" ]]; then
    curie_classes=$(jq -r '.[].class_name' "$curie_json" | sort -u)
    curie_total=$(jq 'length' "$curie_json")
    curie_failed=$(jq '[.[] | select(.status != "passed")] | length' "$curie_json")
  else
    curie_classes=""
    curie_total=0
    curie_failed=0
  fi

  local maven_classes maven_total maven_failed
  maven_classes=""
  maven_total=0
  maven_failed=0
  if [[ -d "$maven_reports" ]]; then
    for report in "$maven_reports"/TEST-*.xml; do
      [[ -e "$report" ]] || continue
      maven_classes+="$(xmllint --xpath 'string(/testsuite/@name)' "$report")"$'\n'
      local t fa er
      t=$(xmllint --xpath 'string(/testsuite/@tests)' "$report")
      fa=$(xmllint --xpath 'string(/testsuite/@failures)' "$report")
      er=$(xmllint --xpath 'string(/testsuite/@errors)' "$report")
      maven_total=$((maven_total + t))
      maven_failed=$((maven_failed + fa + er))
    done
    maven_classes=$(echo "$maven_classes" | sed '/^$/d' | sort -u)
  fi

  if [[ "$curie_classes" != "$maven_classes" ]]; then
    fail "$label: test classes differ"
    diff <(echo "$curie_classes") <(echo "$maven_classes") | sed 's/^/    /' || true
  elif [[ "$curie_total" != "$maven_total" || "$curie_failed" != "$maven_failed" ]]; then
    fail "$label: test counts differ (curie: $curie_total total/$curie_failed failed, mvn: $maven_total total/$maven_failed failed)"
  else
    local n
    n=$(echo "$curie_classes" | grep -c . || true)
    ok "$label: tests match ($curie_total total, $curie_failed failed, $n class(es))"
  fi
}

check_module() {
  local curie_dir=$1 maven_dir=$2 label=$3

  check_resolution_parity "$curie_dir" "$maven_dir" "$label"
  check_jar_parity "$curie_dir" "$maven_dir" "$label"
  check_libs_parity "$curie_dir" "$maven_dir" "$label"
  check_test_parity "$curie_dir" "$maven_dir" "$label"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

main() {
  echo "==> Building curie"
  (cd "$ROOT_DIR" && cargo build -q -p curie-build)

  echo "==> curie build (examples workspace)"
  "$CURIE_BIN" --project "$EXAMPLES_DIR" build --no-docker --no-native >/dev/null

  local mvn_examples="$SCRATCH/examples"
  echo "==> mvn install (examples workspace, scratch copy at $mvn_examples)"
  rsync -a --exclude target "$EXAMPLES_DIR/" "$mvn_examples/"
  # `install` (not just `verify`) so intra-workspace dependencies
  # (e.g. string-utils-cli -> string-utils) are in the local repo for the
  # per-module `dependency:tree` calls below.
  if ! (cd "$mvn_examples" && mvn -q install > "$SCRATCH/mvn-examples.log" 2>&1); then
    fail "examples workspace: mvn install failed (see $SCRATCH/mvn-examples.log)"
    tail -40 "$SCRATCH/mvn-examples.log"
  fi

  # Precompute the set of "group:artifact" GAVs for every reactor module, so
  # mvn_gavs() can drop intra-workspace dependencies from the maven side.
  for rel in "${REACTOR_MODULES[@]}"; do
    local pom="$EXAMPLES_DIR/$rel/pom.xml"
    local ga
    ga="$(pom_field "$pom" groupId):$(pom_field "$pom" artifactId)"
    WORKSPACE_MEMBER_GAS+="$ga"$'\n'
  done

  for rel in "${REACTOR_MODULES[@]}"; do
    echo "--- examples/$rel ---"
    check_module "$EXAMPLES_DIR/$rel" "$mvn_examples/$rel" "examples/$rel"
  done

  for rel in "${STANDALONE_MODULES[@]}"; do
    echo "--- examples/$rel ---"
    local curie_dir="$EXAMPLES_DIR/$rel"
    local maven_dir="$SCRATCH/$rel"

    echo "==> curie build (examples/$rel)"
    "$CURIE_BIN" --project "$curie_dir" build --no-docker --no-native >/dev/null

    echo "==> mvn install (examples/$rel, scratch copy)"
    rsync -a --exclude target "$curie_dir/" "$maven_dir/"
    if ! (cd "$maven_dir" && mvn -q install > "$SCRATCH/mvn-$(basename "$rel").log" 2>&1); then
      fail "examples/$rel: mvn install failed (see $SCRATCH/mvn-$(basename "$rel").log)"
      tail -40 "$SCRATCH/mvn-$(basename "$rel").log"
      continue
    fi

    check_module "$curie_dir" "$maven_dir" "examples/$rel"
  done

  echo "==============================================================="
  if [[ $FAILURES -eq 0 ]]; then
    echo "All parity checks passed."
  else
    echo "$FAILURES parity check(s) failed."
  fi
  exit "$FAILURES"
}

main "$@"

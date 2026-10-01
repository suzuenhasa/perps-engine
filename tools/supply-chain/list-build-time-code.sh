#!/usr/bin/env bash
# Lists every dependency whose code runs at build time on this platform, which is what gets
# read before the first build of a new dependency (INFO.md 5a). That is:
#   - crates with a build script (build.rs), whose build script runs;
#   - build-dependencies of any crate (e.g. `cc`), plus everything they depend on;
#   - procedural-macro crates, plus everything they depend on (e.g. `syn`), because a
#     proc-macro runs inside the compiler.
# A build-script crate's *normal* dependencies are not included: they are compiled, not run.
#
# Output: name, version, why it runs at build time.
# Inspect-only: uses `cargo metadata`, compiles nothing. Run with:
#   ./dev --net tools/supply-chain/list-build-time-code.sh [target-triple]
set -euo pipefail
TARGET="${1:-x86_64-unknown-linux-gnu}"

cargo metadata --format-version 1 --locked --filter-platform "$TARGET" | jq -r '
  (.packages | map({key: .id, value: .}) | from_entries) as $pkg
  | (.resolve.nodes | map({key: .id, value: .deps}) | from_entries) as $deps
  | def kinds($p): [$pkg[$p].targets[].kind[]];
    def normal_deps($p): [$deps[$p][] | select(.dep_kinds | any(.kind == null)) | .pkg];
    def build_deps($p): [$deps[$p][] | select(.dep_kinds | any(.kind == "build")) | .pkg];
  # Roots whose whole dependency closure runs at build time.
  ( [ .resolve.nodes[].id | select(kinds(.) | index("proc-macro")) ]
    + [ .resolve.nodes[].id | build_deps(.)[] ] ) as $roots
  # Everything reachable from those roots through normal dependencies.
  | {todo: $roots, seen: {}}
  | until(.todo | length == 0;
      .todo[0] as $x | .todo |= .[1:]
      | if .seen[$x] then . else .seen[$x] = true | .todo += normal_deps($x) end)
  | .seen as $closure
  | [ $pkg | to_entries[] | .value
      | select(.source != null)
      | .id as $id
      | (kinds($id)) as $k
      | select($closure[$id] or ($k | index("custom-build")))
      | "\(.name)\t\(.version)\t"
        + ([ (if $k | index("proc-macro") then "proc-macro" else empty end),
             (if $k | index("custom-build") then "build-script" else empty end),
             (if $closure[$id] and ($k | index("proc-macro") | not) then "runs-inside-build-code" else empty end)
           ] | join(",")) ]
  | sort[]'

#!/usr/bin/env bash
# Upgrade-delay check (INFO.md 5a): flags every crate version in Cargo.lock that was
# published less than MIN_DAYS ago. Most hijacked releases are caught within days, so a
# version that has been public for two weeks has had time to be noticed.
#
# Also prints each crate's total downloads, so a lookalike with almost no users stands out.
#
# Download-only: queries the crates.io API, compiles nothing. Run with:
#   ./dev --net tools/supply-chain/check-ages.sh
#
# Exception (D-006): a release that fixes a published security advisory may be taken
# before it is 14 days old, once its build-time code has been read. List such versions in
# ALLOW, e.g. ALLOW="rustls@0.23.45"; they are reported but don't fail the check.
set -euo pipefail
MIN_DAYS="${MIN_DAYS:-14}"
ALLOW="${ALLOW:-}"
UA="perps-supply-chain-check (github.com/suzuenhasa)"   # crates.io asks for a contact UA
now=$(date -u +%s)
fail=0

# name<TAB>version for every registry package in the lockfile.
pairs=$(awk '/^\[\[package\]\]/{n="";v="";s=""} /^name = /{n=$3} /^version = /{v=$3}
             /^source = "registry/{gsub(/"/,"",n); gsub(/"/,"",v); print n"\t"v}' Cargo.lock)

printf '%-28s %-12s %6s %14s\n' crate version age_d downloads
while IFS=$'\t' read -r name version; do
    info=$(curl -sS -A "$UA" "https://crates.io/api/v1/crates/$name/$version")
    created=$(jq -r '.version.created_at' <<<"$info")
    sleep 1
    downloads=$(curl -sS -A "$UA" "https://crates.io/api/v1/crates/$name" | jq -r '.crate.downloads')
    age=$(( (now - $(date -u -d "$created" +%s)) / 86400 ))
    flag=""
    if (( age < MIN_DAYS )); then
        if [[ " $ALLOW " == *" $name@$version "* ]]; then
            flag="  <-- younger than $MIN_DAYS days, allowed (advisory fix)"
        else
            flag="  <-- younger than $MIN_DAYS days"; fail=1
        fi
    fi
    printf '%-28s %-12s %6s %14s%s\n' "$name" "$version" "$age" "$downloads" "$flag"
    sleep 1   # crates.io crawler policy: at most one request per second
done <<<"$pairs"

exit "$fail"

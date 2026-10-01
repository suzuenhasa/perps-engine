# Supply-chain log

What was checked before dependency code first ran, and what the checks found. The policy
is D-001 and D-006 in `docs/DECISIONS.md`; the tools are in `tools/supply-chain/`.

**Who reviewed.** The build-time code reviews below were done by Claude Code subagents
(parallel AI reviewers, each given the unmodified crate sources and a fixed checklist),
not by me line by line. I directed the review and read its findings.

Legend: **clean** means the code that runs at build time was covered and does only what its
purpose needs (compiler version probes, `cfg` flags, compiling the crate's own C/asm into
`OUT_DIR`). "Read" means every line of the build script or proc-macro was read. "Swept"
means a targeted pattern search of a large library (for file, network, process,
environment, FFI and obfuscation patterns), with every hit read in context. Neither means
the crate's run-time library code was audited; that is what `cargo vet` tracks.

---

## 2026-09-29 — Milestone 0: initial dependency set

### Summary

| Check | Result |
|---|---|
| Direct dependencies | 6 (proptest, criterion, tokio, tokio-tungstenite, rustls, futures-util), all `=`-pinned |
| Locked packages | 118 registry crates; 96 compile for x86_64 Linux (the rest are Windows/WASM/UEFI-only) |
| Upgrade delay (14 days) | First run flagged 11 transitive versions 1–12 days old; all 11 downgraded to the newest compatible version ≥ 14 days old (below). Final re-run: all 118 pass |
| Build-time code review | Every crate version whose code runs at build time on x86_64 Linux (26), reviewed before the first build in two rounds (20 + 13 reviews, overlapping where versions changed): 22 read in full, 4 swept (see the legend). All clean |
| `cargo audit` | Initially **1 vulnerability**: rustls 0.23.44, RUSTSEC-2026-0285 (medium). Fixed by moving to 0.23.45. Now clean (1,277 advisories checked) |
| `cargo deny check` | advisories ok, bans ok (with an explicit ban list: aws-lc-rs/sys, openssl-sys, native-tls, libsecp256k1), licenses ok, sources ok (crates.io only). Warnings: duplicate versions of `getrandom` and `windows-sys` |
| `cargo vet` | Passes: **29 fully audited** by imported audit sets (Google, Mozilla, Bytecode Alliance, ISRG, Zcash), **89 exempted** (no trusted third-party audit of that exact version exists). Of the 29, 23 are compiled on Linux; 2 of the 29 (wasip2, wit-bindgen, not compiled here) are covered by publisher-trust entries rather than code audits. Embark's audit set was imported at first but contains no audits, so it was dropped |

### Lesson: an advisory fix beats the upgrade delay

rustls 0.23.45 was published 2026-09-14 and is the fix for RUSTSEC-2026-0285. On
2026-09-29 it was 14 whole days old when first checked, so it passed the rule (the check
counts whole days). 0.23.44
was nevertheless picked, to stay clear of the edge, and `cargo audit` then flagged it. The
project moved to 0.23.45. Its `build.rs` is byte-identical to the reviewed 0.23.44's.

The policy now says so explicitly (D-006): a release that fixes a published advisory may be
taken early, once its build-time code has been read. `check-ages.sh` accepts such
versions through `ALLOW=name@version` and reports them as allowed rather than silently
passing them.

### Downgrades for the upgrade delay

| Crate | Resolver picked (age) | Pinned in lockfile (age) | Pulled in by |
|---|---|---|---|
| cc | 1.5.1 (4 d) | 1.4.6 (15 d) | build-dependency of ring, alloca |
| find-msvc-tools | 0.1.14 (4 d) | 0.1.12 (24 d) | cc |
| cfg-if | 1.0.5 (12 d) | 1.0.4 (348 d) | ring, getrandom, sha1, ... |
| rand | 0.10.3 (9 d) | 0.10.2 (88 d) | tungstenite |
| syn | 3.0.6 (12 d) | 3.0.5 (24 d) | serde_derive, thiserror-impl |
| thiserror, thiserror-impl | 2.0.21 (5 d) | 2.0.20 (51 d) | tungstenite |
| tokio-rustls | 0.26.6 (1 d) | 0.26.5 (24 d) | tokio-tungstenite |
| unicode-ident | 1.0.26 (12 d) | 1.0.24 (224 d) | proc-macro2 |
| zerocopy, zerocopy-derive | 0.8.59 (3 d) | 0.8.57 (20 d) | ppv-lite86 (rand) |

### Build-time code review, round 1 (20 crate versions)

Crates with a build script or proc-macro, read by eight parallel Claude Code subagents from
the unmodified crates.io sources before anything was compiled. "Commit" is the upstream git
commit recorded in the crate's `.cargo_vcs_info.json`.

| Crate | Verdict | Commit | What runs at build time |
|---|---|---|---|
| ring 0.17.14 | clean | 2723abbca9e8 (packaged from a dirty tree) | Compiles the crate's shipped C and pregenerated assembly with `cc` into `OUT_DIR`. The Perl/nasm regeneration path runs only from a git checkout, never from crates.io. Prebuilt COFF objects are linked only on Windows. |
| rustls 0.23.44 → 0.23.45 | clean | 64ad386785c7 → 2976d90fd1c2 | 13-line `build.rs`; empty unless the `read_buf` feature is on (it isn't). Identical in both versions. |
| httparse 1.10.1 | clean | 9f29e79f9832 | `rustc --version` probe; SIMD `cfg` flags. |
| zmij 1.0.23 | clean | 7b7cc48b5802 | `rustc --version` probe; `cfg` flags. (dtolnay's float formatter, used by serde_json.) |
| serde 1.0.229 | clean | 7fc3b4c30c94 | `rustc --version` probe; writes a fixed re-export module to `OUT_DIR`. |
| serde_core 1.0.229 | clean | 7fc3b4c30c94 | Same as serde. |
| serde_json 1.0.151 | clean | de8500740cdc | Reads target arch/pointer width; one `cfg` flag. No processes, no file I/O. |
| serde_derive 1.0.229 | clean | 7fc3b4c30c94 | Proc-macro: parses and emits token streams only. |
| thiserror 2.0.21 | clean | b1827ee06f81 | Writes a fixed module to `OUT_DIR`; compiler probe. (Superseded by 2.0.20, round 2.) |
| thiserror-impl 2.0.21 | clean | b1827ee06f81 | Proc-macro: `#[derive(Error)]` token generation only. (Superseded by 2.0.20.) |
| zerocopy 0.8.59 | clean | 41f5b37afe70 | Reads its own `Cargo.toml` for version metadata; `rustc --version`; `cfg` flags. (Superseded by 0.8.57.) |
| zerocopy-derive 0.8.59 | clean | 41f5b37afe70 | Proc-macro: token generation only. (Superseded by 0.8.57.) |
| proc-macro2 1.0.107 | clean | ed8a5497669c | `rustc --version`; compiles its own probe files as metadata only into `OUT_DIR`, then deletes them. |
| quote 1.0.47 | clean | 723dcb47d3f0 | `rustc --version`; one `cfg` flag. |
| num-traits 0.2.19 | clean | 7ec3d41d39b2 | Uses `autocfg` to probe one compiler feature (reviewed in round 2). |
| libc 0.2.189 | clean | ef0906e20828 | `rustc --version`; also runs `emcc -dumpversion` if Emscripten is on `PATH`, to detect that toolchain. Not present in our image. |
| getrandom 0.2.17 | clean | b625985d8526 | Nothing: no build script. |
| getrandom 0.3.4 | clean | 38e4ad38309a | `cfg` flags; probes rustc only on Windows targets. |
| getrandom 0.4.3 | clean | 5e7cd5733536 | One sanitizer `cfg` flag. |
| alloca 0.4.0 | clean | 1a5ff4220155 | Compiles the crate's own `alloca.c` with `cc`. |

### Build-time code review, round 2 (13 crate versions)

Round 1 covered crates that *have* a build script or proc-macro. Round 2 closed the gap:
the four versions changed by the downgrades, plus the libraries that run *inside* build
scripts and proc-macros (build-dependencies such as `cc`, and proc-macro dependencies such
as `syn`). `tools/supply-chain/list-build-time-code.sh` now lists this full set: 26
crate versions for x86_64 Linux, all reviewed across the two rounds. Five reviewers, run in
parallel (Claude Code subagents) before the first build.

| Crate | Verdict | Commit | What runs at build time |
|---|---|---|---|
| thiserror 2.0.20 | clean | b1d5db5e0392 | Writes a fixed re-export module to `OUT_DIR`; compiles its own probe file as metadata only with `$RUSTC`, then deletes it; `rustc --version`. |
| thiserror-impl 2.0.20 | clean | b1d5db5e0392 | Proc-macro: `#[derive(Error)]` token generation only; no file, network, process or environment access beyond `CARGO_PKG_VERSION_PATCH`. |
| zerocopy 0.8.57 | clean | 0c90b11ac708 | Reads its own `Cargo.toml` for version gates; `rustc --version`; `cfg` flags. Writes nothing. |
| zerocopy-derive 0.8.57 | clean | 0c90b11ac708 | Proc-macro: token generation only. |
| cc 1.4.6 | clean | 4c76c59ee2cc | Build-dependency (ring, alloca): spawns only the configured C compiler and archiver, writes objects and archives under `OUT_DIR`, reads the standard compiler variables. |
| shlex 2.0.1 | clean | e82b1411beb7 | Shell-word splitting for `cc`; pure string handling. |
| find-msvc-tools 0.1.12 | clean | 171f8f646982 | Locates MSVC for `cc`; its Windows-only paths (registry, COM) are never reached on Linux. No network. |
| syn 2.0.119 | clean (swept) | 3295f9e98417 | Rust parser running inside proc-macros. Targeted sweep of all `src/`: no filesystem, network, process, environment, FFI or obfuscation; hits only in doc comments. |
| syn 3.0.5 | clean (swept) | e0ad92d68b58 | Same sweep and result as syn 2. |
| unicode-ident 1.0.24 | clean | 5b54a632702b | Static lookup tables and two lookup functions only. |
| autocfg 1.5.1 | clean | 2799b09c24e6 | Build-dependency (num-traits): spawns only `rustc` (and the standard wrappers) to probe features, writing only under `OUT_DIR`. |
| proc-macro2 1.0.107 (library) | clean (swept) | ed8a5497669c | Library code runs inside every proc-macro: no filesystem, network, process or environment access. |
| quote 1.0.47 (library) | clean (swept) | 723dcb47d3f0 | Same: no filesystem, network, process or environment access, no `unsafe`. |

### Final state, before the first build

- `tools/supply-chain/check-ages.sh`: exit 0. Every one of the 118 locked versions is at
  least 14 days old; the youngest is rustls 0.23.45 (14 whole days at that check).
- `cargo audit`: no vulnerabilities (124 crates scanned, 1,277 advisories).
- `cargo deny check`: advisories, bans, licenses and sources all ok.
- `cargo vet --locked`: passes, 29 fully audited, 89 exempted.
- Build-time code: all 26 crate versions clean (22 read in full, 4 swept).

Only then was the first `cargo build` run, offline, in the read-only container.

### Not covered (honest gaps)

- **The reviews were done by AI subagents**, as stated at the top, not independently
  re-read by a person.
- **The supply-chain tools' own dependency trees.** cargo-audit, cargo-deny and cargo-vet
  are compiled from several hundred crates during `docker build`, with network, and none of
  their build-time code was reviewed. They then run in the `check` and `vet` containers,
  which have network but no host secrets, and mount the cargo cache read-only, so they
  can't change the sources that `dev` and `recorder-build` compile. `check` can write only
  its advisory databases, and `vet` only `supply-chain/` (after which `./dev` refuses to
  continue if anything but the three expected files is there). They are trusted as
  well-known projects of RustSec, Embark and Mozilla. Stronger options: checksum-pinned
  release binaries, or building them offline from pre-fetched sources.

- **Run-time library code** of the 118 crates is not reviewed by us. `cargo vet` records
  that 29 exact versions carry third-party audits; the other 89 are exempted, i.e.
  trusted on reputation, download counts and the checks above only.
- **Provenance** was checked by reading each crate's recorded upstream commit, not by
  diffing the published crate against that commit. ring 0.17.14 was packaged from a
  working tree with uncommitted changes (`"dirty": true`), which is its maintainer's
  usual release process; its build script was read in full.
- **The Docker base image** (`rust:1.98.1-slim-trixie`, pinned by digest) and Debian
  packages are trusted as distributed.

## 2026-09-29 — Milestone 3: `k256` for signatures

The owner chose RustCrypto's pure-Rust `k256` (secp256k1 ECDSA) for signing (load
generator) and verifying (gateways). Not the banned `libsecp256k1` crate (an abandoned
lookalike, blocked in `deny.toml`), and not Bitcoin Core's C library through the
`secp256k1` crate, which INFO.md keeps only as a fallback if M3 measurements need it.

### Summary

| Check | Result |
|---|---|
| Direct dependency | `k256 =0.14.0`, default features off, only `ecdsa` (in `gateway` and `loadgen`) |
| New locked crates | 19 (143 in the lockfile now); the rest of its tree was already locked |
| Upgrade delay (14 days) | All pass (`check-ages.sh` exit 0). Youngest: `der` 0.8.2 (24 days), `wnaf` 0.14.1 (26 days) |
| Build-time code | **None added.** No new crate has a build script, a proc-macro, C or assembly files, or `links`; the build-time list is the same 26 crates reviewed in M0 |
| Run-time code | All 19 swept before the first build (below), because they sit on every order's path |
| `cargo audit` | No vulnerabilities (143 crates, 1,277 advisories) |
| `cargo deny check` | advisories, bans, licenses, sources ok |
| `cargo vet` | Passes: 29 fully audited, **108 exempted** (the 19 new crates, which no imported audit set covers yet, plus `autocfg` raised from safe-to-run to safe-to-deploy; it was read in M0) |

### Run-time sweep (19 crates, three AI subagents, sources read from the cargo cache
inside the no-network container, nothing compiled)

Each was checked for: a build script, `links` or proc-macro; its recorded upstream commit
(`.cargo_vcs_info.json`, not dirty); file, network, process, environment, clock or thread
access; FFI and assembly; `include_bytes!`; unexpected data blobs or obfuscation; and what
every `unsafe` block is for. `k256`'s curve constants (p, n, G, b = 7, the GLV constants)
were compared with the known secp256k1 values, and its ECDSA verification path was read:
it rejects high-S signatures and zero scalars, and signing is deterministic (RFC 6979).

| Crate | Age (days) | Verdict | Upstream @ commit |
|---|---|---|---|
| k256 0.14.0 | 83 | clean (swept) | RustCrypto/elliptic-curves @ `788867b46f04` |
| ecdsa 0.17.0 | 89 | clean (swept) | RustCrypto/signatures @ `a51becc840c9` |
| elliptic-curve 0.14.1 | 92 | clean (swept) | RustCrypto/traits @ `049d1fcc3ee4` |
| primeorder 0.14.0 | 89 | clean (swept) | RustCrypto/elliptic-curves @ `ef3a3b10c0ff` |
| primefield 0.14.0 | 97 | clean (swept) | RustCrypto/elliptic-curves @ `777946481334` |
| wnaf 0.14.1 | 26 | clean (read) | RustCrypto/elliptic-curves @ `f722e37cee96` |
| sha2 0.11.0 | 188 | clean (read) | RustCrypto/hashes @ `ffe093984c00` |
| hmac 0.13.0 | 184 | clean (read) | RustCrypto/MACs @ `0236c8eb5009` |
| rfc6979 0.6.0 | 91 | clean (read) | RustCrypto/signatures @ `13c89f859a8a` |
| signature 3.0.0 | 150 | clean (read) | RustCrypto/traits @ `9488e7ea6676` |
| ff 0.14.0 | 122 | clean (read) | zkcrypto/ff @ `8cf62cd9c7a3` |
| group 0.14.0 | 120 | clean (read) | zkcrypto/group @ `f9a84a7587f4` |
| crypto-bigint 0.7.5 | 99 | clean (swept) | RustCrypto/crypto-bigint @ `2b54d248cce0` |
| cmov 0.5.4 | 124 | clean (read) | RustCrypto/utils @ `5c7e4f9bb31a` |
| ctutils 0.4.2 | 180 | clean (read) | RustCrypto/utils @ `53f7fc3fa806` |
| cpubits 0.1.1 | 154 | clean (read) | RustCrypto/utils @ `bff92a8c3362` |
| der 0.8.2 | 24 | clean (swept) | RustCrypto/formats @ `7637997c168f` |
| sec1 0.8.1 | 182 | clean (read) | RustCrypto/formats @ `1d6ed2eddd55` |
| base16ct 1.0.0 | 269 | clean (read) | RustCrypto/formats @ `071bfe69e201` |

`unsafe` found, all of the expected kind: SHA hardware instructions behind CPU-feature
checks (`sha2`), constant-time conditional moves in inline assembly (`cmov`, its stated
purpose), same-layout newtype casts (`elliptic-curve`, `crypto-bigint`, `der`) and UTF-8
conversions of hex output (`base16ct`). `k256`, `ecdsa`, `primeorder`, `primefield`,
`wnaf`, `hmac`, `rfc6979`, `signature`, `ff`, `group`, `ctutils`, `sec1` and `cpubits`
contain no `unsafe` at all.

**Found by the sweep, fixed before building:** the first choice of features included
`precomputed-tables`. In 0.14.0 that needs `primeorder`'s `std` (the build would have
stopped at a `compile_error!`), and it speeds up only signing, not verification, so it
was dropped. Turning on `k256`'s own `std` instead was rejected: it also enables OS
randomness, which we don't use.

**First build:** after all of the above, offline in the `dev` container; the workspace's
166 tests still pass.

**Not covered:** same as M0. The sweep reads for tampering, it is not a cryptographic
audit of the curve arithmetic, and the published crates were not diffed against the
upstream commits.

## 2026-09-30 — Milestone 3: the optional libsecp256k1 verifier

The owner approved Bitcoin Core's libsecp256k1, through rust-bitcoin's `secp256k1` crate,
as an **opt-in** second signature verifier, to compare with `k256` on the rented box
(D-032; `docs/PIPELINE.md` 5.7). It is an optional dependency of `gateway`, behind its
`c-secp256k1` feature, off by default: the default build never compiles it, and its build
script runs only in a build with that feature. (Not the banned crate named
`libsecp256k1`.)

**Scope, set by the owner.** Two checks: the build script, as D-006 requires for all code
that runs at build time, and an upstream-match diff of the vendored C. The owner decided
that the Rust wrapper itself need not be swept.

### Summary

| Check | Result |
|---|---|
| Direct dependency | `secp256k1 =0.33.1`, default features off, only `std` (per-thread contexts; no `rand`); optional in `gateway` (feature `c-secp256k1`), which the `bench` feature of the same name turns on. Since D-033 the gateway's feature also turns on `recovery` (below) |
| New locked crates | 2: `secp256k1` 0.33.1 and `secp256k1-sys` 0.14.1 (145 in the lockfile now). The one build-dependency, `cc`, resolves to 1.4.6, already locked and read in M0 (round 2) |
| Upgrade delay (14 days) | Both published 2026-08-29: 31 days old |
| Build-time code | `secp256k1-sys`'s `build.rs` read in full: clean (below). `secp256k1` has `build = false` and no proc-macro |
| Vendored C | Upstream libsecp256k1 v0.8.0 exactly, once the crate's 6 shipped patches and its symbol renaming are applied (below) |
| Run-time code | Not swept (the owner's choice): the wrapper's Rust, its `unsafe` FFI calls included. The C is shown to be upstream's, not audited by us |
| `cargo audit` | No vulnerabilities (145 crates scanned) |
| `cargo deny check` | Passes |
| `cargo vet` | Passes: 110 exempted, including the 2 new crates |

### Build script: `secp256k1-sys` 0.14.1 (clean, read in full, 67 lines)

Read from the unmodified crate in the cargo cache; nothing was compiled or run for the
review.

- **Manifest.** `links = "rustsecp256k1_v0_14"`; one build-dependency, `cc`; features
  `alloc`, `std` (default), `lowmemory` and `recovery`, of which our build turns on only
  `std` (which implies `alloc`; it was `alloc` alone at review time, and the build script
  doesn't read either). Since D-033 it turns on `recovery` too (below).
- **What it compiles.** One `cc` build of four files of the vendored tree into
  `libsecp256k1.a` in `OUT_DIR`: `contrib/lax_der_parsing.c`, `src/precomputed_ecmult_gen.c`,
  `src/precomputed_ecmult.c` and `src/secp256k1.c`. A fifth file, for wasm32 only, is not
  built on x86_64.
- **Defines.** The ECDH, Schnorr, extrakeys, ElligatorSwift and MuSig modules on; `printf`
  defined to nothing; external default callbacks (the Rust side supplies them); the
  precomputed tables at upstream's default sizes (`ECMULT_WINDOW_SIZE=15`, `COMB_BLOCKS=43`,
  `COMB_TEETH=6`). `lowmemory` (smaller tables) is off. `recovery` (one more module) was
  off at review time; it is on since D-033 (below).
  Two warning flags, each only if the compiler takes it.
- **Environment, processes, files, network.** It reads only `CARGO_CFG_TARGET_ARCH` and its
  features; `cc` reads the standard compiler variables. It starts no program itself;
  through `cc`, only the C compiler (flag probes and the four files) and `ar`. It reads only
  the crate's own sources, writes only under `OUT_DIR`, and has no network access.
- **Fallback.** If the first compile fails, it retries with `wasm/wasm-sysroot` on the
  include path: header stubs (empty `stdio.h` and `stdlib.h`, four prototypes in
  `string.h`), no code.
- **Not used by the build:** `vendor-libsecp.sh` (the maintainers' vendoring script) and
  `depend/check_uint128_t.c`. Neither was run for this review.

### The vendored C against upstream (no difference in C)

- **Which upstream.** The crate's `depend/secp256k1-HEAD-revision.txt` names libsecp256k1
  commit `6e2c8bc4ecdc`. Upstream's tag v0.8.0 points exactly at that commit (GitHub's
  tags API), the source tarball's embedded commit id matches, and upstream's changelog
  dates 0.8.0 on 2026-08-03.
- **Where the crate was packaged from.** `.cargo_vcs_info.json` names rust-secp256k1
  commit `8820a6e310be` ("Release tracking PR: v0.33.1", 2026-08-29), which is on
  git.rust-bitcoin.org, the repository the crate's metadata names; the GitHub mirror lags
  and doesn't have it. The revision file at that commit names the same libsecp256k1
  commit.
- **Reproduced without their script.** Upstream's tree at the tag, with each of the 6
  shipped `depend/*.patch` files applied with `patch` (all clean: no offset, no fuzz) and
  the symbol renaming done with `sed` as the vendoring script does it (`secp256k1_` to
  `rustsecp256k1_v0_14_` outside `#include` lines, then the lax DER parser's name), was
  compared with the crate's `depend/secp256k1` using `diff -r`. One line of output:
  upstream has `autotools-aux/`, the crate doesn't. It holds one autoconf macro file, which
  upstream's own `.gitignore` excludes (so the packaging commit's tree lacks it too); it is
  not C, and the build doesn't use it. Every other file is byte-identical, hidden files
  included, and the same 8 files are executable in both trees.
- **The other direction.** Undoing the renaming in a copy of the crate's tree leaves
  exactly the 6 patched files different from upstream, each by exactly its shipped patch
  (for `secp256k1.h`, the patch file's line-number hints are off by one, which `patch`
  ignores; the removed lines are the same).
- **The patches** only delete code, or add one declaration:

  | Patch | What it does |
  |---|---|
  | `secp256k1.c.patch` | Deletes the context and scratch-space functions that call `malloc` and `free`; the Rust side creates contexts on the preallocated API instead |
  | `secp256k1.h.patch` | Deletes those functions' declarations, and those of two exported nonce functions (still defined; Rust declares them itself) |
  | `scratch.h.patch`, `scratch_impl.h.patch` | Delete the `malloc`/`free`-based scratch create and destroy |
  | `util.h.patch` | Deletes two `printf` debug helpers; `checked_malloc` returns NULL instead of calling `malloc` (no compiled file calls it) |
  | `lax_der_parsing.c.patch` | Adds an `extern` declaration of an existing library function (a MinGW link fix) |

- **Symbols.** Every exported C symbol has the prefix `rustsecp256k1_v0_14_` (10,294
  occurrences, no other prefix), and `links = "rustsecp256k1_v0_14"`, so this copy can't
  collide with another libsecp256k1 in the same binary.

**First build:** after the above, offline in the `dev` container, with
`--features c-secp256k1`. The default build is unchanged: it doesn't compile the new
crates.

### Not covered (honest gaps)

- **The wrapper's run-time code** (`secp256k1` 0.33.1, the Rust around the C, its `unsafe`
  FFI calls included) was not swept: the owner's choice. One behaviour of it was found
  while building, a performance matter rather than a supply-chain one: without its `std`
  feature, each verification builds a fresh context (`docs/PIPELINE.md` 22).
- **The C itself** is shown to be upstream's release, not audited by us: the trust rests
  on Bitcoin Core's library and its maintainers.
- As before: the review was done by AI subagents, not re-read line by line by me.

## 2026-09-30 — Milestone 3: libsecp256k1's recovery module, for the EIP-712 scheme

The opt-in EIP-712 scheme (D-033; `docs/PIPELINE.md` 5.8) needs the signer's public key
recovered from each signature. `k256` already does it (`VerifyingKey::recover_from_prehash`,
in the locked `ecdsa` 0.17.0). For libsecp256k1, the gateway's `c-secp256k1` feature now
also turns on the `secp256k1` crate's `recovery` feature, which turns on `secp256k1-sys`'s:

```toml
c-secp256k1 = ["dep:secp256k1", "secp256k1/recovery"]
```

keccak-256 and the MessagePack writer the scheme also needs are our own code (the owner's
choice, D-033), so they add nothing here. The default build is unchanged: it still compiles
neither crate.

### Summary

| Check | Result |
|---|---|
| New crates | **None.** Both crates were already locked, and a crate's features are not recorded in the lockfile: `Cargo.lock` is byte for byte unchanged (`git diff` empty; SHA-256 `9fd53dd4…c865` before and after) |
| Build script | The same `build.rs`, already read in full (above). With `recovery` on, it adds one define, `ENABLE_MODULE_RECOVERY=1` (lines 45 and 46). It compiles the same four files, runs the same programs, and reads and writes the same places |
| C compiled | Of the four files, only `src/secp256k1.c` reads the define: it then includes `src/modules/recovery/main_impl.h` (159 lines), which includes the module's public header, `include/secp256k1_recovery.h` (123 lines). They add parsing, writing and converting a recoverable signature, recoverable signing, and `ecdsa_recover`, which finds `R` from `r` and then uses verification's own multiplication and precomputed tables: no new table, no allocation. The module's other files (its tests and benchmarks) are not built |
| Upstream match | The module was already in the vendored tree that the review compared with libsecp256k1 v0.8.0 (above): byte-identical once renamed, and none of the six shipped patches touches it. Its symbols carry the same `rustsecp256k1_v0_14_` prefix |
| Rust compiled | `secp256k1-sys`'s `src/recovery.rs` (241 lines: the types and the `extern "C"` declarations of the five functions) and `secp256k1`'s `src/ecdsa/recovery.rs` (481 lines: `RecoverableSignature`, `RecoveryId`, `recover_ecdsa`), with their `unsafe` FFI calls. Not swept: the wrapper's run-time code is out of this review's scope by the owner's decision (D-032) |
| `cargo audit`, `cargo deny`, `cargo vet` | Not run again: they check the lockfile and its crates, which didn't change |

**How it was checked.** Reading, in the container's read-only cargo cache: the build
script (the `#[cfg(feature = "recovery")]` define is its only use of the feature),
`src/secp256k1.c` (the one `#ifdef ENABLE_MODULE_RECOVERY`, at its end), the module's two
compiled files, the six patches (none names the module) and the two Rust files above.
Nothing was compiled or run for the review. Then the feature build, offline in the `dev`
container: the gateway's tests recover all 12 of Polymarket's golden signatures with both
libraries, and cross-check both libraries' recovery over 240 cases (`docs/PIPELINE.md`
18.1).

### Not covered (honest gaps)

- As for the verifier: the wrapper's Rust around the recovery module, its `unsafe` FFI
  calls included, was not swept (the owner's choice, D-032), and the C is shown to be
  upstream's, not audited by us.
- The review of these files was done by Claude Code, not re-read line by line by me.

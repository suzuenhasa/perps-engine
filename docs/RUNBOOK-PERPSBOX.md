# Runbook: the M3 headline session on PERPSBOX

How to run the M3 benchmark session (`docs/PIPELINE.md` 15) on a rented vast.ai box,
from a fresh container to the results on the local machine. Every block is meant to be
pasted as is; the lines to change are marked `# EDIT`.

**Rules.**
- The box holds **no secrets**. It gets the committed tree and nothing else: no `.env`,
  no `data/`, no SSH keys, no tokens, no agent forwarding (`ssh -A`).
- **No Docker.** vast.ai boxes are containers themselves. We build natively with the
  pinned toolchain (`rust-toolchain.toml`: 1.98.1, clippy, rustfmt, profile minimal),
  `--locked` dependencies and `PERPS_IN_CONTAINER=1`, which tells the host guard
  (`.cargo/config.toml`) that this is a build machine (INFO.md 5a, D-001).
- **The probe decides first** (owner, 2026-09-30): if the disk's `fdatasync` p99 is above
  5 ms, the durable rows (load sweep, journal and signed searches, `T` sweep, headline,
  both ablations) need a box with local NVMe. Stop and rent another.
- **Never pass `--allow-generator-limited`** here: it is for local development only.
- **Two passes** (owner, 2026-09-30; PIPELINE.md 5.7, D-032): first the whole session with
  `k256`, from the default build, which compiles no C (sections 3 to 5); then a second,
  shorter pass with Bitcoin Core's libsecp256k1 verifying, from a build with
  `--features c-secp256k1` (section 6). Both on the same box, with the same flow and the
  same gateway count, so the two verifiers compare side by side.
- **Then two EIP-712 passes** (owner, 2026-09-30; PIPELINE.md 5.8, D-033): the headline
  and the signed search again, with every message signed the way Polymarket Perps signs
  it, once with libsecp256k1 and once with `k256` (section 7). Same box, flow and gateway
  count again.
- **Then two D-034 passes** (owner, 2026-09-30; PIPELINE.md 14.12, D-034): the
  Polymarket-shaped flow's three headlines (as it is, with bursts, with the stress shock)
  and its searches, once with `k256` in the first session and once with libsecp256k1 in
  the second, each next to that session's M3 numbers (section 7b).

Budget (15.11, at the minimum, no reruns): about 3 hours of runs for the first pass, 35
minutes for the second, about 1.5 hours for the two EIP-712 passes and about 1 hour 40
minutes for the two D-034 passes, plus 15 to 20 minutes of setup and builds.

## 0. Local, before renting

```sh
cd ~/perpstest
./dev cargo test --workspace --locked --offline      # green before spending box time
./dev cargo test -p gateway -p bench --features c-secp256k1 --locked --offline   # the second pass's build
python3 tools/calibrate/calibrate.py generate --check # the D-034 profile table is the tool's output (standard library)
git status --short                                    # must print nothing: commit first
git rev-parse --short HEAD                            # note it; every result names it
git bundle create /tmp/perpstest.bundle main          # committed history only
```

Rent: x86_64, one CPU package (or NUMA balancing off, 1), 14 or more physical cores, 60 GB
RAM or more, 50 GB disk, **local NVMe** (the offer's disk line), Ubuntu or Debian image. Add
the box to `~/.ssh/config` as `PERPSBOX` (without `ForwardAgent`).

## 1. Get the code onto the box

```sh
scp /tmp/perpstest.bundle PERPSBOX:
ssh PERPSBOX
```

On the box, first check that a busy core runs at full speed (10 seconds). Some hosts keep
every core at its lowest clock (the `acpi-cpufreq` driver with the `powersave` governor),
which makes everything two or more times slower and fails the probe's clock check (4). The
container can't change it, so rent another box. (2026-09-30: an EPYC 9684X box ran every
core at 1.5 GHz of its 3.7 this way, with boost off.)

```sh
cd /sys/devices/system/cpu/cpufreq/policy1 && cat scaling_driver scaling_governor cpuinfo_max_freq
taskset -c 1 timeout 4 sh -c 'while :; do :; done' & sleep 3; cat scaling_cur_freq; wait; cd
```

`timeout` ends the busy loop by itself: a loop left running (say, after a dropped ssh
session) would share CPU 1 with the pipeline's gate thread and quietly add milliseconds to
every durable ack (it happened on 2026-09-30). The busy figure (kHz) should be near
`cpuinfo_max_freq`, and at least the chip's base clock.
Trust the busy figure, not the governor's name: `powersave` under the `intel_pstate` or
`amd-pstate` drivers still speeds up under load. If there's no `cpufreq` directory, go on;
the probe's clock check (4) still catches a slow box.

Second, check NUMA balancing. On a machine with more than one NUMA node (two sockets, or
one EPYC split into nodes by its NPS setting) the kernel may keep unmapping a program's
pages to see which node touches them. Each touch after that is a minor page fault, so
every run gets flagged for faults in the window (8), and the smoke test (3) fails. The
container can't opt out (`numactl` and `set_mempolicy` need `CAP_SYS_NICE`), so rent
another box if both lines below are over 1. (2026-09-30: a two-socket EPYC 7V13 box had it
on.)

```sh
lscpu | grep 'NUMA node(s)'; cat /proc/sys/kernel/numa_balancing   # 1 node, or balancing 0
```

Then:

```sh
command -v git >/dev/null || (apt-get update && apt-get install -y --no-install-recommends git)
git clone -b main ~/perpstest.bundle ~/perpstest
cd ~/perpstest && git status --short && git rev-parse --short HEAD   # clean; same hash as local
```

## 2. Toolchain (about 2 minutes)

```sh
cd ~
curl --proto '=https' --tlsv1.2 -sSfO https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init
curl --proto '=https' --tlsv1.2 -sSfO https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init.sha256
echo "$(cut -d' ' -f1 rustup-init.sha256)  rustup-init" | sha256sum -c -
chmod +x rustup-init && ./rustup-init -y --profile minimal --default-toolchain none
. "$HOME/.cargo/env"
rustup toolchain install 1.98.1 --profile minimal -c clippy -c rustfmt
command -v cc >/dev/null || (apt-get update && apt-get install -y --no-install-recommends build-essential)
```

Rust links through `cc`, hence `build-essential` when the image lacks it. The first pass's
`e2e` binary has no C code; the smoke test's dev-dependencies do (`criterion` → `alloca`,
compiled with the same `cc`), and the recorder's `ring` isn't built here. The second pass's
binary also compiles libsecp256k1's C with it (`secp256k1-sys`, section 6).

## 3. Build (a few minutes)

```sh
cd ~/perpstest
export PERPS_IN_CONTAINER=1                           # the host guard's "this is a build machine"
export PERPS_GIT_COMMIT=$(git rev-parse --short HEAD) # else every result says commit "unknown"
rustc --version                                       # rustc 1.98.1 (from rust-toolchain.toml)
cargo fetch --locked                                  # downloads only (both passes' crates), compiles nothing
cargo build --release --locked --offline -p bench --bin e2e
cargo test --release --locked --offline -p bench --test smoke   # the whole pipeline in seconds
```

## 4. Probe first (about 2 minutes)

The session directory must be on the disk you mean to measure; the probe writes its
`fdatasync` test there.

```sh
cd ~/perpstest
export S=$HOME/runs/m3-$(date +%F)                    # the session directory
E=./target/release/e2e
$E probe --dir $S
grep -E '^(machine|clock|fs|fsync)\.' $S/probe/summary.txt
p99=$(sed -n 's/^fsync.fdatasync.p99 = //p' $S/probe/summary.txt)
[ "$p99" -le 5000000 ] && echo "disk OK: flush p99 $p99 ns" || echo "STOP: flush p99 $p99 ns > 5 ms; durable rows need local NVMe"
```

Go on only if all of these hold:
- the disk line says `disk OK`, `fs.verdict = Ok`, and `fsync.verdict` is "not suspicious"
  (a p50 under about 20 µs means a flush that may do nothing);
- `clock.source = tsc` and `clock.counts_for_headline = true`;
- `machine.packages = 1`, or more if section 1's NUMA check passed (the layout stays on one
  package, 2.6, and `machine.physical_cores` counts that package only), and `machine.quota`
  leaves room for the layout (below).

**The gateway count `G`** (2.6): `machine.physical_cores − 4` (the sender, the core, the
sequencer, and the gate with the journal writer take one physical core each), and at most
`floor(machine.quota) − 7` (signed mode spins `5 + G` threads and the harness refuses more
than `floor(quota) − 2`). The 14-core PERPSBOX of 2026-09-28 gives `G = 10`. The probe's
`scaling.*.per_second` lines say how many verifications a second `G` cores give; below
about 100,000 the headline can't pass, which section 17 then explains.

## 5. The session (about 3 hours)

Every command is **resumable** (a finished run is read back, not run again), so after a
failure, fix the cause and start the script again. Pass the **same options to every
command** (a finished run is reused only with the same settings; review finding
F4-resume-keyed-by-name-only), and never `--window` to a search.

```sh
cat > ~/m3-session.sh <<'EOF'
#!/bin/sh
# The M3 session in order (docs/RUNBOOK-PERPSBOX.md 5). Resumable: run it again after a failure.
set -eu
cd ~/perpstest
E=./target/release/e2e
O="--dir ${S:?} --gateways ${G:?}"
$E sweep --kind load $O                 # ~16 min; its pre-verified 20k/s point sets the durable limit
$E sweep --kind headline $O             # ~15 min; 3 timing runs, the capture run (replay, audit), a second seed
$E search --name core-path $O           # the three searches, ~25 min together
$E search --name journal $O
$E search --name signed $O
$E sweep --kind commit-interval $O      # ~10 min
R=$(sed -n 's/^search.result = //p' "$S/search-core-path/search.txt")
[ "$R" = none ] || $E sweep --kind stamps --rate $((2 * R)) $O   # a few minutes
$E ablate verify-on-core --search $O    # ~35 min
$E ablate fsync-per-order --search $O   # ~65 min
$E report --dir "$S" > "$S/report-final.md"
echo "m3 session: done"
EOF
export G=10                                           # EDIT: from section 4
nohup sh ~/m3-session.sh > ~/m3-session.log 2>&1 &
tail -f ~/m3-session.log                              # Ctrl-C stops the tail, not the session
```

The headline runs right after the load sweep, so the main number lands first if the box
dies. Each command prints its plan and time estimate before it starts. `$S/report.md` is
rebuilt after every command; `$E report --dir $S` prints it at any time. After a new login,
`export` the same `S` (not a new date) and `G` before starting the script again.

## 6. Second pass: libsecp256k1 (about 35 minutes)

Only after section 5's script has printed `m3 session: done`. The binary is rebuilt at the
same path with the feature (it could still verify with `k256`, but the first pass is done
by then). The second session starts with a copy of the first one's pre-verified 20k/s
runs, so both passes' signed search and headline are judged against the same durable
limit (15.7): pre-verified runs verify nothing, their summaries say `run.verifier = none`,
and the second session reads them back instead of running them again.

```sh
cd ~/perpstest
export PERPS_IN_CONTAINER=1 PERPS_GIT_COMMIT=$(git rev-parse --short HEAD)
cargo build --release --locked --offline -p bench --features c-secp256k1 --bin e2e   # compiles libsecp256k1's C
cargo test --release --locked --offline -p gateway --features c-secp256k1           # the cross-check of 5.7, on this CPU
cargo test --release --locked --offline -p bench --features c-secp256k1 --test smoke
export S2=$S-libsecp256k1                             # the second session, next to the first
mkdir -p $S2/sweep-load && cp -r $S/sweep-load/preverified-20k-r* $S2/sweep-load/
cat > ~/m3-libsecp.sh <<'END'
#!/bin/sh
# The libsecp256k1 pass (docs/RUNBOOK-PERPSBOX.md 6). Resumable, like the first.
set -eu
cd ~/perpstest
E=./target/release/e2e
$E probe --dir "${S2:?}"                # measures both verifiers: t_v and the scaling curves
O="--dir $S2 --gateways ${G:?} --verifier libsecp256k1"
$E search --name signed $O              # ~10 min; the durable limit comes from the copied runs
$E sweep --kind headline $O             # ~15 min; the audit verifies with libsecp256k1 too
$E report --dir "$S2" > "$S2/report-final.md"
echo "m3 libsecp256k1 pass: done"
END
nohup sh ~/m3-libsecp.sh > ~/m3-libsecp.log 2>&1 &
tail -f ~/m3-libsecp.log                              # Ctrl-C stops the tail, not the pass
```

Check before going on: `grep -E 'verify_ns|^scaling' $S2/probe/summary.txt` lists both
verifiers; every signed run's `summary.txt` in `$S2` says `run.verifier = libsecp256k1`
(the copied pre-verified runs say `none`); `$S2/search-signed/search.txt` says
`search.verifier = libsecp256k1`. Section 8's checks apply to these runs too. After a new
login, `export` the same `S`, `S2` and `G` before starting the script again.

## 7. Third and fourth passes: EIP-712 (about 1.5 hours)

Only after section 6's script has printed `m3 libsecp256k1 pass: done`. These passes
answer the owner's question of 2026-09-30 (D-033): does the pipeline reach its signed rate
when every message is signed the way Polymarket Perps signs it? That is EIP-712 over the
keccak-256 of the command's MessagePack form, with a salt and a millisecond timestamp; the
gateways recover each signer and keep a table of recent requests (PIPELINE.md 5.8).

- **One session per verifier:** `$S-eip712` with libsecp256k1, then `$S-eip712-k256`. The
  signed search's run names are fixed, and a session refuses a run of another verifier or
  scheme under a name it already holds. libsecp256k1 goes first because it is the likelier
  to carry 100k/s (locally the gateway's whole signer check takes 47 µs with it and 91 µs
  with `k256`: 7 and 14 gateways by section 17's formula), and its binary is already built.
- Each session starts with a copy of the first pass's pre-verified 20k/s runs, so all four
  passes are judged against the same durable limit (15.7). Pre-verified runs sign nothing:
  their summaries say `run.auth = none`, and the sessions read them back.
- **The headline first** in each, so its number lands first if the box dies.
- **Never `--resume`**, and keep the headline's `--window` at 85 s or less (the default is
  30 s). The harness refuses an EIP-712 run whose timed flow is over 3 minutes, and the
  headline doubles the window: 5 s + 2 × 85 s + 0.2 s = 175.2 s. Messages are signed before
  each run, and the gateways refuse them 5 minutes after their `ts`.
- **Expect signing before most runs.** The harness signs the flow again whenever the kept
  messages would be over 5 minutes old at a run's end (its age, plus the run's sending time,
  plus a 60 s margin), and refuses a run that even the newly signed messages would not last
  through. The log says `workload: the kept EIP-712 messages would be ... s old
  when this run ends: over 5 minutes: signing them again`, then the estimate and the time.
  Each is `messages × t_sign / signing threads` (PIPELINE.md 14.8): for a headline run's
  6.5M messages, roughly 15 to 40 s. That is the scheme's cost to the harness, not to the
  pipeline.

**The libsecp256k1 session** (about 40 minutes). The binary is section 6's; the build line
does nothing unless the tree changed. Smoke first, on this CPU: the gateway's tests check
Polymarket's 12 golden vectors and the two libraries' recovery against each other, and the
smoke test's 9 runs include `signed_eip712_smoke_run` and
`signed_eip712_smoke_run_with_libsecp256k1`.

```sh
cd ~/perpstest
export PERPS_IN_CONTAINER=1 PERPS_GIT_COMMIT=$(git rev-parse --short HEAD)
cargo build --release --locked --offline -p bench --features c-secp256k1 --bin e2e   # section 6's binary
cargo test --release --locked --offline -p gateway --features c-secp256k1           # golden vectors, both libraries
cargo test --release --locked --offline -p bench --features c-secp256k1 --test smoke
export S3=$S-eip712                                   # the third session, next to the first
mkdir -p $S3/sweep-load && cp -r $S/sweep-load/preverified-20k-r* $S3/sweep-load/
cat > ~/m3-eip712.sh <<'END'
#!/bin/sh
# The EIP-712 pass with libsecp256k1 (docs/RUNBOOK-PERPSBOX.md 7). Resumable, like the others.
set -eu
cd ~/perpstest
E=./target/release/e2e
$E probe --dir "${S3:?}"                # both verifiers: t_v, recover_ns, eip712.digest_ns, all four curves
O="--dir $S3 --gateways ${G:?} --verifier libsecp256k1 --auth eip712"
$E sweep --kind headline $O             # ~20 min; the audit rebuilds every digest, verifies with libsecp256k1
$E search --name signed $O              # ~15 min; the durable limit comes from the copied runs
$E report --dir "$S3" > "$S3/report-final.md"
echo "m3 eip712 libsecp256k1 pass: done"
END
nohup sh ~/m3-eip712.sh > ~/m3-eip712.log 2>&1 &
tail -f ~/m3-eip712.log                               # Ctrl-C stops the tail, not the pass
```

**The `k256` session** (about 40 minutes), only after `m3 eip712 libsecp256k1 pass: done`.
Its binary is the default build again, as in the first pass: `k256` only, no C. It is
rebuilt at the same path, so to resume the libsecp256k1 script after this, rebuild with the
feature first.

```sh
cd ~/perpstest
export PERPS_IN_CONTAINER=1 PERPS_GIT_COMMIT=$(git rev-parse --short HEAD)
cargo build --release --locked --offline -p bench --bin e2e                          # the default build: k256, no C
cargo test --release --locked --offline -p bench --test smoke                        # 6 runs, signed_eip712 among them
export S4=$S-eip712-k256                              # the fourth session
mkdir -p $S4/sweep-load && cp -r $S/sweep-load/preverified-20k-r* $S4/sweep-load/
cat > ~/m3-eip712-k256.sh <<'END'
#!/bin/sh
# The EIP-712 pass with k256 (docs/RUNBOOK-PERPSBOX.md 7). Resumable, like the others.
set -eu
cd ~/perpstest
E=./target/release/e2e
$E probe --dir "${S4:?}"                # k256 only in this binary
O="--dir $S4 --gateways ${G:?} --auth eip712"
$E sweep --kind headline $O             # ~20 min
$E search --name signed $O              # ~15 min
$E report --dir "$S4" > "$S4/report-final.md"
echo "m3 eip712 k256 pass: done"
END
nohup sh ~/m3-eip712-k256.sh > ~/m3-eip712-k256.log 2>&1 &
tail -f ~/m3-eip712-k256.log
```

If a smoke test fails on a hot thread's minor fault, it is a finding: stop and look before
spending box time. (The faults seen now and then locally were the kernel's page
migrations; the smoke test now prints faults in a window where the kernel migrated pages,
with the count, and fails only the others: PIPELINE.md 15.4, 22.)

**Check before going on**, for each session (`$S3` here; the same for `$S4`):

```sh
grep -E 'recover_ns|digest_ns|keccak|_recover\.' $S3/probe/summary.txt
grep -rhE '^run\.(auth|verifier) ' $S3/*/*/summary.txt | sort | uniq -c
grep -E '^search\.(auth|verifier) ' $S3/search-signed/search.txt
grep -rE 'gateway\.rejects\.(StaleTimestamp|FutureTimestamp|ReusedRequest|SaltTableFull)' $S3
grep -E '^(audit\.passed|audit\.failures|replay\.verdict) ' $S3/headline/*-capture-r*/summary.txt
```

- The probe lists `<verifier>.recover_ns`, `eip712.digest_ns`, `keccak.64_ns` and the
  recover-scaling curves: both verifiers in `$S3`, `k256` in `$S4`. It must be this
  binary's probe: an older one has no `recover_ns`, and section 17 of the report then has
  nothing to show for the scheme.
- Every signed run says `run.auth = eip712`, with `run.verifier = libsecp256k1` in `$S3`
  or `k256` in `$S4`; the copied pre-verified runs say `run.auth = none` and
  `run.verifier = none`. `search.txt` says `search.auth = eip712` and the same verifier.
- The rejects line prints nothing: no gateway refused a harness message for its time, as a
  replay or for want of room. Such a run is invalid (and run again as `-a2`, `-a3`); look
  at the `workload:` lines of the log. If signing alone takes so long that even freshly
  signed messages wouldn't last the run, the harness refuses the run before starting it,
  with `refused: the EIP-712 messages took N s to sign, ...`, and the script stops: shorten
  the headline's `--window`, then start the script again.
- The headline's capture run says `audit.passed = true`, `audit.failures = 0` and
  `replay.verdict = identical`.
- Section 8's checks apply to these runs too. After a new login, `export` the same `S`,
  `S3` (or `S4`) and `G`, and make sure the binary at `./target/release/e2e` is the one
  its script needs (the feature build for `$S3`, the default one for `$S4`), before
  starting a script again.

## 7b. Fifth and sixth passes: the Polymarket-shaped flow (about 1 hour 40 minutes)

Only after section 7's `k256` script has printed `m3 eip712 k256 pass: done` (any time
after section 5's `m3 session: done` works too: each block below builds the binary it
needs, and makes `$S2` if section 6 didn't). These passes answer the owner's question of
2026-09-30 (D-034): what does Polymarket Perps' own traffic shape cost the pipeline, at
about 100 times its volume, and does one busy account, bursty arrivals or a correlated
multi-market shock break the signed headline? The flow (`--flow polymarket`, PIPELINE.md
14.12) is Polymarket Perps' 88 real markets, traded with the shape of about 22 hours of
their recorded traffic. Its stress switches are `--makers K`, `--bursts median|busiest`
and `--shock real|stress`. The owner asked for it before deciding whether libsecp256k1
should become the default verifier, so it runs once per verifier.

- **One pass per verifier, each in its verifier's M3 session:** `k256` in the first
  session, `$S`, from the default build; libsecp256k1 in the second, `$S2`, from the
  feature build. The Polymarket runs have names of their own
  (`signed-100k-polymarket-timing-r1`, `search-signed-polymarket`, ...), so they sit next
  to the session's M3 runs without clashing, and each session's report gives every flow
  its own headline, the M3 flow's first: both flows side by side, as D-034 asks. The two
  verifiers' Polymarket runs have the same names, so they must not share a session: a
  session refuses a run of another verifier under a name it already holds.
- **One durable limit.** Each session already holds the M3 flow's pre-verified 20k/s runs
  (`$S` its own, `$S2` section 6's copy), and every flow and switch is judged against the
  limit they set (PIPELINE.md 15.7). If section 6 didn't run, the libsecp256k1 block below
  creates `$S2` the same way.
- **The headlines first** (`sweep --kind polymarket`), interleaved: the flow as it is, with
  the median recorded hour's bursts, and with the stress shock (every 10 s of flow time, 14
  to 88 markets, 14 at the median, move 2% to 6% the same way, and a cohort of
  high-leverage accounts is liquidated together). Each is a full headline: 3 timing runs of 65 s, a capture run (the
  replay test and the audit, over the shocks' liquidations too) and a second seed; 15 runs
  in all. The sweep picks the Polymarket flow itself: no `--flow` needed.
- **Then the searches:** the signed search; the core-path search, only in the `k256` pass
  (its runs are pre-verified and verify nothing, so it is the same for both verifiers); and
  the signed search with 3 market makers (`--makers 3`). With 3 makers each sends about a
  third of all traffic through its one gateway (`account mod G`), so that search finds the
  per-account ceiling (PIPELINE.md 17).
- Pass the session's own options to every command (`--gateways $G`, and in `$S2`
  `--verifier libsecp256k1`); never `--window` to a search, never `--resume`.
- Times, at `--gateways 10` and without reruns: the sweep about 22 minutes (the runs,
  plus signing 4 flows of about 6.5M messages at 15 to 40 s each, since the bursts variant
  reuses the plain flow's, plus 3 replays and audits); a search about 14 runs of 27 s plus
  signing or generating its largest probe, about 8 to 12 minutes. The harness prints its
  own estimate before each command.

**The `k256` pass** (about 50 minutes), in `$S`. After section 7 the binary is already
the default build, and the build line does nothing unless the tree changed. The smoke
test's 6 runs include 3 of the Polymarket flow (signed; pre-verified with 3 makers and
bursts; EIP-712).

```sh
cd ~/perpstest
export PERPS_IN_CONTAINER=1 PERPS_GIT_COMMIT=$(git rev-parse --short HEAD)
cargo build --release --locked --offline -p bench --bin e2e                          # the default build: k256, no C
cargo test --release --locked --offline -p bench --test smoke                        # 6 runs, 3 of the Polymarket flow
cat > ~/m3-polymarket.sh <<'END'
#!/bin/sh
# The D-034 pass with k256 (docs/RUNBOOK-PERPSBOX.md 7b). Resumable, like the others.
set -eu
cd ~/perpstest
E=./target/release/e2e
O="--dir ${S:?} --gateways ${G:?}"
$E sweep --kind polymarket $O                           # ~22 min: 3 headlines, 15 runs
$E search --name signed --flow polymarket $O            # ~12 min
$E search --name core-path --flow polymarket $O         # ~10 min, journal discarded; the same for both verifiers
$E search --name signed --flow polymarket --makers 3 $O # ~8 min: the per-account ceiling
$E report --dir "$S" > "$S/report-final.md"
echo "m3 polymarket k256 pass: done"
END
nohup sh ~/m3-polymarket.sh > ~/m3-polymarket.log 2>&1 &
tail -f ~/m3-polymarket.log                           # Ctrl-C stops the tail, not the pass
```

**The libsecp256k1 pass** (about 45 minutes), in `$S2`, only after
`m3 polymarket k256 pass: done`. Its binary is the feature build (section 6's), rebuilt at
the same path, so to resume the `k256` script after this, rebuild without the feature
first. The smoke test's 9 runs add `signed_polymarket_smoke_run_with_libsecp256k1`.

```sh
cd ~/perpstest
export PERPS_IN_CONTAINER=1 PERPS_GIT_COMMIT=$(git rev-parse --short HEAD)
cargo build --release --locked --offline -p bench --features c-secp256k1 --bin e2e   # compiles libsecp256k1's C
cargo test --release --locked --offline -p bench --features c-secp256k1 --test smoke # 9 runs, 4 of the Polymarket flow
export S2=$S-libsecp256k1                             # section 6's session (made here if section 6 didn't run)
mkdir -p $S2/sweep-load && cp -rn $S/sweep-load/preverified-20k-r* $S2/sweep-load/   # -n: keeps what is there
cat > ~/m3-polymarket-libsecp.sh <<'END'
#!/bin/sh
# The D-034 pass with libsecp256k1 (docs/RUNBOOK-PERPSBOX.md 7b). Resumable, like the others.
set -eu
cd ~/perpstest
E=./target/release/e2e
[ -f "${S2:?}/probe/summary.txt" ] || $E probe --dir "$S2"   # section 6's probe, unless it ran
O="--dir $S2 --gateways ${G:?} --verifier libsecp256k1"
$E sweep --kind polymarket $O                           # ~22 min: 3 headlines, 15 runs
$E search --name signed --flow polymarket $O            # ~12 min
$E search --name signed --flow polymarket --makers 3 $O # ~10 min: the per-account ceiling
$E report --dir "$S2" > "$S2/report-final.md"
echo "m3 polymarket libsecp256k1 pass: done"
END
nohup sh ~/m3-polymarket-libsecp.sh > ~/m3-polymarket-libsecp.log 2>&1 &
tail -f ~/m3-polymarket-libsecp.log                   # Ctrl-C stops the tail, not the pass
```

The smoke test fails a hot thread's minor fault only in a window where the kernel migrated
no page; faults while it did are printed with the count (PIPELINE.md 15.4, 22). So a
smoke failure is a finding: stop and look before spending box time.

**Check before trusting it**, for each session (section 8 applies too):

```sh
for D in $S $S2; do
  echo "== $D"
  grep -rhE '^run\.(flow|makers|bursts|shock|verifier) ' $D/headline/*polymarket*/summary.txt \
    $D/search-*polymarket*/*/summary.txt | sort | uniq -c
  grep -E '^(audit\.passed|audit\.failures|replay\.verdict|breakdown\.window\.liquidations) ' \
    $D/headline/*polymarket*-capture-r*/summary.txt
  grep -h 'end to end on the' $D/report-final.md
  grep -E '^search\.(name|flow|makers|verifier|result|first_fail) ' $D/search-*polymarket*/search.txt
  grep -hE '^(flow\.busiest_account|flow\.busiest_account_ppm|flow\.busiest_account_lane|check\.reject_share_ppm) ' \
    $D/search-signed-polymarket-makers3/*-confirm1/summary.txt
done
```

- Every Polymarket run says `run.flow = polymarket`, with `run.bursts = median` only in the
  `-bursts-median` runs and `run.shock = stress` only in the `-shock-stress` runs, and
  `run.makers = 3` only in the makers-3 search. `run.verifier` is `k256` in `$S` and
  `libsecp256k1` in `$S2` (`none` in the core-path search, which verifies nothing).
- The report has one headline per variant, after the M3 flow's: "100k signed orders/s end
  to end on the Polymarket-shaped flow (D-034) ...", "... + bursts (median hour) ...",
  "... + shocks (stress) ...".
- Each capture run says `audit.passed = true`, `audit.failures = 0` and
  `replay.verdict = identical`. The shock-stress one has liquidations (locally, the stress
  shock at 100k/s: 34 liquidations in a 25-s window, the longest `SetMark` core service
  166 µs, at most 57 events from one command); the other two may have none, since the
  default flow almost never liquidates (D-034). No shock-stress run may carry the flag
  "the flow has shocks (--shock), but none fell in the window": at 100k/s a 60 s window
  holds 6.
- The reject share: locally 0.05% to 0.07% (`check.reject_share_ppm` 490 to 700) at 50k/s
  and 100k/s, with and without the stress shock, `UnknownOrder` (cancels and resizes of
  quotes the takers or a shock's band sweep removed). A few `PostOnlyWouldCross` may join
  them: one maker's new quote reaching the engine before another maker's cancel on another
  gateway (PIPELINE.md 14.12, "Open loop"). Above 1% below saturation is a finding.
- The load per gateway, without `--makers`: the 60 makers spread evenly, so each
  `flow.lane.<g>.messages` is within about 5% of the mean when `G` divides 60 (locally, at
  10 gateways, 9.5% to 10.3% each); with another `G`, within about 10% (7 or 8) to 20%
  (14).
- The makers-3 search: the confirmation runs' busiest account is one of accounts 1 to 3,
  with `flow.busiest_account_ppm` about 290,000 to 370,000, and the report's per-gateway
  table shows its gateway (`flow.busiest_account_lane`) near 100% busy at the failing rate
  while the others have room. Its result is below one core's rate over that share
  (PIPELINE.md 17: the probe's `scaling.<verifier>.0.per_second` / 0.36, about 2.8 times
  one core), since the p99 must stay under the durable limit; the libsecp256k1 session's
  should be about twice the `k256` one's.
- After a new login, `export` the same `S`, `S2` and `G`, and make sure the binary at
  `./target/release/e2e` is the one the script needs (the default build for `$S`, the
  feature build for `$S2`), before starting a script again.

## 8. What to check before trusting a number

Per run (its `summary.txt` and `report.md`; the session report lists invalid runs and why):
- **`check.valid = true`**, and `check.flags` says nothing about a generator limit. Invalid
  runs (throttling, clock inversion, late sender) are run again as `-a2`, `-a3`; the
  report uses valid runs only.
- **Clock source:** `machine.clock_source = tsc` and `check.inversions = 0`. Another clock
  source means no headline (15.1).
- **Throttling and CPU quota:** `check.throttled_periods = 0`. Throttling in any run means
  the layout spins too many threads for `machine.quota`: lower `G`.
- **Page faults:** every `health.<thread>.minor_faults = 0` in the window, the core's
  included (the core writes the engine's reserved memory before the run,
  `Engine::prefault`, PIPELINE.md 15.4). A fault on the box is a finding to report, not
  noise.
- **Generator-limited:** sender lag p99 at most 5 µs (`stage.client.sender_lag.p99`). The
  harness makes such runs invalid unless `--allow-generator-limited` is passed, which it
  must not be here.
- **Drops and rejects:** below saturation, `window.dropped = 0` and
  `check.reject_share_ppm` under 50,000. The flow is built in advance and can't react:
  a dropped cancel leaves a quote that later post-only quotes keep crossing, so 1,000
  drops from one lane put the reject share at 20% to 27% for the next 10 s of flow
  (PIPELINE.md 22; locally, 3,615 drops gave 28.7%). Drops at a rate the pipeline should
  sustain mean a stall at least as long as a lane's worth of flow (1,024 items a lane:
  about 100 ms at 100k/s over 10 lanes): look at the probe's jitter.
- **Identity:** `machine.commit` is the hash you built, not empty or `unknown`, and
  `machine.profile` is `release`.
- **The limit label:** `health.limit` names what limited a saturated run. The journal
  writer's busy share is CPU work; its time blocked in `fdatasync` is
  `health.journal_writer.fdatasync_ppm`, reported apart.

For the session: the durable limit printed with each result is `2 × T` plus the median
`fdatasync` p99 of the three pre-verified 20k/s runs (15.7), and "headline: yes" needs 3
valid timing runs whose clock counts (15.7).

## 9. Copy the results back

From the local machine (all four sessions' summaries, reports, probes and logs, the D-034
passes' included, since they live in the first two sessions; journals and `*.bin` captures
stay behind):

```sh
S=runs/m3-2026-10-01                                  # EDIT: the box's session, relative to its home
mkdir -p ~/perpsbox-results
ssh PERPSBOX "tar -C ~ --exclude='*/journal' --exclude='*.bin' -czf - $S $S-libsecp256k1 $S-eip712 $S-eip712-k256 \
  m3-session.log m3-libsecp.log m3-eip712.log m3-eip712-k256.log m3-polymarket*.log" \
  | tar -xzf - -C ~/perpsbox-results
ls ~/perpsbox-results/$S ~/perpsbox-results/$S-libsecp256k1 ~/perpsbox-results/$S-eip712 ~/perpsbox-results/$S-eip712-k256
```

`report-final.md` is the fragment for `docs/BENCHMARKS.md` (pasted by hand, 15.11), and
`$S-libsecp256k1/report-final.md` holds the libsecp256k1 pass (its probe section gives both
verifiers' single-thread cost and scaling curve, measured the same way);
`$S-eip712/report-final.md` and `$S-eip712-k256/report-final.md` hold the EIP-712 passes
(their probe sections add each verifier's signer check and recover-scaling curve). The
Polymarket-shaped flow's headlines and searches are in `$S/report-final.md` (`k256`) and
`$S-libsecp256k1/report-final.md` (libsecp256k1), after the M3 flow's. Update the PERPSBOX
row of its machines table from `probe/summary.txt`, and the `evidence:` lines of
D-021..D-034 in `docs/DECISIONS.md`.

## 10. Tear down

Check the copy (`report-final.md` and every `summary.txt` are there), then **destroy** the instance in the vast.ai
console (or `vastai destroy instance <id>`). A stopped instance still bills for its disk.
The box held no secrets, so there is nothing to revoke.

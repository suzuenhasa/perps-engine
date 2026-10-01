//! The machine probes run before a session's benchmarks, and the machine facts every run
//! records (`docs/PIPELINE.md` 15.10, 15.1, 15.4, 2.6 and 11.6).
//!
//! **Contract.** Everything is read from files with `std::fs` (`/proc`, `/sys`) or measured
//! with `std::time::Instant`:
//! - [`ClockCheck`]: the clock source and the cost of one `Instant::now()`. A run counts
//!   toward a headline only if the source is read through the vDSO (`tsc`, `kvm-clock`,
//!   `hyperv_clocksource_tsc_page`) and a read costs at most 50 ns (15.1).
//! - [`Mount`] and [`fs_verdict`]: the file system holding a directory, from
//!   `/proc/self/mountinfo` (the mount whose mount point is the longest prefix of the path).
//!   tmpfs, ramfs and an overlay mounted `volatile` are refused: `fdatasync` there makes
//!   nothing durable. ZFS and btrfs are noted: copy-on-write makes preallocation useless
//!   (11.6).
//! - [`verify_ns`] and [`sign_costs`]: one thread's verification, with each verifier the
//!   binary has (`k256`, and libsecp256k1 in a build with `--features c-secp256k1`; 5.7),
//!   and `k256`'s signing and SHA-256 of 72 bytes.
//! - [`recover_ns`] and [`eip712_costs`]: the same for the EIP-712 scheme (5.8): one
//!   thread's signer check per verifier (the gateway's checks 11 to 13: low-S, the digest,
//!   the recovery and the address), and, as parts of it, the digest of a place (its
//!   MessagePack form and three keccak-256 hashes) and one keccak-256 of 64 bytes.
//! - [`verify_scaling`] and [`recover_scaling`]: verifications, or signer checks, a second
//!   with one verifier on 1, 2, 4, ... physical cores, then with their SMT siblings too:
//!   the evidence for the gateway count (section 17). The probe draws both curves for each
//!   verifier the binary has.
//! - [`jitter`]: on every candidate physical core at once, the largest gap between
//!   consecutive clock reads and how many gaps exceed 10 µs; the quietest two cores get the
//!   core and the sequencer (2.6).
//! - [`fsync_probe`]: journal-like writes (15.2 KB every 1 ms, as at 100k signed orders a
//!   second) into the run directory, with the `fdatasync` histogram; a p50 under 20 µs is
//!   "suspiciously fast", and [`fsync_verdict`] says what it is believed to be, and why.
//!
//! **Complexity.** Each probe runs for the duration it is given; the parsers are linear in
//! the files they read.

use std::fs::{self, File, OpenOptions};
use std::hint::black_box;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use engine::command::{Command, PlaceOrder};
use engine::types::{AccountId, MarketId, OrderSeq, Price, Qty, Side, TimeInForce, order_id};
use gateway::eip712::{Address, Domain};
use gateway::keccak::keccak256;
use gateway::wire::{
    DecodedEip712, SIGNATURE_BYTES, check_signer, decode_eip712, encode_signed_part, sign_eip712, signature,
    verify_signature,
};
use gateway::{PublicKey, VerifierKind};
use k256::ecdsa::signature::Signer;
use k256::ecdsa::{Signature, SigningKey};
use k256::sha2::{Digest, Sha256};
use pipeline::affinity::{Topology, pin_current_thread};
use pipeline::histogram::LatencyHistogram;

use super::summary::Summary;

// ---------------------------------------------------------------------------------------
// The clock (15.1).

/// Clock sources the kernel reads through the vDSO, with no system call (15.1).
pub const VDSO_CLOCK_SOURCES: [&str; 3] = ["tsc", "kvm-clock", "hyperv_clocksource_tsc_page"];
/// The most a clock read may cost for a run to count toward a headline, in picoseconds.
pub const MAX_CLOCK_READ_PS: u64 = 50_000;

/// The clock source and what one read costs (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockCheck {
    /// `current_clocksource`, or "unknown".
    pub source: String,
    /// The mean cost of one `Instant::now()`, in picoseconds.
    pub read_ps: u64,
}

impl ClockCheck {
    /// Reads the clock source and times `reads` back-to-back clock reads.
    pub fn measure(reads: u32) -> ClockCheck {
        let source = read_trimmed("/sys/devices/system/clocksource/clocksource0/current_clocksource")
            .unwrap_or_else(|| "unknown".to_string());
        let started = Instant::now();
        for _ in 0..reads {
            black_box(Instant::now());
        }
        let total_ps = started.elapsed().as_nanos() * 1_000;
        ClockCheck { source, read_ps: (total_ps / u128::from(reads.max(1))) as u64 }
    }

    /// True if the source is read through the vDSO and a read costs at most 50 ns.
    pub fn counts_for_headline(&self) -> bool {
        VDSO_CLOCK_SOURCES.contains(&self.source.as_str()) && self.read_ps <= MAX_CLOCK_READ_PS
    }
}

// ---------------------------------------------------------------------------------------
// The machine.

/// The first line of a file, trimmed; `None` if it can't be read.
fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok().map(|text| text.trim().to_string())
}

/// The value of the first `key : value` line of `/proc/cpuinfo`-style text.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then(|| value.trim())
    })
}

/// The CPU's model name.
pub fn cpu_model() -> Option<String> {
    field(&fs::read_to_string("/proc/cpuinfo").ok()?, "model name").map(str::to_string)
}

/// The kernel release.
pub fn kernel() -> Option<String> {
    read_trimmed("/proc/sys/kernel/osrelease")
}

/// The memory this process may use, in bytes: the machine's (`MemTotal` in
/// `/proc/meminfo`), or the container's cgroup limit if that is lower. In a container
/// `MemTotal` is the host's memory, and going past the cgroup's limit gets the process
/// killed (review finding F11-memory-cap-ignores-cgroup).
pub fn memory_bytes() -> Option<u64> {
    let machine = machine_memory_bytes()?;
    Some(cgroup_memory_limit().map_or(machine, |limit| limit.min(machine)))
}

/// `MemTotal` in `/proc/meminfo`, in bytes.
fn machine_memory_bytes() -> Option<u64> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let kib: u64 = field(&text, "MemTotal")?.trim_end_matches("kB").trim().parse().ok()?;
    Some(kib * 1_024)
}

/// The cgroup's memory limit, read the way `CpuQuota::read` reads the CPU quota: `memory.max`
/// in cgroup v2, else `memory.limit_in_bytes` in v1. `None` if there is no limit (`max` in
/// v2, which doesn't parse as a number; v1's "no limit" is a huge number, which the caller's
/// `min` with the machine's memory takes care of) or no such file.
fn cgroup_memory_limit() -> Option<u64> {
    ["/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"]
        .into_iter()
        .find_map(read_trimmed)
        .and_then(|text| text.parse().ok())
}

/// The memory this process holds now (`VmRSS` in `/proc/self/status`), in bytes.
pub fn resident_bytes() -> Option<u64> {
    let text = fs::read_to_string("/proc/self/status").ok()?;
    let kib: u64 = field(&text, "VmRSS")?.trim_end_matches("kB").trim().parse().ok()?;
    Some(kib * 1_024)
}

/// The current frequency of logical CPU `cpu` in MHz: `scaling_cur_freq` where present,
/// else `cpu MHz` in `/proc/cpuinfo` (15.4).
pub fn cpu_mhz(cpu: usize) -> Option<u64> {
    let scaling = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_cur_freq");
    if let Some(khz) = read_trimmed(scaling).and_then(|text| text.parse::<u64>().ok()) {
        return Some(khz / 1_000);
    }
    let text = fs::read_to_string("/proc/cpuinfo").ok()?;
    let block = text.split("\n\n").find(|block| field(block, "processor") == Some(&cpu.to_string()))?;
    field(block, "cpu MHz")?.parse::<f64>().ok().map(|mhz| mhz.round() as u64)
}

// ---------------------------------------------------------------------------------------
// The file system (15.10, 11.6).

/// One line of `/proc/self/mountinfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub mount_point: String,
    /// `major:minor` of the device (major 0 for virtual file systems such as overlay).
    pub device: String,
    pub fs_type: String,
    pub source: String,
    /// The per-mount options, then the file system's own.
    pub mount_options: String,
    pub super_options: String,
}

impl Mount {
    /// An overlay's upper directory, where its writes go.
    pub fn upperdir(&self) -> Option<&str> {
        self.super_options.split(',').find_map(|option| option.strip_prefix("upperdir="))
    }

    fn has_option(&self, name: &str) -> bool {
        self.mount_options.split(',').chain(self.super_options.split(',')).any(|option| option == name)
    }
}

/// Parses one mountinfo line: `id parent major:minor root mount-point options [optional
/// fields] - type source super-options`.
pub fn parse_mountinfo_line(line: &str) -> Option<Mount> {
    let (before, after) = line.split_once(" - ")?;
    let fields: Vec<&str> = before.split(' ').collect();
    let mut rest = after.split(' ');
    Some(Mount {
        device: fields.get(2)?.to_string(),
        mount_point: unescape(fields.get(4)?),
        mount_options: fields.get(5)?.to_string(),
        fs_type: rest.next()?.to_string(),
        source: rest.next()?.to_string(),
        super_options: rest.next().unwrap_or("").to_string(),
    })
}

/// Undoes mountinfo's octal escapes (`\040` for a space).
fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.by_ref().take(3).collect();
        match u8::from_str_radix(&digits, 8) {
            Ok(byte) => out.push(char::from(byte)),
            Err(_) => {
                out.push('\\');
                out.push_str(&digits);
            }
        }
    }
    out
}

/// The mount holding `path` (absolute): the one whose mount point is its longest prefix,
/// counting whole components; of mounts on the same point, the last (the one on top).
pub fn mount_of(path: &Path, mountinfo: &str) -> Option<Mount> {
    let mut best: Option<Mount> = None;
    for mount in mountinfo.lines().filter_map(parse_mountinfo_line) {
        let holds = path.starts_with(&mount.mount_point);
        let longer = best.as_ref().is_none_or(|b| mount.mount_point.len() >= b.mount_point.len());
        if holds && longer {
            best = Some(mount);
        }
    }
    best
}

/// The mount holding directory `dir` (created if it doesn't exist).
pub fn file_system_of(dir: &Path) -> io::Result<Mount> {
    fs::create_dir_all(dir)?;
    let path = fs::canonicalize(dir)?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    mount_of(&path, &mountinfo)
        .ok_or_else(|| io::Error::other(format!("no mount holds {} in /proc/self/mountinfo", path.display())))
}

/// What a file system means for durable numbers (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsVerdict {
    /// `fdatasync` makes nothing durable here: refused.
    Refused(String),
    /// Durable as reported, but preallocation has no effect (copy-on-write).
    CopyOnWrite(String),
    Ok,
}

/// Judges a mount (module docs).
pub fn fs_verdict(mount: &Mount) -> FsVerdict {
    match mount.fs_type.as_str() {
        "tmpfs" | "ramfs" => FsVerdict::Refused(format!("{} keeps files in memory only", mount.fs_type)),
        "overlay" if mount.has_option("volatile") => {
            FsVerdict::Refused("an overlay mounted with `volatile` ignores fsync".to_string())
        }
        "zfs" | "btrfs" => FsVerdict::CopyOnWrite(format!(
            "{} is copy-on-write: preallocation has no effect, every overwrite allocates (11.6)",
            mount.fs_type
        )),
        _ => FsVerdict::Ok,
    }
}

/// The block device's `queue/write_cache` ("write back" or "write through") for a mount's
/// `major:minor`, looking at the whole disk for a partition; `None` for virtual devices.
pub fn write_cache(device: &str) -> Option<String> {
    let base = PathBuf::from(format!("/sys/dev/block/{device}"));
    read_trimmed(base.join("queue/write_cache")).or_else(|| read_trimmed(base.join("../queue/write_cache")))
}

// ---------------------------------------------------------------------------------------
// Signatures (15.10, section 17, 5.7).

/// One thread's cost of `k256`'s signing (the load generator's, 14.8) and of SHA-256 of 72
/// bytes, in nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignCosts {
    pub sign_ns: u64,
    pub sha256_72_ns: u64,
}

/// The account of the benchmark's sample messages: the worked example's (5.6).
const SAMPLE_ACCOUNT: AccountId = AccountId::new(9);

/// The command of the benchmark's sample messages: the worked example's place (5.6).
fn sample_command() -> Command {
    Command::PlaceOrder(PlaceOrder {
        order_id: order_id(SAMPLE_ACCOUNT, OrderSeq::new(1)),
        price: Price::new(102_998),
        qty: Qty::new(500_000),
        market: MarketId::new(3),
        side: Side::Buy,
        tif: TimeInForce::Gtc,
        post_only: true,
    })
}

/// A signed message of the benchmark's kind: the part that is signed, and its signature.
fn sample_signed() -> (SigningKey, [u8; 72], [u8; SIGNATURE_BYTES]) {
    let key = loadgen::keys::signing_key(1, SAMPLE_ACCOUNT);
    let signed = encode_signed_part(1, SAMPLE_ACCOUNT, 1, u64::MAX, &sample_command());
    let signature: Signature = key.sign(&signed);
    (key, signed, signature.to_bytes().into())
}

/// `key`'s public key, parsed for `verifier`. Panics if this binary doesn't have
/// `verifier`: the callers take theirs from `VerifierKind::built`.
fn public_key(verifier: VerifierKind, key: &SigningKey) -> PublicKey {
    if let Err(not_built) = verifier.check_built() {
        panic!("{not_built}");
    }
    let compressed = PublicKey::K256(*key.verifying_key()).to_compressed();
    PublicKey::from_compressed(verifier, &compressed).expect("the sample's key is a point on the curve")
}

/// One verification with `verifier` on this thread, in nanoseconds: the mean of
/// `iterations`, through the gateway's own check (`wire::verify_signature`).
pub fn verify_ns(verifier: VerifierKind, iterations: u32) -> u64 {
    let (key, signed, signature) = sample_signed();
    let key = public_key(verifier, &key);
    let started = Instant::now();
    for _ in 0..iterations {
        assert!(verify_signature(&key, black_box(&signed), black_box(&signature)).is_ok());
    }
    (started.elapsed().as_nanos() / u128::from(iterations.max(1))) as u64
}

/// Times `iterations` signatures, and 100 times as many SHA-256s of 72 bytes, on this
/// thread (module docs).
pub fn sign_costs(iterations: u32) -> SignCosts {
    let (key, signed, _) = sample_signed();
    let per = |elapsed: Duration| (elapsed.as_nanos() / u128::from(iterations.max(1))) as u64;
    let started = Instant::now();
    for _ in 0..iterations {
        let signature: Signature = key.sign(black_box(&signed));
        black_box(signature);
    }
    let sign_ns = per(started.elapsed());
    let started = Instant::now();
    for _ in 0..iterations * 100 {
        black_box(Sha256::digest(black_box(&signed)));
    }
    let sha256_72_ns = (started.elapsed().as_nanos() / u128::from((iterations * 100).max(1))) as u64;
    SignCosts { sign_ns, sha256_72_ns }
}

/// An EIP-712 message of the benchmark's kind (5.8), as a gateway holds it after its cheap
/// checks: the same place, signed by account 9's key for deployment 1 with salt 1.
struct Eip712Sample {
    domain: Domain,
    decoded: DecodedEip712,
    signature: [u8; SIGNATURE_BYTES],
    /// Account 9's address, which the recovered key's must be.
    address: Address,
}

impl Eip712Sample {
    fn new() -> Eip712Sample {
        let key = loadgen::keys::signing_key(1, SAMPLE_ACCOUNT);
        let domain = Domain::new(1);
        let message = sign_eip712(&key, &domain, SAMPLE_ACCOUNT, 1, 1_790_000_000_000, &sample_command());
        Eip712Sample {
            domain,
            decoded: decode_eip712(&message).expect("the gateway decodes it"),
            signature: *signature(&message),
            address: PublicKey::K256(*key.verifying_key()).address(),
        }
    }

    /// The gateway's checks 11 to 13 with `verifier` (`wire::check_signer`); panics unless
    /// they pass, which they do with every verifier the binary has.
    fn check(&self, verifier: VerifierKind) {
        let checked = check_signer(verifier, &self.domain, &self.decoded, &self.signature, &self.address);
        assert_eq!(checked, Ok(()), "{verifier} recovers the sample's signer");
    }
}

/// One EIP-712 signer check with `verifier` on this thread, in nanoseconds: the mean of
/// `iterations`, through the gateway's own check (module docs). Panics if this binary
/// doesn't have `verifier`: the callers take theirs from `VerifierKind::built`.
pub fn recover_ns(verifier: VerifierKind, iterations: u32) -> u64 {
    if let Err(not_built) = verifier.check_built() {
        panic!("{not_built}");
    }
    let sample = Eip712Sample::new();
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(&sample).check(verifier);
    }
    (started.elapsed().as_nanos() / u128::from(iterations.max(1))) as u64
}

/// One thread's cost of the EIP-712 scheme's hashing, in nanoseconds (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eip712Costs {
    /// The digest of a place: its compact form in MessagePack, and three keccak-256 hashes.
    pub digest_ns: u64,
    /// keccak-256 of 64 bytes, one block: what the address costs, after the recovery.
    pub keccak_64_ns: u64,
}

/// Times `100 × iterations` digests and keccak-256 hashes on this thread (module docs).
pub fn eip712_costs(iterations: u32) -> Eip712Costs {
    let sample = Eip712Sample::new();
    let n = iterations.max(1) * 100;
    let per = |elapsed: Duration| (elapsed.as_nanos() / u128::from(n)) as u64;
    let started = Instant::now();
    for _ in 0..n {
        black_box(black_box(&sample.decoded).digest(&sample.domain));
    }
    let digest_ns = per(started.elapsed());
    let block = [0x5A_u8; 64];
    let started = Instant::now();
    for _ in 0..n {
        black_box(keccak256(black_box(&block)));
    }
    Eip712Costs { digest_ns, keccak_64_ns: per(started.elapsed()) }
}

/// Verifications, or EIP-712 signer checks, a second with some number of threads, each
/// pinned to its CPU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScalingPoint {
    pub threads: usize,
    /// True if the threads also use the SMT siblings of their physical cores.
    pub smt: bool,
    pub per_second: u64,
}

/// Verifications a second with `verifier` on 1, 2, 4, ... physical cores (one sibling
/// each), then on every physical core with both siblings (module docs), each point for
/// `duration`.
pub fn verify_scaling(verifier: VerifierKind, topology: &Topology, duration: Duration) -> Vec<ScalingPoint> {
    let (key, signed, signature) = sample_signed();
    let key = public_key(verifier, &key);
    scaling(topology, duration, || assert!(verify_signature(&key, &signed, &signature).is_ok()))
}

/// EIP-712 signer checks a second with `verifier` (checks 11 to 13, as [`recover_ns`]), on
/// the same cores as [`verify_scaling`] (module docs), each point for `duration`.
pub fn recover_scaling(verifier: VerifierKind, topology: &Topology, duration: Duration) -> Vec<ScalingPoint> {
    if let Err(not_built) = verifier.check_built() {
        panic!("{not_built}");
    }
    let sample = Eip712Sample::new();
    scaling(topology, duration, || sample.check(verifier))
}

/// `check` done a second on 1, 2, 4, ... physical cores (one sibling each), then on every
/// physical core with both siblings, each point for `duration`.
fn scaling(topology: &Topology, duration: Duration, check: impl Fn() + Sync) -> Vec<ScalingPoint> {
    let Some(package) = topology.busiest_package() else { return Vec::new() };
    let cores: Vec<Vec<usize>> = topology
        .physical_cores(package)
        .into_iter()
        .filter(|c| !c.allowed.is_empty())
        .map(|c| c.allowed)
        .collect();
    let firsts: Vec<usize> = cores.iter().map(|cpus| cpus[0]).collect();
    let mut points = Vec::new();
    let mut n = 1;
    while n <= firsts.len() {
        let per_second = per_second_on(&firsts[..n], duration, &check);
        points.push(ScalingPoint { threads: n, smt: false, per_second });
        n = if n == firsts.len() { n + 1 } else { (n * 2).min(firsts.len()) };
    }
    let all: Vec<usize> = cores.iter().flatten().copied().collect();
    if all.len() > firsts.len() {
        let per_second = per_second_on(&all, duration, &check);
        points.push(ScalingPoint { threads: all.len(), smt: true, per_second });
    }
    points
}

/// `check` done a second with one thread pinned to each of `cpus`, for `duration`.
fn per_second_on(cpus: &[usize], duration: Duration, check: &(impl Fn() + Sync)) -> u64 {
    let done = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for &cpu in cpus {
            let (done, stop) = (&done, &stop);
            scope.spawn(move || {
                let _ = pin_current_thread(cpu); // unpinned still measures something
                let mut n = 0;
                while !stop.load(Ordering::Relaxed) {
                    check();
                    n += 1;
                }
                done.fetch_add(n, Ordering::Relaxed);
            });
        }
        std::thread::sleep(duration);
        stop.store(true, Ordering::Relaxed);
    });
    (u128::from(done.into_inner()) * 1_000_000_000 / duration.as_nanos().max(1)) as u64
}

// ---------------------------------------------------------------------------------------
// Jitter (2.6).

/// What one CPU's clock-read loop saw (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Jitter {
    pub cpu: usize,
    pub max_gap_ns: u64,
    pub gaps_over_10us: u64,
}

/// A gap in a tight clock-read loop longer than this means the CPU was taken away.
pub const JITTER_GAP_NS: u64 = 10_000;

/// Runs a clock-read loop on each of `cpus` at once for `duration` (module docs), quietest
/// first: fewest gaps over 10 µs, then the smallest largest gap.
pub fn jitter(cpus: &[usize], duration: Duration) -> Vec<Jitter> {
    let mut results: Vec<Jitter> = std::thread::scope(|scope| {
        let handles: Vec<_> = cpus
            .iter()
            .map(|&cpu| {
                scope.spawn(move || {
                    let _ = pin_current_thread(cpu);
                    let started = Instant::now();
                    let mut last = started;
                    let (mut max_gap_ns, mut gaps_over_10us) = (0, 0);
                    while last.duration_since(started) < duration {
                        let now = Instant::now();
                        let gap = now.duration_since(last).as_nanos() as u64;
                        max_gap_ns = gap.max(max_gap_ns);
                        gaps_over_10us += u64::from(gap > JITTER_GAP_NS);
                        last = now;
                    }
                    Jitter { cpu, max_gap_ns, gaps_over_10us }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("the jitter loop ends")).collect()
    });
    results.sort_by_key(|j| (j.gaps_over_10us, j.max_gap_ns));
    results
}

// ---------------------------------------------------------------------------------------
// fdatasync (15.10, 11.6).

/// What the fsync probe measured.
#[derive(Clone, Debug)]
pub struct FsyncProbe {
    /// `fdatasync` alone, per batch.
    pub fdatasync_ns: LatencyHistogram,
    pub flushes: u64,
    pub elapsed_ns: u64,
}

/// The largest probe file: the writes wrap around in it, so every write lands on blocks
/// that were already written, as the journal's do in its preallocated segments (11.6).
const MAX_FSYNC_FILE_BYTES: u64 = 64 << 20;

/// Writes `batch_bytes` every `interval` into a preallocated file in `dir`, each followed by
/// `fdatasync`, for `duration` (module docs). The file is removed afterwards.
pub fn fsync_probe(
    dir: &Path,
    duration: Duration,
    batch_bytes: usize,
    interval: Duration,
) -> io::Result<FsyncProbe> {
    fs::create_dir_all(dir)?;
    let path = dir.join("fsync-probe.bin");
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path)?;
    let result = probe_file(&file, duration, batch_bytes, interval);
    drop(file);
    fs::remove_file(&path)?;
    result
}

fn probe_file(
    file: &File,
    duration: Duration,
    batch_bytes: usize,
    interval: Duration,
) -> io::Result<FsyncProbe> {
    // Room for every batch the probe will write, up to the cap, written with zeros first.
    let batches = (duration.as_nanos() / interval.as_nanos().max(1)) as u64 + 1;
    let file_bytes = (batches * batch_bytes as u64).min(MAX_FSYNC_FILE_BYTES).max(batch_bytes as u64);
    let zeros = vec![0u8; 1 << 20];
    let mut offset = 0;
    while offset < file_bytes {
        let n = (file_bytes - offset).min(zeros.len() as u64) as usize;
        file.write_all_at(&zeros[..n], offset)?;
        offset += n as u64;
    }
    file.sync_all()?;
    // Nonzero, record-like bytes.
    let batch: Vec<u8> = (0..batch_bytes).map(|i| (i % 251) as u8 + 1).collect();
    let mut fdatasync_ns = LatencyHistogram::new();
    let started = Instant::now();
    let (mut offset, mut flushes, mut next) = (0, 0, Duration::ZERO);
    while started.elapsed() < duration {
        while started.elapsed() < next {
            std::hint::spin_loop(); // like the writer, which spins between batches
        }
        next += interval;
        if offset + batch_bytes as u64 > file_bytes {
            offset = 0;
        }
        file.write_all_at(&batch, offset)?;
        let synced = Instant::now();
        file.sync_data()?;
        fdatasync_ns.record(synced.elapsed().as_nanos() as u64);
        offset += batch_bytes as u64;
        flushes += 1;
    }
    Ok(FsyncProbe { fdatasync_ns, flushes, elapsed_ns: started.elapsed().as_nanos() as u64 })
}

/// A flush faster than this at the median is suspicious (15.10).
pub const SUSPICIOUS_FSYNC_NS: u64 = 20_000;

/// What an `fdatasync` p50 of `p50_ns` is believed to be, and why (15.10).
pub fn fsync_verdict(p50_ns: u64, mount: &Mount, write_cache: Option<&str>, kernel: Option<&str>) -> String {
    if p50_ns >= SUSPICIOUS_FSYNC_NS {
        return format!("not suspicious: a p50 of {p50_ns} ns is what a device that really flushes takes");
    }
    let why = if let FsVerdict::Refused(reason) = fs_verdict(mount) {
        format!("a flush that does nothing: {reason}")
    } else if kernel.is_some_and(|k| k.to_lowercase().contains("microsoft")) {
        "a flush WSL2's virtual disk may absorb: whether it reaches the physical disk depends on the Windows \
         host's disk caching"
            .to_string()
    } else {
        match write_cache {
            Some("write through") => "a device with power-loss protection or no volatile cache: the device \
                                      reports a write-through cache, so a flush has nothing to empty"
                .to_string(),
            Some("write back") => "a flush that does nothing, or reaches only a volatile cache: the device \
                                   reports a write-back cache, which a real flush would have to empty"
                .to_string(),
            _ => format!(
                "unknown: {} on device {} reports no write-cache setting (a virtual or network disk?)",
                mount.fs_type, mount.device
            ),
        }
    };
    format!("suspiciously fast (p50 {p50_ns} ns, under 20 µs): believed to be {why}")
}

// ---------------------------------------------------------------------------------------
// The probe as a whole.

/// How long each part of the probe runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeDurations {
    pub clock_reads: u32,
    /// Verifications timed per verifier, and signatures timed.
    pub signature_iterations: u32,
    pub scaling: Duration,
    pub jitter: Duration,
    pub fsync: Duration,
}

impl ProbeDurations {
    /// The spec's: 10M clock reads, 10 s of jitter, 30 s of fsync (15.10).
    pub fn full() -> ProbeDurations {
        ProbeDurations {
            clock_reads: 10_000_000,
            signature_iterations: 2_000,
            scaling: Duration::from_secs(2),
            jitter: Duration::from_secs(10),
            fsync: Duration::from_secs(30),
        }
    }

    /// A quick look, for development: about 10 s in all.
    pub fn quick() -> ProbeDurations {
        ProbeDurations {
            clock_reads: 1_000_000,
            signature_iterations: 200,
            scaling: Duration::from_millis(300),
            jitter: Duration::from_secs(1),
            fsync: Duration::from_secs(3),
        }
    }
}

/// Runs every probe of 15.10 against `run_dir` and returns what it found, as a summary
/// (`probe.*` keys; the report renders it). Printing is the caller's.
pub fn probe(run_dir: &Path, durations: ProbeDurations) -> io::Result<Summary> {
    let mut s = Summary::new();
    let topology = Topology::read()?;
    let package = topology.busiest_package();
    let cores = package.map(|p| topology.physical_cores(p)).unwrap_or_default();
    s.put("machine.cpu_model", cpu_model().unwrap_or_else(|| "unknown".into()));
    s.put("machine.kernel", kernel().unwrap_or_else(|| "unknown".into()));
    s.put("machine.memory_bytes", memory_bytes().unwrap_or(0));
    s.put(
        "machine.memory_limit",
        match cgroup_memory_limit() {
            Some(limit) if machine_memory_bytes().is_some_and(|machine| limit < machine) => "cgroup",
            _ => "machine",
        },
    );
    s.put("machine.logical_cpus", topology.cpus().len());
    s.put("machine.allowed_cpus", topology.allowed().len());
    s.put("machine.physical_cores", cores.len());
    s.put("machine.packages", distinct_packages(&topology));
    s.put("machine.quota", format!("{:?}", pipeline::affinity::CpuQuota::read()?));
    let throttling = pipeline::affinity::Throttling::read()?;
    s.put("machine.throttled_periods", throttling.map_or(0, |t| t.nr_throttled));

    let clock = ClockCheck::measure(durations.clock_reads);
    s.put("clock.source", &clock.source);
    s.put("clock.read_ps", clock.read_ps);
    s.put("clock.counts_for_headline", clock.counts_for_headline());

    let mount = file_system_of(run_dir)?;
    let cache = write_cache(&mount.device);
    s.put("fs.dir", run_dir.display());
    s.put("fs.mount_point", &mount.mount_point);
    s.put("fs.type", &mount.fs_type);
    s.put("fs.options", format!("{} {}", mount.mount_options, mount.super_options));
    s.put("fs.upperdir", mount.upperdir().unwrap_or("-"));
    s.put("fs.write_cache", cache.as_deref().unwrap_or("-"));
    s.put("fs.verdict", format!("{:?}", fs_verdict(&mount)));

    // `<verifier>.verify_ns` and `scaling.<verifier>.<i>.*`, for each verifier this binary
    // has (5.7); and the EIP-712 scheme's `<verifier>.recover_ns` and
    // `scaling.<verifier>_recover.<i>.*` (5.8).
    for verifier in VerifierKind::built() {
        let name = verifier.name();
        s.put(format!("{name}.verify_ns"), verify_ns(verifier, durations.signature_iterations));
        put_scaling(&mut s, name, &verify_scaling(verifier, &topology, durations.scaling));
        s.put(format!("{name}.recover_ns"), recover_ns(verifier, durations.signature_iterations));
        let curve = recover_scaling(verifier, &topology, durations.scaling);
        put_scaling(&mut s, &format!("{name}_recover"), &curve);
    }
    let costs = sign_costs(durations.signature_iterations);
    s.put("k256.sign_ns", costs.sign_ns);
    s.put("k256.sha256_72_ns", costs.sha256_72_ns);
    let eip712 = eip712_costs(durations.signature_iterations);
    s.put("eip712.digest_ns", eip712.digest_ns);
    s.put("keccak.64_ns", eip712.keccak_64_ns);

    // Every physical core but the first (the sender's and main's), on one sibling each.
    let candidates: Vec<usize> =
        cores.iter().skip(1).filter_map(|core| core.allowed.first().copied()).collect();
    let quiet = jitter(&candidates, durations.jitter);
    for j in &quiet {
        s.put(format!("jitter.cpu{}.max_gap_ns", j.cpu), j.max_gap_ns);
        s.put(format!("jitter.cpu{}.gaps_over_10us", j.cpu), j.gaps_over_10us);
    }
    let ranking: Vec<String> = quiet.iter().map(|j| j.cpu.to_string()).collect();
    s.put("jitter.quietest_first", if ranking.is_empty() { "-".to_string() } else { ranking.join(",") });

    if let FsVerdict::Refused(reason) = fs_verdict(&mount) {
        s.put("fsync.refused", reason);
    } else {
        let probe = fsync_probe(run_dir, durations.fsync, 15_200, Duration::from_millis(1))?;
        s.put_histogram("fsync.fdatasync", &probe.fdatasync_ns);
        s.put("fsync.flushes_per_s", probe.flushes * 1_000_000_000 / probe.elapsed_ns.max(1));
        let p50 = probe.fdatasync_ns.percentile(50, 100).unwrap_or(0);
        s.put("fsync.verdict", fsync_verdict(p50, &mount, cache.as_deref(), kernel().as_deref()));
    }
    Ok(s)
}

/// A scaling curve as `scaling.<curve>.<i>.threads`, `.smt` and `.per_second`.
fn put_scaling(s: &mut Summary, curve: &str, points: &[ScalingPoint]) {
    for (i, point) in points.iter().enumerate() {
        s.put(format!("scaling.{curve}.{i}.threads"), point.threads);
        s.put(format!("scaling.{curve}.{i}.smt"), point.smt);
        s.put(format!("scaling.{curve}.{i}.per_second"), point.per_second);
    }
}

fn distinct_packages(topology: &Topology) -> usize {
    let mut packages: Vec<i32> = topology.cpus().iter().map(|cpu| cpu.package).collect();
    packages.sort_unstable();
    packages.dedup();
    packages.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 0:21 / / rw,relatime - overlay overlay rw,lowerdir=/l,upperdir=/var/lib/docker/overlay2/x/diff,workdir=/w
30 22 8:3 / /target rw,relatime - ext4 /dev/sdc rw,discard
31 22 0:40 / /tmp rw,nosuid - tmpfs tmpfs rw,size=65536k
32 22 0:41 / /data\\040dir rw - overlay overlay rw,upperdir=/u,volatile
33 30 0:42 / /target/zfs rw - zfs pool/ds rw,xattr";

    #[test]
    fn the_mount_is_the_longest_prefix_counting_whole_components() {
        let mount_at = |path: &str| mount_of(Path::new(path), MOUNTINFO).expect("a mount").mount_point;
        assert_eq!(mount_at("/target/runs/x/journal"), "/target");
        assert_eq!(mount_at("/targets"), "/", "/target is not a prefix of /targets");
        assert_eq!(mount_at("/tmp/a"), "/tmp");
        assert_eq!(mount_at("/data dir/journal"), "/data dir", "octal escapes are undone");
        assert_eq!(mount_at("/target/zfs/j"), "/target/zfs");
        let root = mount_of(Path::new("/work"), MOUNTINFO).expect("root");
        assert_eq!(root.upperdir(), Some("/var/lib/docker/overlay2/x/diff"));
        assert_eq!(root.device, "0:21");
    }

    #[test]
    fn memory_and_volatile_file_systems_are_refused_and_copy_on_write_is_noted() {
        let verdict = |path: &str| fs_verdict(&mount_of(Path::new(path), MOUNTINFO).expect("a mount"));
        assert!(matches!(verdict("/tmp/j"), FsVerdict::Refused(_)));
        assert!(matches!(verdict("/data dir/j"), FsVerdict::Refused(_)));
        assert!(matches!(verdict("/target/zfs/j"), FsVerdict::CopyOnWrite(_)));
        assert_eq!(verdict("/target/j"), FsVerdict::Ok);
        assert_eq!(verdict("/work"), FsVerdict::Ok, "an overlay without `volatile`");
    }

    #[test]
    fn a_fast_flush_is_explained() {
        let ext4 = mount_of(Path::new("/target"), MOUNTINFO).expect("ext4");
        let tmpfs = mount_of(Path::new("/tmp"), MOUNTINFO).expect("tmpfs");
        assert!(fsync_verdict(500_000, &ext4, None, None).starts_with("not suspicious"));
        assert!(fsync_verdict(5_000, &tmpfs, None, None).contains("does nothing"));
        assert!(fsync_verdict(5_000, &ext4, Some("write through"), None).contains("power-loss protection"));
        assert!(fsync_verdict(5_000, &ext4, Some("write back"), None).contains("volatile cache"));
        assert!(
            fsync_verdict(5_000, &ext4, None, Some("6.18.33.2-microsoft-standard-WSL2")).contains("WSL2")
        );
    }

    #[test]
    fn the_clock_check_knows_the_vdso_sources_and_the_50_ns_limit() {
        let check =
            |source: &str, read_ps| ClockCheck { source: source.into(), read_ps }.counts_for_headline();
        assert!(check("tsc", 20_000));
        assert!(check("hyperv_clocksource_tsc_page", 50_000));
        assert!(!check("tsc", 50_001));
        assert!(!check("acpi_pm", 20_000));
        let measured = ClockCheck::measure(10_000);
        assert!(measured.read_ps > 0);
    }

    #[test]
    fn the_fsync_probe_writes_syncs_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("bench-fsync-probe-{}", std::process::id()));
        let probe = fsync_probe(&dir, Duration::from_millis(30), 15_200, Duration::from_millis(1))
            .expect("the probe ran");
        assert!(probe.flushes >= 1);
        assert_eq!(probe.fdatasync_ns.count(), probe.flushes);
        assert!(!dir.join("fsync-probe.bin").exists());
        fs::remove_dir_all(&dir).expect("removed");
    }

    #[test]
    fn every_verifier_of_the_build_and_the_signing_costs_are_measured() {
        for verifier in VerifierKind::built() {
            assert!(verify_ns(verifier, 3) > 0, "{verifier}");
            assert!(recover_ns(verifier, 3) > 0, "{verifier}");
        }
        let costs = sign_costs(3);
        assert!(costs.sign_ns > 0 && costs.sha256_72_ns > 0);
        let eip712 = eip712_costs(1);
        assert!(eip712.digest_ns > 0 && eip712.keccak_64_ns > 0);
    }

    #[test]
    fn the_recovery_scaling_curve_counts_signer_checks_on_this_machines_cores() {
        let topology = Topology::read().expect("the topology");
        for verifier in VerifierKind::built() {
            let curve = recover_scaling(verifier, &topology, Duration::from_millis(50));
            assert!(!curve.is_empty(), "{verifier}");
            assert_eq!(curve[0].threads, 1);
            assert!(curve.iter().all(|point| point.per_second > 0), "{verifier}: {curve:?}");
            let mut s = Summary::new();
            put_scaling(&mut s, &format!("{}_recover", verifier.name()), &curve);
            assert_eq!(s.u64(&format!("scaling.{}_recover.0.threads", verifier.name())), Some(1));
        }
    }
}

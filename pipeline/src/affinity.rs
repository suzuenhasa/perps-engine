//! Which CPU each pipeline thread runs on, and the one system call that pins it there
//! (`docs/PIPELINE.md` 2.6 and 19.1; `docs/DECISIONS.md` D-026).
//!
//! **Contract.**
//! - [`Topology::read`] reads the machine's logical CPUs, which physical core and package
//!   each belongs to, and which ones this process may use, all from files with `std::fs`:
//!   `/sys/devices/system/cpu/cpu*/topology/{core_id, physical_package_id}` and the
//!   `Cpus_allowed_list` line of `/proc/self/status`.
//! - [`default_layout`] turns a topology into a [`CpuLayout`]: one logical CPU per thread
//!   role. The run prints it, and `--cpus role=list` overrides any part of it
//!   ([`CpuLayout::apply_override`]).
//! - [`CpuQuota`] and [`Throttling`] read the container's CPU quota and its throttling
//!   counters from the cgroup files, so the harness can refuse a layout that spins more
//!   threads than the quota allows, and discard a run the kernel throttled.
//! - [`pin_current_thread`] pins the calling thread to one CPU. It is the only `unsafe`
//!   code in the pipeline outside test binaries.
//!
//! **The default layout** is the two tables of 2.6, as rules.
//!
//! *Roomy* (PERPSBOX, 14 cores / 28 threads), when there are enough whole physical cores
//! (every sibling ours) with two siblings each, A and B, for everything but the gateways
//! (4 in signed mode, 5 in pre-verified mode). Gateways that then don't fit are an error,
//! not a reason to fall back to the compact layout:
//! - the first one (`P0`, housekeeping): A the sender, B main and the OS;
//! - the next two, or the two quietest if a jitter probe ranked them: the core and the
//!   sequencer, each alone on its physical core (the serial part of the pipeline; a busy
//!   sibling would compete for the same execution units);
//! - signed mode: one core for the gate (A) and the journal writer (B), then one core per
//!   gateway (A; the B siblings stay idle, or take more gateways with `--gateway-smt`);
//! - pre-verified mode: one core for the gate and one for the journal writer, each alone
//!   (with the journal discarded the writer spins the whole time, and a shared core could
//!   become the limit the core-path search finds).
//!
//! *Compact* (the local machine: four sibling pairs, CPU 7 kept for the recorders), when
//! there are fewer whole cores than that: main and the journal writer share the first
//! CPU (main mostly sleeps); the core takes the first CPU of the second physical core. In
//! signed mode the gate and the sequencer take those two cores' second siblings, then the
//! gateways and the sender take the remaining CPUs in order. In pre-verified mode both
//! second siblings stay idle, since the core and the writer are the busiest threads
//! there, and the sequencer, the sender and the gate take the remaining CPUs.
//!
//! Every thread stays on one CPU package (the one with the most allowed CPUs), so main can
//! pin itself there before it allocates, and every page is first touched on that package.
//!
//! **Complexity.** Everything here runs once, before a run starts.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use crate::records::InjectionMode;

// ---------------------------------------------------------------------------------------
// Pinning.

/// Pins the calling thread to one logical CPU.
///
/// The only unsafe code outside test binaries. SAFETY: `set` is a plain bitmask we zero and
/// fill ourselves (`CPU_ZERO`/`CPU_SET` from libc; an all-zero `cpu_set_t` is a valid empty
/// set, and `cpu` is checked to be inside it first); pid 0 means "this thread"; the size
/// passed is the size of the set. The call reads the set and changes only the calling
/// thread's scheduling.
#[allow(unsafe_code)]
pub fn pin_current_thread(cpu: usize) -> io::Result<()> {
    if cpu >= libc::CPU_SETSIZE as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("CPU {cpu} is beyond CPU_SETSIZE")));
    }
    // SAFETY: see the function's documentation.
    let result = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set)
    };
    if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

// ---------------------------------------------------------------------------------------
// Topology.

/// One logical CPU, as the kernel describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpu {
    pub id: usize,
    /// `topology/core_id`. Logical CPUs with the same package and core id are SMT siblings
    /// on one physical core.
    pub core_id: i32,
    /// `topology/physical_package_id`: the socket. (Some virtual machines report -1.)
    pub package: i32,
    /// In this process's `Cpus_allowed_list`.
    pub allowed: bool,
}

/// One physical core and its logical CPUs (SMT siblings), lowest id first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalCore {
    pub package: i32,
    pub core_id: i32,
    pub cpus: Vec<usize>,
    /// The siblings this process may use.
    pub allowed: Vec<usize>,
}

impl PhysicalCore {
    /// Every sibling is ours, so nothing else we know of runs on this core.
    pub fn is_whole(&self) -> bool {
        self.allowed.len() == self.cpus.len()
    }
}

/// The machine's logical CPUs, sorted by id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topology {
    cpus: Vec<Cpu>,
}

impl Topology {
    pub fn new(mut cpus: Vec<Cpu>) -> Topology {
        cpus.sort_by_key(|cpu| cpu.id);
        Topology { cpus }
    }

    /// Reads this machine's topology (module docs). CPUs that are offline have no
    /// `topology` directory and are left out.
    pub fn read() -> io::Result<Topology> {
        let allowed = allowed_cpus()?;
        let mut cpus = Vec::new();
        for entry in fs::read_dir("/sys/devices/system/cpu")? {
            let entry = entry?;
            let name = entry.file_name();
            // cpu0, cpu1, ...; not cpufreq, cpuidle and the rest.
            let Some(id) =
                name.to_str().and_then(|name| name.strip_prefix("cpu")).and_then(|n| n.parse().ok())
            else {
                continue;
            };
            let topology = entry.path().join("topology");
            let (Ok(core_id), Ok(package)) =
                (read_number(&topology.join("core_id")), read_number(&topology.join("physical_package_id")))
            else {
                continue;
            };
            cpus.push(Cpu { id, core_id, package, allowed: allowed.contains(&id) });
        }
        Ok(Topology::new(cpus))
    }

    pub fn cpus(&self) -> &[Cpu] {
        &self.cpus
    }

    /// The CPUs this process may use, in order.
    pub fn allowed(&self) -> Vec<usize> {
        self.cpus.iter().filter(|cpu| cpu.allowed).map(|cpu| cpu.id).collect()
    }

    /// The package of CPU `id`, if the topology lists it.
    pub fn package_of(&self, id: usize) -> Option<i32> {
        self.cpus.iter().find(|cpu| cpu.id == id).map(|cpu| cpu.package)
    }

    /// The package with the most allowed CPUs (the lowest id on a tie).
    pub fn busiest_package(&self) -> Option<i32> {
        let mut packages: Vec<i32> = self.cpus.iter().map(|cpu| cpu.package).collect();
        packages.sort_unstable();
        packages.dedup();
        let allowed_in =
            |package: i32| self.cpus.iter().filter(|cpu| cpu.allowed && cpu.package == package).count();
        // max_by_key returns the last maximum, so walk from the highest id down.
        packages
            .into_iter()
            .rev()
            .filter(|&package| allowed_in(package) > 0)
            .max_by_key(|&package| allowed_in(package))
    }

    /// The physical cores of `package` with at least one allowed CPU, ordered by their
    /// lowest CPU id.
    pub fn physical_cores(&self, package: i32) -> Vec<PhysicalCore> {
        let mut cores: Vec<PhysicalCore> = Vec::new();
        for cpu in self.cpus.iter().filter(|cpu| cpu.package == package) {
            let index = match cores.iter().position(|core| core.core_id == cpu.core_id) {
                Some(index) => index,
                None => {
                    cores.push(PhysicalCore {
                        package,
                        core_id: cpu.core_id,
                        cpus: Vec::new(),
                        allowed: Vec::new(),
                    });
                    cores.len() - 1
                }
            };
            cores[index].cpus.push(cpu.id);
            if cpu.allowed {
                cores[index].allowed.push(cpu.id);
            }
        }
        // The CPUs were visited in id order, so each core's lists are sorted, and so is the
        // list of cores by first CPU.
        cores.retain(|core| !core.allowed.is_empty());
        cores
    }
}

/// A whole number from a one-line sysfs file.
fn read_number(path: &Path) -> io::Result<i32> {
    let text = fs::read_to_string(path)?;
    text.trim().parse().map_err(|_| invalid_data(format!("{}: not a number: {text:?}", path.display())))
}

fn invalid_data(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// The CPUs this process may run on: the `Cpus_allowed_list` line of `/proc/self/status`.
pub fn allowed_cpus() -> io::Result<Vec<usize>> {
    let status = fs::read_to_string("/proc/self/status")?;
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .ok_or_else(|| invalid_data("no Cpus_allowed_list in /proc/self/status".to_string()))?;
    parse_cpu_list(line).map_err(invalid_data)
}

/// Parses a kernel CPU list such as `0-6,8,10-11` into the CPU ids, in the order written.
pub fn parse_cpu_list(text: &str) -> Result<Vec<usize>, String> {
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
        let number = |s: &str| s.trim().parse::<usize>().map_err(|_| format!("bad CPU list {text:?}"));
        match part.split_once('-') {
            Some((first, last)) => {
                let (first, last) = (number(first)?, number(last)?);
                if first > last {
                    return Err(format!("bad CPU range {part:?}"));
                }
                cpus.extend(first..=last);
            }
            None => cpus.push(number(part)?),
        }
    }
    Ok(cpus)
}

// ---------------------------------------------------------------------------------------
// The layout.

/// A pipeline thread, as far as pinning is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    Main,
    Sender,
    Core,
    Sequencer,
    Journal,
    Gate,
    Gateway(usize),
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Role::Main => f.write_str("main"),
            Role::Sender => f.write_str("sender"),
            Role::Core => f.write_str("core"),
            Role::Sequencer => f.write_str("sequencer"),
            Role::Journal => f.write_str("journal writer"),
            Role::Gate => f.write_str("gate"),
            Role::Gateway(g) => write!(f, "gateway {g}"),
        }
    }
}

/// The CPU each thread role pins itself to. A role that isn't listed isn't pinned, so
/// [`CpuLayout::unpinned`] (tests) pins nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuLayout {
    pins: Vec<(Role, usize)>,
}

impl CpuLayout {
    /// No thread is pinned.
    pub fn unpinned() -> CpuLayout {
        CpuLayout::default()
    }

    /// The CPU for `role`; `None` means "don't pin".
    pub fn cpu(&self, role: Role) -> Option<usize> {
        self.pins.iter().find(|(r, _)| *r == role).map(|&(_, cpu)| cpu)
    }

    /// Pins `role` to `cpu`, replacing any earlier choice for it.
    pub fn pin(&mut self, role: Role, cpu: usize) {
        match self.pins.iter_mut().find(|(r, _)| *r == role) {
            Some(pin) => pin.1 = cpu,
            None => self.pins.push((role, cpu)),
        }
    }

    /// Every pinned role and its CPU, sorted by CPU, then role.
    pub fn pins(&self) -> Vec<(Role, usize)> {
        let mut pins = self.pins.clone();
        pins.sort_by_key(|&(role, cpu)| (cpu, role));
        pins
    }

    /// Applies one `--cpus role=list` argument: `main=0`, `core=2`, `gateways=4-13`, ...
    /// The single roles (`main`, `sender`, `core`, `sequencer`, `journal`, `gate`) take one
    /// CPU; `gateways` takes one CPU per gateway, in gateway order.
    pub fn apply_override(&mut self, arg: &str) -> Result<(), String> {
        let (name, list) =
            arg.split_once('=').ok_or_else(|| format!("--cpus {arg:?}: expected role=list"))?;
        let cpus = parse_cpu_list(list)?;
        if name == "gateways" {
            for (g, &cpu) in cpus.iter().enumerate() {
                self.pin(Role::Gateway(g), cpu);
            }
            return Ok(());
        }
        let role = match name {
            "main" => Role::Main,
            "sender" => Role::Sender,
            "core" => Role::Core,
            "sequencer" => Role::Sequencer,
            "journal" => Role::Journal,
            "gate" => Role::Gate,
            _ => return Err(format!("--cpus: unknown role {name:?}")),
        };
        match cpus[..] {
            [cpu] => {
                self.pin(role, cpu);
                Ok(())
            }
            _ => Err(format!("--cpus {arg:?}: {name} takes exactly one CPU")),
        }
    }

    /// Checks that every pinned CPU is one this process may use, and that they all lie on
    /// one package (module docs).
    pub fn check(&self, topology: &Topology) -> Result<(), String> {
        let allowed = topology.allowed();
        let mut package = None;
        for (role, cpu) in self.pins() {
            if !allowed.contains(&cpu) {
                return Err(format!(
                    "{role} on CPU {cpu}, which this process may not use (allowed: {allowed:?})"
                ));
            }
            let here = topology.package_of(cpu);
            if package.is_some_and(|package| Some(package) != here) {
                return Err(format!("{role} on CPU {cpu} is on another CPU package than the others"));
            }
            package = here;
        }
        Ok(())
    }
}

/// The table the run prints: one line per CPU, with the roles on it.
impl fmt::Display for CpuLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.pins.is_empty() {
            return writeln!(f, "no thread is pinned");
        }
        let pins = self.pins();
        for (i, &(role, cpu)) in pins.iter().enumerate() {
            let first_on_cpu = i == 0 || pins[i - 1].1 != cpu;
            let last_on_cpu = i + 1 == pins.len() || pins[i + 1].1 != cpu;
            if first_on_cpu {
                write!(f, "cpu {cpu:>3}: {role}")?;
            } else {
                write!(f, ", {role}")?;
            }
            if last_on_cpu {
                writeln!(f)?;
            }
        }
        Ok(())
    }
}

/// What the default layout is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutRequest {
    pub mode: InjectionMode,
    /// Gateway threads (signed mode; ignored in pre-verified mode, which has none).
    pub gateways: usize,
    /// Let gateways take the idle siblings of the gateways' cores (`--gateway-smt`).
    pub gateway_smt: bool,
    /// Physical cores ranked by the jitter probe, quietest first, each named by any of its
    /// CPUs. The core and the sequencer take the two quietest (roomy layout only). Empty:
    /// they take the first two after `P0`.
    pub quietest_first: Vec<usize>,
}

/// Threads that spin (every one but main): each uses a whole CPU of the quota (2.6).
/// The default signed layout spins 15 (10 gateways), the pre-verified one 5.
pub fn spinning_threads(mode: InjectionMode, gateways: usize, senders: usize) -> usize {
    let pipeline = 4; // sequencer, journal writer, core, gate
    match mode {
        InjectionMode::Signed => senders + gateways + pipeline,
        InjectionMode::PreVerified => senders + pipeline,
    }
}

/// The default layout (module docs), or why there is none, in which case `--cpus` must
/// give one.
pub fn default_layout(topology: &Topology, request: &LayoutRequest) -> Result<CpuLayout, String> {
    let package = topology.busiest_package().ok_or("no CPU is allowed")?;
    let cores = topology.physical_cores(package);
    let pairs: Vec<[usize; 2]> = cores
        .iter()
        .filter(|core| core.is_whole() && core.allowed.len() >= 2)
        .map(|core| [core.allowed[0], core.allowed[1]])
        .collect();
    // The roomy layout's fixed part: P0, the core, the sequencer, and the gate and the
    // journal writer (one core in signed mode, two in pre-verified mode). A machine with
    // that many whole cores gets the roomy layout or none: gateways that don't fit are an
    // error, never a quiet move to the compact layout, which shares the core's physical core.
    let fixed = match request.mode {
        InjectionMode::Signed => 4,
        InjectionMode::PreVerified => 5,
    };
    let layout =
        if pairs.len() >= fixed { roomy_layout(pairs, request) } else { compact_layout(&cores, request) };
    layout.ok_or_else(|| {
        let allowed: usize = cores.iter().map(|core| core.allowed.len()).sum();
        format!(
            "{allowed} allowed CPUs on package {package} are too few for the default {:?} layout with {} \
             gateways{}; give --cpus",
            request.mode,
            request.gateways,
            if request.gateway_smt { " (--gateway-smt)" } else { "" },
        )
    })
}

/// The PERPSBOX table of 2.6 (module docs), on the whole two-sibling cores `pairs` (at
/// least the fixed part), or `None` if the gateways don't fit.
fn roomy_layout(mut pairs: Vec<[usize; 2]>, request: &LayoutRequest) -> Option<CpuLayout> {
    let housekeeping = pairs.remove(0);
    // The core and the sequencer: the two quietest of the rest (a stable sort keeps the
    // unranked ones in order, after the ranked ones).
    let rank = |pair: &[usize; 2]| {
        request.quietest_first.iter().position(|cpu| pair.contains(cpu)).unwrap_or(usize::MAX)
    };
    let mut by_quiet = pairs.clone();
    by_quiet.sort_by_key(rank);
    let (core, sequencer) = (by_quiet[0], by_quiet[1]);
    pairs.retain(|pair| *pair != core && *pair != sequencer);

    let mut layout = CpuLayout::unpinned();
    layout.pin(Role::Sender, housekeeping[0]);
    layout.pin(Role::Main, housekeeping[1]);
    layout.pin(Role::Core, core[0]);
    layout.pin(Role::Sequencer, sequencer[0]);
    match request.mode {
        InjectionMode::Signed => {
            let gate_and_journal = pairs[0];
            layout.pin(Role::Gate, gate_and_journal[0]);
            layout.pin(Role::Journal, gate_and_journal[1]);
            let gateway_cores = &pairs[1..];
            // One core each first; with --gateway-smt, then the idle siblings of those cores.
            let mut cpus: Vec<usize> = gateway_cores.iter().map(|pair| pair[0]).collect();
            if request.gateway_smt {
                cpus.extend(gateway_cores.iter().map(|pair| pair[1]));
            }
            if cpus.len() < request.gateways {
                return None;
            }
            for (g, &cpu) in cpus.iter().take(request.gateways).enumerate() {
                layout.pin(Role::Gateway(g), cpu);
            }
        }
        InjectionMode::PreVerified => {
            layout.pin(Role::Gate, pairs[0][0]);
            layout.pin(Role::Journal, pairs[1][0]);
        }
    }
    Some(layout)
}

/// The local table of 2.6 (module docs), or `None` if it doesn't fit either.
fn compact_layout(cores: &[PhysicalCore], request: &LayoutRequest) -> Option<CpuLayout> {
    let [first, second] = [cores.first()?, cores.get(1)?];
    if !(first.is_whole() && second.is_whole() && first.allowed.len() >= 2 && second.allowed.len() >= 2) {
        return None;
    }
    let rest: Vec<usize> = cores[2..].iter().flat_map(|core| core.allowed.iter().copied()).collect();
    let mut layout = CpuLayout::unpinned();
    layout.pin(Role::Main, first.allowed[0]);
    layout.pin(Role::Journal, first.allowed[0]);
    layout.pin(Role::Core, second.allowed[0]);
    match request.mode {
        InjectionMode::Signed => {
            layout.pin(Role::Gate, first.allowed[1]);
            layout.pin(Role::Sequencer, second.allowed[1]);
            if rest.len() < request.gateways + 1 {
                return None;
            }
            for (g, &cpu) in rest.iter().take(request.gateways).enumerate() {
                layout.pin(Role::Gateway(g), cpu);
            }
            layout.pin(Role::Sender, rest[request.gateways]);
        }
        InjectionMode::PreVerified => {
            let [sequencer, sender, gate, ..] = rest[..] else { return None };
            layout.pin(Role::Sequencer, sequencer);
            layout.pin(Role::Sender, sender);
            layout.pin(Role::Gate, gate);
        }
    }
    Some(layout)
}

// ---------------------------------------------------------------------------------------
// The container's CPU quota and throttling (cgroup v2, else v1).

/// The CPU time the container may use per period. vast.ai boxes have one (about 26.9 CPUs
/// when probed); going over it makes the kernel throttle the whole container for the rest
/// of a 100 ms period (2.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuQuota {
    /// No quota: `max` in cgroup v2, `-1` in v1, or no cgroup CPU files at all.
    Unlimited,
    Limited {
        quota_us: u64,
        period_us: u64,
    },
}

/// Where cgroup v1 mounts the CPU controller, depending on the distribution.
const CGROUP_V1_CPU_DIRS: [&str; 2] = ["/sys/fs/cgroup/cpu", "/sys/fs/cgroup/cpu,cpuacct"];

impl CpuQuota {
    /// Reads the quota: `/sys/fs/cgroup/cpu.max` (v2), else `cpu.cfs_quota_us` and
    /// `cpu.cfs_period_us` (v1).
    pub fn read() -> io::Result<CpuQuota> {
        if let Some(text) = read_if_present(Path::new("/sys/fs/cgroup/cpu.max"))? {
            return CpuQuota::parse_v2(&text).ok_or_else(|| invalid_data(format!("cpu.max: {text:?}")));
        }
        for dir in CGROUP_V1_CPU_DIRS {
            let dir = Path::new(dir);
            let Some(quota) = read_if_present(&dir.join("cpu.cfs_quota_us"))? else { continue };
            let period = fs::read_to_string(dir.join("cpu.cfs_period_us"))?;
            return CpuQuota::parse_v1(&quota, &period).ok_or_else(|| {
                invalid_data(format!("cpu.cfs_quota_us {quota:?}, cpu.cfs_period_us {period:?}"))
            });
        }
        Ok(CpuQuota::Unlimited)
    }

    /// `cpu.max`: `max 100000`, or `2690000 100000`.
    fn parse_v2(text: &str) -> Option<CpuQuota> {
        let (quota, period) = text.trim().split_once(' ')?;
        if quota == "max" {
            return Some(CpuQuota::Unlimited);
        }
        Some(CpuQuota::Limited { quota_us: quota.parse().ok()?, period_us: period.trim().parse().ok()? })
    }

    /// `cpu.cfs_quota_us` (`-1` for none) and `cpu.cfs_period_us`.
    fn parse_v1(quota: &str, period: &str) -> Option<CpuQuota> {
        let quota: i64 = quota.trim().parse().ok()?;
        if quota < 0 {
            return Some(CpuQuota::Unlimited);
        }
        Some(CpuQuota::Limited { quota_us: quota as u64, period_us: period.trim().parse().ok()? })
    }

    /// The most spinning threads a layout may have: `floor(quota) - 2`, one CPU for main
    /// and one for kernel work done on the container's behalf. `None` if unlimited.
    pub fn max_spinning_threads(self) -> Option<usize> {
        match self {
            CpuQuota::Unlimited => None,
            CpuQuota::Limited { quota_us, period_us } => {
                Some(usize::try_from((quota_us / period_us.max(1)).saturating_sub(2)).unwrap_or(usize::MAX))
            }
        }
    }

    /// Refuses a run that would spin more threads than the quota allows (2.6).
    pub fn check(self, spinning: usize) -> Result<(), String> {
        match self.max_spinning_threads() {
            Some(max) if spinning > max => Err(format!(
                "{spinning} spinning threads exceed floor(CPU quota) - 2 = {max} ({self:?}); the kernel would throttle the run"
            )),
            _ => Ok(()),
        }
    }
}

/// The container's throttling counters, read before and after every run: a run during
/// which either moved is discarded (2.6, 15.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Throttling {
    /// Periods in which the container was throttled.
    pub nr_throttled: u64,
    /// Total time throttled, in microseconds.
    pub throttled_us: u64,
}

impl Throttling {
    /// Reads `cpu.stat` (v2, else v1). `None` if there is no such file (no quota to throttle
    /// against).
    pub fn read() -> io::Result<Option<Throttling>> {
        let v1 = CGROUP_V1_CPU_DIRS.map(|dir| Path::new(dir).join("cpu.stat"));
        for path in std::iter::once(Path::new("/sys/fs/cgroup/cpu.stat").to_path_buf()).chain(v1) {
            if let Some(text) = read_if_present(&path)? {
                return Ok(Some(Throttling::parse(&text)));
            }
        }
        Ok(None)
    }

    /// `nr_throttled` and `throttled_usec` (v2), or `throttled_time` in nanoseconds (v1).
    /// A missing key reads as 0.
    fn parse(text: &str) -> Throttling {
        let mut throttling = Throttling::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once(' ') else { continue };
            let Ok(value) = value.trim().parse::<u64>() else { continue };
            match key {
                "nr_throttled" => throttling.nr_throttled = value,
                "throttled_usec" => throttling.throttled_us = value,
                "throttled_time" => throttling.throttled_us = value / 1_000,
                _ => {}
            }
        }
        throttling
    }
}

/// A file's contents, or `None` if it doesn't exist.
fn read_if_present(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PERPSBOX as probed: one package, 14 cores, siblings `i` and `i + 14`, all allowed.
    fn perpsbox() -> Topology {
        let cpus =
            (0..28).map(|id| Cpu { id, core_id: (id % 14) as i32, package: 0, allowed: true }).collect();
        Topology::new(cpus)
    }

    /// The local machine under `./dev`: sibling pairs (0,1) (2,3) (4,5) (6,7), CPUs 0 to 6
    /// allowed (7 is the recorders').
    fn local() -> Topology {
        let cpus =
            (0..8).map(|id| Cpu { id, core_id: (id / 2) as i32, package: 0, allowed: id < 7 }).collect();
        Topology::new(cpus)
    }

    fn request(mode: InjectionMode, gateways: usize) -> LayoutRequest {
        LayoutRequest { mode, gateways, gateway_smt: false, quietest_first: Vec::new() }
    }

    fn roles_on(layout: &CpuLayout, cpu: usize) -> Vec<Role> {
        layout.pins().into_iter().filter(|&(_, c)| c == cpu).map(|(role, _)| role).collect()
    }

    #[test]
    fn cpu_lists_parse_like_the_kernel_writes_them() {
        assert_eq!(parse_cpu_list("0-6"), Ok(vec![0, 1, 2, 3, 4, 5, 6]));
        assert_eq!(parse_cpu_list("0,2,4-5,7\n"), Ok(vec![0, 2, 4, 5, 7]));
        assert_eq!(parse_cpu_list("\t3"), Ok(vec![3]));
        assert_eq!(parse_cpu_list(""), Ok(vec![]));
        assert!(parse_cpu_list("5-3").is_err());
        assert!(parse_cpu_list("a").is_err());
        assert!(parse_cpu_list("1-").is_err());
    }

    #[test]
    fn physical_cores_group_siblings() {
        let cores = local().physical_cores(0);
        assert_eq!(cores.len(), 4);
        assert_eq!((cores[0].cpus.clone(), cores[0].is_whole()), (vec![0, 1], true));
        assert_eq!(
            (cores[3].cpus.clone(), cores[3].allowed.clone(), cores[3].is_whole()),
            (vec![6, 7], vec![6], false)
        );
        let cores = perpsbox().physical_cores(0);
        assert_eq!(cores.len(), 14);
        assert_eq!(cores[1].cpus, vec![1, 15]);
        assert_eq!(cores[13].cpus, vec![13, 27]);
    }

    #[test]
    fn the_perpsbox_signed_layout_is_the_specs_table() {
        let layout = default_layout(&perpsbox(), &request(InjectionMode::Signed, 10)).expect("fits");
        assert_eq!(layout.cpu(Role::Sender), Some(0));
        assert_eq!(layout.cpu(Role::Main), Some(14));
        assert_eq!(layout.cpu(Role::Core), Some(1));
        assert_eq!(layout.cpu(Role::Sequencer), Some(2));
        assert_eq!(layout.cpu(Role::Gate), Some(3));
        assert_eq!(layout.cpu(Role::Journal), Some(17));
        for g in 0..10 {
            assert_eq!(layout.cpu(Role::Gateway(g)), Some(4 + g), "gateway {g}");
        }
        // The core's and the sequencer's siblings stay idle.
        assert!(roles_on(&layout, 15).is_empty() && roles_on(&layout, 16).is_empty());
        assert_eq!(layout.pins().len(), 16);
        assert_eq!(spinning_threads(InjectionMode::Signed, 10, 1), 15);
        assert_eq!(layout.check(&perpsbox()), Ok(()));
    }

    #[test]
    fn the_perpsbox_pre_verified_layout_is_the_specs_table() {
        let layout = default_layout(&perpsbox(), &request(InjectionMode::PreVerified, 10)).expect("fits");
        let expected = [
            (Role::Sender, 0),
            (Role::Core, 1),
            (Role::Sequencer, 2),
            (Role::Gate, 3),
            (Role::Journal, 4),
            (Role::Main, 14),
        ];
        assert_eq!(layout.pins(), expected);
        assert_eq!(spinning_threads(InjectionMode::PreVerified, 10, 1), 5);
    }

    #[test]
    fn gateway_smt_fills_the_gateway_cores_idle_siblings() {
        let mut smt = request(InjectionMode::Signed, 19);
        smt.gateway_smt = true;
        let layout = default_layout(&perpsbox(), &smt).expect("fits");
        for g in 0..10 {
            assert_eq!(layout.cpu(Role::Gateway(g)), Some(4 + g), "gateway {g}");
        }
        for g in 10..19 {
            assert_eq!(layout.cpu(Role::Gateway(g)), Some(18 + g - 10), "gateway {g}");
        }
        // 19 gateways at 26.9 CPUs is 24 spinning threads: exactly the quota's limit.
        let quota = CpuQuota::Limited { quota_us: 2_690_000, period_us: 100_000 };
        assert_eq!(quota.max_spinning_threads(), Some(24));
        assert_eq!(quota.check(spinning_threads(InjectionMode::Signed, 19, 1)), Ok(()));
        assert!(quota.check(spinning_threads(InjectionMode::Signed, 20, 1)).is_err());
        // Without --gateway-smt, 19 gateways don't fit on 14 cores.
        assert!(default_layout(&perpsbox(), &request(InjectionMode::Signed, 19)).is_err());
    }

    #[test]
    fn the_core_and_the_sequencer_take_the_two_quietest_cores() {
        let mut quiet = request(InjectionMode::Signed, 10);
        quiet.quietest_first = vec![20, 9]; // physical cores P6 (6, 20) and P9 (9, 23)
        let layout = default_layout(&perpsbox(), &quiet).expect("fits");
        assert_eq!(layout.cpu(Role::Core), Some(6));
        assert_eq!(layout.cpu(Role::Sequencer), Some(9));
        // The others keep their order on the remaining cores.
        assert_eq!((layout.cpu(Role::Gate), layout.cpu(Role::Journal)), (Some(1), Some(15)));
        let gateways: Vec<usize> = (0..10).filter_map(|g| layout.cpu(Role::Gateway(g))).collect();
        assert_eq!(gateways, [2, 3, 4, 5, 7, 8, 10, 11, 12, 13]);
    }

    #[test]
    fn the_local_layouts_are_the_specs_table() {
        let signed = default_layout(&local(), &request(InjectionMode::Signed, 2)).expect("fits");
        assert_eq!(roles_on(&signed, 0), [Role::Main, Role::Journal]);
        assert_eq!(roles_on(&signed, 1), [Role::Gate]);
        assert_eq!(roles_on(&signed, 2), [Role::Core]);
        assert_eq!(roles_on(&signed, 3), [Role::Sequencer]);
        assert_eq!(roles_on(&signed, 4), [Role::Gateway(0)]);
        assert_eq!(roles_on(&signed, 5), [Role::Gateway(1)]);
        assert_eq!(roles_on(&signed, 6), [Role::Sender]);
        assert!(roles_on(&signed, 7).is_empty());

        let pre_verified = default_layout(&local(), &request(InjectionMode::PreVerified, 2)).expect("fits");
        assert_eq!(roles_on(&pre_verified, 0), [Role::Main, Role::Journal]);
        assert!(roles_on(&pre_verified, 1).is_empty(), "the writer's sibling is idle");
        assert_eq!(roles_on(&pre_verified, 2), [Role::Core]);
        assert!(roles_on(&pre_verified, 3).is_empty(), "the core's sibling is idle");
        assert_eq!(roles_on(&pre_verified, 4), [Role::Sequencer]);
        assert_eq!(roles_on(&pre_verified, 5), [Role::Sender]);
        assert_eq!(roles_on(&pre_verified, 6), [Role::Gate]);
        assert_eq!(pre_verified.check(&local()), Ok(()));

        assert_eq!(
            signed.to_string(),
            "cpu   0: main, journal writer\ncpu   1: gate\ncpu   2: core\ncpu   3: sequencer\n\
             cpu   4: gateway 0\ncpu   5: gateway 1\ncpu   6: sender\n"
        );
    }

    #[test]
    fn too_few_cpus_is_an_error_that_asks_for_cpus() {
        let error =
            default_layout(&local(), &request(InjectionMode::Signed, 3)).expect_err("3 gateways don't fit");
        assert!(error.contains("--cpus"), "{error}");
        let tiny = Topology::new(vec![Cpu { id: 0, core_id: 0, package: 0, allowed: true }]);
        assert!(default_layout(&tiny, &request(InjectionMode::PreVerified, 0)).is_err());
        let none = Topology::new(vec![Cpu { id: 0, core_id: 0, package: 0, allowed: false }]);
        assert_eq!(
            default_layout(&none, &request(InjectionMode::PreVerified, 0)),
            Err("no CPU is allowed".into())
        );
    }

    #[test]
    fn the_layout_stays_on_the_package_with_the_most_allowed_cpus() {
        // Two packages of 8 threads (4 sibling pairs); CPUs 0 and 1 are not ours, so
        // package 1 has more allowed CPUs.
        let cpus = (0..16)
            .map(|id| Cpu { id, core_id: (id % 8 / 2) as i32, package: (id / 8) as i32, allowed: id >= 2 })
            .collect();
        let topology = Topology::new(cpus);
        assert_eq!(topology.busiest_package(), Some(1));
        let mut layout = default_layout(&topology, &request(InjectionMode::PreVerified, 0)).expect("fits");
        assert!(layout.pins().iter().all(|&(_, cpu)| topology.package_of(cpu) == Some(1)), "{layout}");
        assert_eq!(layout.check(&topology), Ok(()));
        layout.pin(Role::Gate, 2);
        assert!(layout.check(&topology).expect_err("two packages").contains("another CPU package"));
        layout.pin(Role::Gate, 1);
        assert!(layout.check(&topology).expect_err("not allowed").contains("may not use"));
    }

    #[test]
    fn overrides_replace_single_roles_and_list_gateways() {
        let mut layout = default_layout(&local(), &request(InjectionMode::Signed, 2)).expect("fits");
        layout.apply_override("core=5").expect("valid");
        layout.apply_override("gateways=2,6").expect("valid");
        assert_eq!(layout.cpu(Role::Core), Some(5));
        assert_eq!((layout.cpu(Role::Gateway(0)), layout.cpu(Role::Gateway(1))), (Some(2), Some(6)));
        assert!(layout.apply_override("core=1-2").is_err());
        assert!(layout.apply_override("cook=1").is_err());
        assert!(layout.apply_override("core").is_err());
        let mut unpinned = CpuLayout::unpinned();
        assert_eq!(unpinned.cpu(Role::Core), None);
        assert_eq!(unpinned.to_string(), "no thread is pinned\n");
        unpinned.apply_override("journal=3").expect("valid");
        assert_eq!(unpinned.pins(), [(Role::Journal, 3)]);
    }

    #[test]
    fn quotas_parse_for_both_cgroup_versions() {
        assert_eq!(CpuQuota::parse_v2("max 100000\n"), Some(CpuQuota::Unlimited));
        assert_eq!(
            CpuQuota::parse_v2("2690000 100000\n"),
            Some(CpuQuota::Limited { quota_us: 2_690_000, period_us: 100_000 })
        );
        assert_eq!(CpuQuota::parse_v2("garbage"), None);
        assert_eq!(CpuQuota::parse_v1("-1\n", "100000\n"), Some(CpuQuota::Unlimited));
        assert_eq!(
            CpuQuota::parse_v1("250000\n", "100000\n"),
            Some(CpuQuota::Limited { quota_us: 250_000, period_us: 100_000 })
        );
        assert_eq!(
            CpuQuota::Limited { quota_us: 250_000, period_us: 100_000 }.max_spinning_threads(),
            Some(0)
        );
        assert_eq!(
            CpuQuota::Limited { quota_us: 100_000, period_us: 100_000 }.max_spinning_threads(),
            Some(0)
        );
        assert_eq!(CpuQuota::Unlimited.max_spinning_threads(), None);
        assert_eq!(CpuQuota::Unlimited.check(1_000), Ok(()));
    }

    #[test]
    fn throttling_parses_for_both_cgroup_versions() {
        let v2 = "usage_usec 100\nuser_usec 60\nsystem_usec 40\nnr_periods 50\nnr_throttled 3\nthrottled_usec 4500\n";
        assert_eq!(Throttling::parse(v2), Throttling { nr_throttled: 3, throttled_us: 4_500 });
        let v1 = "nr_periods 50\nnr_throttled 2\nthrottled_time 7000000\n";
        assert_eq!(Throttling::parse(v1), Throttling { nr_throttled: 2, throttled_us: 7_000 });
        assert_eq!(Throttling::parse(""), Throttling::default());
    }

    #[test]
    fn this_machine_can_be_read() {
        let topology = Topology::read().expect("sysfs and /proc are readable");
        let allowed = allowed_cpus().expect("/proc/self/status is readable");
        assert!(!allowed.is_empty());
        assert_eq!(topology.allowed(), allowed, "every allowed CPU has a topology");
        CpuQuota::read().expect("the cgroup files, if any, parse");
        Throttling::read().expect("the cgroup files, if any, parse");
    }

    #[test]
    fn a_thread_pins_itself_to_an_allowed_cpu() {
        let allowed = allowed_cpus().expect("readable");
        let cpu = *allowed.last().expect("at least one CPU");
        let seen = std::thread::spawn(move || {
            pin_current_thread(cpu).expect("an allowed CPU");
            let status = fs::read_to_string("/proc/thread-self/status").expect("readable");
            let line =
                status.lines().find_map(|line| line.strip_prefix("Cpus_allowed_list:")).expect("listed");
            parse_cpu_list(line).expect("a CPU list")
        })
        .join()
        .expect("the thread ends cleanly");
        assert_eq!(seen, [cpu]);
    }

    #[test]
    fn pinning_to_an_impossible_cpu_is_an_error() {
        let error = pin_current_thread(1 << 20).expect_err("beyond CPU_SETSIZE");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}

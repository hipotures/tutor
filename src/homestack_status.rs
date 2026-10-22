//! HomeStack service telemetry served at GET /.well-known/homestack/status,
//! see .agents/skills/homestack-service/service-status.md.
//!
//! One background thread samples every five seconds and HTTP handlers only clone
//! the latest sample, so a stalled network filesystem cannot block a request.
//! Resource figures come from the service's own cgroup v2. /proc/loadavg is not
//! used: inside a Proxmox LXC container it is the host's load.

// Counter deltas and CPU counts stay far below the 2^52 where f64 loses integers
#![allow(clippy::cast_precision_loss)]

use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{Arc, PoisonError, RwLock},
    time::{Duration, Instant},
};

const MAX_FILESYSTEMS: usize = 128;
const INTERVAL: Duration = Duration::from_secs(5);
// Rates over shorter intervals would mostly measure sampling jitter
const MIN_RATE_INTERVAL: Duration = Duration::from_millis(500);
// cgroup interface files are small; io.stat grows by one line per device
const MAX_CGROUP_FILE: u64 = 64 * 1024;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
}

#[derive(Serialize)]
pub struct Report {
    schema: u32,
    /// Process liveness, independent of dependency readiness
    status: Status,
    uptime_seconds: u64,
    sample_age_seconds: Option<u64>,
    /// Always null: /proc/loadavg is not container-scoped, see `container.pressure`
    load_average: Option<()>,
    filesystems: Vec<Filesystem>,
    container: Container,
    /// A stable error code, never a raw OS error
    error: Option<&'static str>,
}

#[derive(Clone, Serialize)]
struct Filesystem {
    mount: String,
    filesystem: Option<String>,
    /// Whether mountinfo confirms this exact path as a mount point
    mounted: bool,
    total_bytes: Option<u64>,
    used_bytes: Option<u64>,
    free_bytes: Option<u64>,
    /// Free space available to an unprivileged user
    available_bytes: Option<u64>,
    error: Option<&'static str>,
}

/// Resource usage of the cgroup this process runs in
#[derive(Clone, Default, Serialize)]
struct Container {
    cpu: Option<Cpu>,
    memory: Option<Memory>,
    pressure: Pressure,
    io: Option<Io>,
    error: Option<&'static str>,
}

#[derive(Clone, Serialize)]
struct Cpu {
    /// Logical CPUs the cgroup may use: its cpuset, lowered by any visible cpu.max quota
    capacity_cpus: Option<f64>,
    /// The fields below cover the last interval and are null until two samples exist
    interval_seconds: Option<f64>,
    /// 100 means one logical CPU fully busy
    usage_percent: Option<f64>,
    /// 100 means all of `capacity_cpus` fully busy
    capacity_percent: Option<f64>,
    user_percent: Option<f64>,
    system_percent: Option<f64>,
    /// Null when the cpu controller is not enabled for the cgroup
    throttling: Option<Throttling>,
}

#[derive(Clone, Serialize)]
struct Throttling {
    periods: u64,
    throttled_periods: u64,
    throttled_seconds: f64,
}

#[derive(Clone, Serialize)]
struct Memory {
    /// memory.current: everything charged to the cgroup, page cache included
    usage_bytes: u64,
    /// Lowest visible memory.max; null when unlimited or set outside the visible hierarchy
    limit_bytes: Option<u64>,
    /// Anonymous memory of all processes in the cgroup
    anon_bytes: Option<u64>,
    /// Page cache, mostly reclaimable
    file_bytes: Option<u64>,
    /// Resident set of this application process alone
    process_rss_bytes: Option<u64>,
    /// Cumulative memory.events counters
    events: Option<MemoryEvents>,
}

#[derive(Clone, Serialize)]
struct MemoryEvents {
    high: u64,
    max: u64,
    oom: u64,
    oom_kill: u64,
}

/// Pressure stall information: time stalled on contention, not utilization.
/// A resource is null when the kernel does not provide PSI for it.
#[derive(Clone, Default, Serialize)]
struct Pressure {
    cpu: Option<Psi>,
    memory: Option<Psi>,
    io: Option<Psi>,
}

#[derive(Clone, Serialize)]
struct Psi {
    some: PsiLine,
    full: Option<PsiLine>,
}

#[derive(Clone, Serialize)]
struct PsiLine {
    /// Percent of wall time over the last 10, 60 and 300 seconds
    avg10: f64,
    avg60: f64,
    avg300: f64,
    total_usec: u64,
}

/// Block I/O rates from io.stat, summed over devices, over the last interval
#[derive(Clone, Serialize)]
struct Io {
    interval_seconds: f64,
    read_bytes_per_second: f64,
    write_bytes_per_second: f64,
    read_ops_per_second: f64,
    write_ops_per_second: f64,
}

struct Sample {
    at: Option<Instant>,
    filesystems: Vec<Filesystem>,
    container: Container,
    error: Option<&'static str>,
}

/// Initialize once during startup and share through Rocket's managed state
pub struct Sampler {
    started: Instant,
    sample: Arc<RwLock<Sample>>,
}

impl Sampler {
    pub fn start() -> std::io::Result<Self> {
        let sample = Arc::new(RwLock::new(Sample {
            at: None,
            filesystems: Vec::new(),
            container: Container::default(),
            error: Some("pending"),
        }));
        let weak = Arc::downgrade(&sample);
        std::thread::Builder::new().name("homestack-status".to_owned()).spawn(move || {
            // Counters from the previous sample, kept for rates
            let mut state = State::default();
            loop {
                if weak.strong_count() == 0 {
                    break;
                }
                // Do not hold a strong reference or the write lock across OS I/O
                let next = collect(&mut state);
                let Some(shared) = weak.upgrade() else {
                    break;
                };
                *shared.write().unwrap_or_else(PoisonError::into_inner) = next;
                drop(shared);
                std::thread::sleep(INTERVAL);
            }
        })?;
        Ok(Self {
            started: Instant::now(),
            sample,
        })
    }

    pub fn report(&self, status: Status) -> Report {
        let sample = self.sample.read().unwrap_or_else(PoisonError::into_inner);
        Report {
            schema: 1,
            status,
            uptime_seconds: self.started.elapsed().as_secs(),
            sample_age_seconds: sample.at.map(|at| at.elapsed().as_secs()),
            load_average: None,
            filesystems: sample.filesystems.clone(),
            container: sample.container.clone(),
            error: sample.error,
        }
    }
}

#[derive(Default)]
struct State {
    cgroup: Option<Cgroup>,
    cpu: Option<(Instant, CpuStat)>,
    io: Option<(Instant, IoStat)>,
}

fn bounded_file(path: impl AsRef<Path>, limit: u64) -> std::io::Result<String> {
    let mut text = String::new();
    File::open(path)?.take(limit + 1).read_to_string(&mut text)?;
    if text.len() as u64 > limit {
        return Err(std::io::Error::other("measurement exceeds limit"));
    }
    Ok(text)
}

fn unescape(value: &str) -> Option<String> {
    let mut result = Vec::new();
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        result.push(if byte == b'\\' {
            match (bytes.next(), bytes.next(), bytes.next()) {
                (Some(b'0'), Some(b'4'), Some(b'0')) => b' ',
                (Some(b'0'), Some(b'1'), Some(b'1')) => b'\t',
                (Some(b'0'), Some(b'1'), Some(b'2')) => b'\n',
                (Some(b'1'), Some(b'3'), Some(b'4')) => b'\\',
                _ => return None,
            }
        } else {
            byte
        });
    }
    String::from_utf8(result).ok()
}

/// Mount root, mount point and filesystem type of each mountinfo line
fn mountinfo_entries(text: &str) -> impl Iterator<Item = (String, String, &str)> {
    text.lines().filter_map(|line| {
        let (left, right) = line.split_once(" - ")?;
        let mut fields = left.split_whitespace().skip(3);
        let root = unescape(fields.next()?)?;
        let path = unescape(fields.next()?)?;
        Some((root, path, right.split_whitespace().next()?))
    })
}

fn data_mount(path: &str, kind: &str) -> bool {
    if path == "/" || path == "/home/app" {
        return true;
    }
    if ["/proc", "/sys", "/dev", "/run"].iter().any(|base| path == *base || path.starts_with(&format!("{base}/"))) {
        return false;
    }
    !matches!(
        kind,
        "proc"
            | "sysfs"
            | "devtmpfs"
            | "devpts"
            | "tmpfs"
            | "cgroup"
            | "cgroup2"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "pstore"
            | "mqueue"
            | "hugetlbfs"
            | "configfs"
            | "fusectl"
            | "autofs"
    )
}

fn collect(state: &mut State) -> Sample {
    // Timestamp the beginning: an NFS call that eventually returns must not
    // make old measurements appear fresh
    let at = Instant::now();
    let mountinfo = bounded_file("/proc/self/mountinfo", 1024 * 1024).ok();
    let (filesystems, error) = filesystems(mountinfo.as_deref());
    // Resolved once and again only when the cgroup stops answering, e.g. after a migration
    if state.cgroup.as_ref().is_none_or(|cgroup| !cgroup.dir.join("cpu.stat").exists()) {
        state.cgroup = None;
        state.cpu = None;
        state.io = None;
    }
    let resolved = match mountinfo.as_deref() {
        _ if state.cgroup.is_some() => Ok(()),
        Some(mountinfo) => Cgroup::resolve(mountinfo).map(|cgroup| state.cgroup = Some(cgroup)),
        None => Err("cgroup_unavailable"),
    };
    let container = resolved.map_or_else(
        |error| Container {
            error: Some(error),
            ..Container::default()
        },
        |()| container(state),
    );
    Sample {
        at: Some(at),
        filesystems,
        container,
        error,
    }
}

fn filesystems(mountinfo: Option<&str>) -> (Vec<Filesystem>, Option<&'static str>) {
    let mut mounts = BTreeMap::new();
    let mut text_bytes = 0;
    let mut error = None;
    match mountinfo {
        Some(text) => {
            for (_, path, kind) in mountinfo_entries(text) {
                if data_mount(&path, kind) && std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                    text_bytes += path.len() + kind.len();
                    // Leave room for the two required entries and JSON fields
                    if mounts.len() >= MAX_FILESYSTEMS - 2 || text_bytes > 4 * 1024 {
                        error = Some("too_many_mounts");
                        break;
                    }
                    // Later stacked mounts are the visible ones
                    mounts.insert(path, kind.to_owned());
                }
            }
        }
        None => error = Some("mount_inventory_unavailable"),
    }
    let mut filesystems: Vec<Filesystem> = mounts.iter().map(|(path, kind)| measure(path, kind)).collect();
    // Never silently report the root's free space as /home/app's free space
    for required in ["/", "/home/app"] {
        if !mounts.contains_key(required) {
            filesystems.push(Filesystem {
                mount: required.to_owned(),
                filesystem: None,
                mounted: false,
                total_bytes: None,
                used_bytes: None,
                free_bytes: None,
                available_bytes: None,
                error: Some(if error.is_some() {
                    "mount_inventory_unavailable"
                } else {
                    "not_mounted"
                }),
            });
        }
    }
    filesystems.sort_by(|a, b| a.mount.cmp(&b.mount));
    (filesystems, error)
}

fn measure(path: &str, kind: &str) -> Filesystem {
    let mut result = Filesystem {
        mount: path.to_owned(),
        filesystem: Some(kind.to_owned()),
        mounted: true,
        total_bytes: None,
        used_bytes: None,
        free_bytes: None,
        available_bytes: None,
        error: None,
    };
    match rustix::fs::statvfs(path) {
        Ok(stat) => {
            let unit = stat.f_frsize;
            let sizes = (|| {
                Some((
                    stat.f_blocks.checked_mul(unit)?,
                    stat.f_blocks.checked_sub(stat.f_bfree)?.checked_mul(unit)?,
                    stat.f_bfree.checked_mul(unit)?,
                    stat.f_bavail.checked_mul(unit)?,
                ))
            })();
            if let Some((total, used, free, available)) =
                sizes.filter(|(total, _, free, available)| unit > 0 && *free <= *total && *available <= *free)
            {
                result.total_bytes = Some(total);
                result.used_bytes = Some(used);
                result.free_bytes = Some(free);
                result.available_bytes = Some(available);
            } else {
                result.error = Some("invalid_statistics");
            }
        }
        Err(_) => result.error = Some("measurement_unavailable"),
    }
    result
}

/// The process's cgroup v2 directory, below the cgroup2 mount point `top`
struct Cgroup {
    top: PathBuf,
    dir: PathBuf,
}

impl Cgroup {
    fn resolve(mountinfo: &str) -> Result<Self, &'static str> {
        let membership = bounded_file("/proc/self/cgroup", MAX_CGROUP_FILE).map_err(|_| "cgroup_unavailable")?;
        Self::from_text(&membership, mountinfo)
    }

    /// `membership` is /proc/self/cgroup. Its unified "0::" path is relative to the
    /// cgroup namespace root, as is the root of a cgroup2 mount made inside the
    /// namespace: a Proxmox LXC container sees its own cgroup as "/" there.
    fn from_text(membership: &str, mountinfo: &str) -> Result<Self, &'static str> {
        let path = membership.lines().find_map(|line| line.strip_prefix("0::")).ok_or("cgroup_unavailable")?;
        let (top, relative) = mountinfo_entries(mountinfo)
            .filter(|(_, _, kind)| *kind == "cgroup2")
            .find_map(|(root, mount, _)| {
                // A path outside the mount (e.g. "/../sibling") has no visible directory
                let relative = Path::new(path).strip_prefix(&root).ok()?;
                relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
                    .then(|| (PathBuf::from(mount), relative.to_path_buf()))
            })
            .ok_or("cgroup_unavailable")?;
        let dir = top.join(relative);
        // The real hierarchy root has no cgroup.type: its counters cover the whole host
        if !dir.join("cgroup.type").exists() {
            return Err("host_cgroup");
        }
        Ok(Self {
            top,
            dir,
        })
    }

    fn read(&self, file: &str) -> Option<String> {
        bounded_file(self.dir.join(file), MAX_CGROUP_FILE).ok()
    }

    /// The cgroup and its ancestors up to the visible root
    fn ancestors(&self) -> impl Iterator<Item = &Path> {
        self.dir.ancestors().take_while(|dir| dir.starts_with(&self.top))
    }
}

fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// "key value" lines, as in cpu.stat, memory.stat and memory.events
fn flat_keyed(text: &str) -> BTreeMap<&str, u64> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(' ')?;
            Some((key, value.trim().parse().ok()?))
        })
        .collect()
}

#[derive(Clone, Copy)]
struct CpuStat {
    usage_usec: u64,
    user_usec: u64,
    system_usec: u64,
    // Present only with the cpu controller enabled
    throttling: Option<(u64, u64, u64)>,
}

impl CpuStat {
    fn parse(text: &str) -> Option<Self> {
        let fields = flat_keyed(text);
        Some(Self {
            usage_usec: *fields.get("usage_usec")?,
            user_usec: *fields.get("user_usec")?,
            system_usec: *fields.get("system_usec")?,
            throttling: (|| {
                Some((*fields.get("nr_periods")?, *fields.get("nr_throttled")?, *fields.get("throttled_usec")?))
            })(),
        })
    }
}

#[derive(Clone, Copy)]
struct IoStat {
    read_bytes: u64,
    write_bytes: u64,
    read_ops: u64,
    write_ops: u64,
}

impl IoStat {
    /// "MAJ:MIN rbytes=… wbytes=… rios=… wios=… dbytes=… dios=…" per device
    fn parse(text: &str) -> Option<Self> {
        let mut total = Self {
            read_bytes: 0,
            write_bytes: 0,
            read_ops: 0,
            write_ops: 0,
        };
        for (key, value) in
            text.lines().flat_map(|line| line.split_whitespace().skip(1)).filter_map(|f| f.split_once('='))
        {
            let counter = match key {
                "rbytes" => &mut total.read_bytes,
                "wbytes" => &mut total.write_bytes,
                "rios" => &mut total.read_ops,
                "wios" => &mut total.write_ops,
                _ => continue,
            };
            *counter = counter.checked_add(value.parse().ok()?)?;
        }
        Some(total)
    }
}

/// Elapsed time since the previous counters, when long enough for a rate
fn interval(previous: Option<&(Instant, impl Copy)>, now: Instant) -> Option<f64> {
    let elapsed = now.checked_duration_since(previous?.0)?;
    (elapsed >= MIN_RATE_INTERVAL).then_some(elapsed.as_secs_f64())
}

fn container(state: &mut State) -> Container {
    let Some(cgroup) = &state.cgroup else {
        return Container::default();
    };
    let now = Instant::now();

    let cpu = cgroup.read("cpu.stat").and_then(|text| CpuStat::parse(&text)).map(|stat| {
        let previous = state.cpu;
        let seconds = interval(previous.as_ref(), now);
        // A decreasing counter means a reset: report nothing for this interval
        let rates = seconds.and_then(|seconds| {
            let old = previous?.1;
            let percent =
                |new: u64, old: u64| Some(round(new.checked_sub(old)? as f64 / (seconds * 1_000_000.0) * 100.0));
            Some((
                percent(stat.usage_usec, old.usage_usec)?,
                percent(stat.user_usec, old.user_usec)?,
                percent(stat.system_usec, old.system_usec)?,
                stat.throttling.zip(old.throttling).and_then(|(new, old)| {
                    Some(Throttling {
                        periods: new.0.checked_sub(old.0)?,
                        throttled_periods: new.1.checked_sub(old.1)?,
                        throttled_seconds: round(new.2.checked_sub(old.2)? as f64 / 1_000_000.0),
                    })
                }),
            ))
        });
        // Keep the older counters when the interval is too short for a rate
        if seconds.is_some() || previous.is_none() {
            state.cpu = Some((now, stat));
        }
        let capacity_cpus = cpu_capacity(cgroup);
        Cpu {
            capacity_cpus: capacity_cpus.map(round),
            interval_seconds: rates.as_ref().and(seconds).map(round),
            usage_percent: rates.as_ref().map(|r| r.0),
            capacity_percent: rates.as_ref().zip(capacity_cpus).map(|(r, cpus)| round(r.0 / cpus)),
            user_percent: rates.as_ref().map(|r| r.1),
            system_percent: rates.as_ref().map(|r| r.2),
            throttling: rates.and_then(|r| r.3),
        }
    });

    let memory = cgroup.read("memory.current").and_then(|text| text.trim().parse().ok()).map(|usage_bytes| {
        let stat = cgroup.read("memory.stat");
        let stat = stat.as_deref().map(flat_keyed);
        let events = cgroup.read("memory.events");
        let events = events.as_deref().map(flat_keyed);
        Memory {
            usage_bytes,
            limit_bytes: lowest_limit(cgroup, "memory.max", |value| value.parse().ok()),
            anon_bytes: stat.as_ref().and_then(|stat| stat.get("anon").copied()),
            file_bytes: stat.as_ref().and_then(|stat| stat.get("file").copied()),
            process_rss_bytes: process_rss(),
            events: events.and_then(|events| {
                Some(MemoryEvents {
                    high: *events.get("high")?,
                    max: *events.get("max")?,
                    oom: *events.get("oom")?,
                    oom_kill: *events.get("oom_kill")?,
                })
            }),
        }
    });

    let pressure = Pressure {
        cpu: cgroup.read("cpu.pressure").and_then(|text| parse_psi(&text)),
        memory: cgroup.read("memory.pressure").and_then(|text| parse_psi(&text)),
        io: cgroup.read("io.pressure").and_then(|text| parse_psi(&text)),
    };

    let io = cgroup.read("io.stat").and_then(|text| IoStat::parse(&text)).and_then(|stat| {
        let previous = state.io;
        let seconds = interval(previous.as_ref(), now);
        let rates = seconds.and_then(|seconds| {
            let old = previous?.1;
            let rate = |new: u64, old: u64| Some(round(new.checked_sub(old)? as f64 / seconds));
            Some(Io {
                interval_seconds: round(seconds),
                read_bytes_per_second: rate(stat.read_bytes, old.read_bytes)?,
                write_bytes_per_second: rate(stat.write_bytes, old.write_bytes)?,
                read_ops_per_second: rate(stat.read_ops, old.read_ops)?,
                write_ops_per_second: rate(stat.write_ops, old.write_ops)?,
            })
        });
        if seconds.is_some() || previous.is_none() {
            state.io = Some((now, stat));
        }
        rates
    });

    Container {
        cpu,
        memory,
        pressure,
        io,
        error: None,
    }
}

/// The smallest limit set on the cgroup or a visible ancestor; "max" means none.
/// Limits a container manager sets above the cgroup namespace stay invisible.
fn lowest_limit<T: PartialOrd>(cgroup: &Cgroup, file: &str, parse: impl Fn(&str) -> Option<T>) -> Option<T> {
    cgroup
        .ancestors()
        .filter_map(|dir| bounded_file(dir.join(file), MAX_CGROUP_FILE).ok())
        .filter_map(|text| parse(text.trim()))
        .reduce(|lowest, limit| {
            if limit < lowest {
                limit
            } else {
                lowest
            }
        })
}

fn cpu_capacity(cgroup: &Cgroup) -> Option<f64> {
    // cpuset.cpus.effective already accounts for the ancestors' cpusets
    let cpus = cgroup
        .read("cpuset.cpus.effective")
        .and_then(|text| count_cpus(text.trim()))
        .or_else(|| std::thread::available_parallelism().ok().map(std::num::NonZero::get))? as f64;
    // cpu.max is "$QUOTA $PERIOD" or "max $PERIOD"
    let quota = lowest_limit(cgroup, "cpu.max", |value| {
        let (quota, period) = value.split_once(' ')?;
        let (quota, period) = (quota.parse::<u64>().ok()?, period.parse::<u64>().ok()?);
        (period > 0).then(|| quota as f64 / period as f64)
    });
    Some(quota.map_or(cpus, |quota| quota.min(cpus)))
}

/// A cpu list such as "0-3,6"
fn count_cpus(cpu_list: &str) -> Option<usize> {
    cpu_list.split(',').filter(|range| !range.is_empty()).try_fold(0, |count, range| {
        let (first, last) = range.split_once('-').unwrap_or((range, range));
        let (first, last) = (first.parse::<usize>().ok()?, last.parse::<usize>().ok()?);
        Some(count + last.checked_sub(first)? + 1)
    })
}

fn process_rss() -> Option<u64> {
    let status = bounded_file("/proc/self/status", MAX_CGROUP_FILE).ok()?;
    let kib: u64 =
        status.lines().find_map(|line| line.strip_prefix("VmRSS:"))?.trim().strip_suffix("kB")?.trim().parse().ok()?;
    kib.checked_mul(1024)
}

/// "some avg10=0.00 avg60=0.00 avg300=0.00 total=0" and optionally a "full" line
fn parse_psi(text: &str) -> Option<Psi> {
    let line = |kind: &str| -> Option<PsiLine> {
        let fields: BTreeMap<&str, &str> = text
            .lines()
            .find_map(|line| line.strip_prefix(kind)?.strip_prefix(' '))?
            .split_whitespace()
            .filter_map(|field| field.split_once('='))
            .collect();
        let average = |key: &str| fields.get(key)?.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0);
        Some(PsiLine {
            avg10: average("avg10")?,
            avg60: average("avg60")?,
            avg300: average("avg300")?,
            total_usec: fields.get("total")?.parse().ok()?,
        })
    };
    Some(Psi {
        some: line("some")?,
        full: line("full"),
    })
}

//! HomeStack service telemetry served at GET /.well-known/homestack/status,
//! see .agents/skills/homestack-service/service-status.md.
//!
//! One worker collects only when requested. Handlers wait at most two seconds;
//! a stalled network filesystem cannot create more workers or block the listener.
//! Resource figures come from the service's own cgroup v2. /proc/loadavg is not
//! used: inside a Proxmox LXC container it is the host's load.

// CPU counts and quotas stay far below the 2^52 where f64 loses integers
#![allow(clippy::cast_precision_loss)]

use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{Arc, PoisonError, RwLock, mpsc},
    time::{Duration, Instant},
};

const MAX_FILESYSTEMS: usize = 128;
const MAX_WAIT: Duration = Duration::from_secs(2);
const REUSE_FOR: Duration = Duration::from_secs(2);
// cgroup interface files are small; io.stat grows by one line per device
const MAX_CGROUP_FILE: u64 = 64 * 1024;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Fail,
}

#[derive(Serialize)]
pub struct Report {
    schema: u32,
    /// Process liveness, independent of dependency readiness
    pub status: Status,
    uptime_seconds: u64,
    sample_uptime_usec: Option<u64>,
    sample_age_seconds: Option<u64>,
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
    pressure: Option<Pressure>,
    io: Option<Io>,
    error: Option<&'static str>,
}

#[derive(Clone, Serialize)]
struct Cpu {
    /// Logical CPUs the cgroup may use: its cpuset, lowered by any visible cpu.max quota
    capacity_cpus: Option<f64>,
    /// Cumulative kernel counters; HomeStack computes rates between reports
    usage_usec: u64,
    user_usec: u64,
    system_usec: u64,
    /// Null when the cpu controller is not enabled for the cgroup
    throttling: Option<Throttling>,
}

#[derive(Clone, Serialize)]
struct Throttling {
    periods: u64,
    throttled_periods: u64,
    throttled_usec: u64,
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

/// Cumulative block I/O counters, summed over devices
#[derive(Clone, Serialize)]
struct Io {
    read_bytes: u64,
    write_bytes: u64,
    read_ops: u64,
    write_ops: u64,
}

struct Sample {
    at: Option<Instant>,
    filesystems: Vec<Filesystem>,
    container: Container,
    error: Option<&'static str>,
}

struct Shared {
    sample: Sample,
    finished: Option<Instant>,
    in_flight: bool,
}

/// A single worker sleeps on its channel until an HTTP request needs a measurement.
/// The short lock is never held across OS I/O or an await.
pub struct Telemetry {
    started: Instant,
    shared: Arc<RwLock<Shared>>,
    requests: mpsc::SyncSender<()>,
    completed: tokio::sync::watch::Receiver<()>,
}

impl Telemetry {
    pub fn start(started: Instant) -> std::io::Result<Self> {
        let mut cgroup = None;
        Self::with_collector(started, move || collect(&mut cgroup))
    }

    fn with_collector(started: Instant, mut collect: impl FnMut() -> Sample + Send + 'static) -> std::io::Result<Self> {
        let shared = Arc::new(RwLock::new(Shared {
            sample: Sample {
                at: None,
                filesystems: Vec::new(),
                container: Container {
                    error: Some("cgroup_unavailable"),
                    ..Container::default()
                },
                error: Some("pending"),
            },
            finished: None,
            in_flight: false,
        }));
        let (requests, receiver) = mpsc::sync_channel(1);
        let (completed, updates) = tokio::sync::watch::channel(());
        let worker = Arc::clone(&shared);
        std::thread::Builder::new().name("homestack-status".to_owned()).spawn(move || {
            while receiver.recv().is_ok() {
                let next = collect();
                let mut shared = worker.write().unwrap_or_else(PoisonError::into_inner);
                shared.sample = next;
                shared.finished = Some(Instant::now());
                shared.in_flight = false;
                // Notify while holding the lock so another request cannot miss completion.
                completed.send_replace(());
            }
        })?;
        Ok(Self {
            started,
            shared,
            requests,
            completed: updates,
        })
    }

    pub async fn report(&self, status: Status) -> Report {
        let mut completed = self.completed.clone();
        let wait = {
            let mut shared = self.shared.write().unwrap_or_else(PoisonError::into_inner);
            // Match the watched version to this cache state under the same lock.
            completed.borrow_and_update();
            if shared.finished.is_some_and(|at| at.elapsed() < REUSE_FOR) {
                false
            } else {
                if !shared.in_flight {
                    shared.in_flight = true;
                    if self.requests.try_send(()).is_err() {
                        shared.in_flight = false;
                    }
                }
                shared.in_flight
            }
        };
        if wait {
            let _ = tokio::time::timeout(MAX_WAIT, completed.changed()).await;
        }
        let shared = self.shared.read().unwrap_or_else(PoisonError::into_inner);
        let sample = &shared.sample;
        Report {
            schema: 2,
            status,
            uptime_seconds: self.started.elapsed().as_secs(),
            sample_uptime_usec: sample
                .at
                .and_then(|at| u64::try_from(at.saturating_duration_since(self.started).as_micros()).ok()),
            sample_age_seconds: sample.at.map(|at| at.elapsed().as_secs()),
            filesystems: sample.filesystems.clone(),
            container: sample.container.clone(),
            error: sample.error,
        }
    }
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
fn mountinfo_entries(text: &str) -> Option<Vec<(String, String, &str)>> {
    text.lines()
        .map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let mut fields = left.split_whitespace().skip(3);
            let root = unescape(fields.next()?)?;
            let path = unescape(fields.next()?)?;
            Some((root, path, right.split_whitespace().next()?))
        })
        .collect()
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

fn collect(cgroup: &mut Option<Cgroup>) -> Sample {
    // Timestamp the beginning: an NFS call that eventually returns must not
    // make old measurements appear fresh.
    let at = Instant::now();
    let mountinfo = bounded_file("/proc/self/mountinfo", 1024 * 1024).ok();
    let (filesystems, error) = filesystems(mountinfo.as_deref());
    if cgroup.as_ref().is_none_or(|cgroup| !cgroup.dir.join("cgroup.type").exists()) {
        *cgroup = None;
    }
    let resolved = match mountinfo.as_deref() {
        _ if cgroup.is_some() => Ok(()),
        Some(mountinfo) => Cgroup::resolve(mountinfo).map(|resolved| *cgroup = Some(resolved)),
        None => Err("cgroup_unavailable"),
    };
    let container = resolved.map_or_else(
        |error| Container {
            error: Some(error),
            ..Container::default()
        },
        |()| container(cgroup.as_ref().expect("resolved cgroup")),
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
    match mountinfo.and_then(mountinfo_entries).filter(|entries| !entries.is_empty()) {
        Some(entries) => {
            for (_, path, kind) in entries {
                // File bind mounts are excluded, but an unreadable mount remains visible.
                if data_mount(&path, kind) && !std::fs::metadata(&path).is_ok_and(|metadata| !metadata.is_dir()) {
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
            .ok_or("cgroup_unavailable")?
            .into_iter()
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

fn container(cgroup: &Cgroup) -> Container {
    let cpu = cgroup.read("cpu.stat").and_then(|text| CpuStat::parse(&text)).map(|stat| Cpu {
        capacity_cpus: cpu_capacity(cgroup).map(round),
        usage_usec: stat.usage_usec,
        user_usec: stat.user_usec,
        system_usec: stat.system_usec,
        throttling: stat.throttling.map(|(periods, throttled_periods, throttled_usec)| Throttling {
            periods,
            throttled_periods,
            throttled_usec,
        }),
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

    let io = cgroup.read("io.stat").and_then(|text| IoStat::parse(&text)).map(|stat| Io {
        read_bytes: stat.read_bytes,
        write_bytes: stat.write_bytes,
        read_ops: stat.read_ops,
        write_ops: stat.write_ops,
    });

    Container {
        cpu,
        memory,
        pressure: Some(pressure),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sample() -> Sample {
        Sample {
            at: Some(Instant::now()),
            filesystems: Vec::new(),
            container: Container::default(),
            error: None,
        }
    }

    #[rocket::async_test]
    async fn homestack_telemetry_is_idle_and_coalesces_requests() {
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let telemetry = Telemetry::with_collector(Instant::now(), move || {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            sample()
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let reports = futures::future::join_all((0..16).map(|_| telemetry.report(Status::Pass))).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(reports.iter().all(|r| r.schema == 2 && r.sample_uptime_usec == reports[0].sample_uptime_usec));
        let json = serde_json::to_value(&reports[0]).unwrap();
        assert!(json.get("load_average").is_none());
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        telemetry.shared.write().unwrap().finished = Instant::now().checked_sub(REUSE_FOR);
        telemetry.report(Status::Pass).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[rocket::async_test]
    async fn homestack_stalled_measurement_is_bounded_and_preserves_age() {
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let (release, blocked) = mpsc::channel();
        let telemetry = Telemetry::with_collector(Instant::now() - Duration::from_secs(100), move || {
            let next = sample();
            if worker_calls.fetch_add(1, Ordering::SeqCst) == 1 {
                blocked.recv().unwrap();
            }
            next
        })
        .unwrap();
        telemetry.report(Status::Pass).await;
        {
            let mut shared = telemetry.shared.write().unwrap();
            shared.finished = Instant::now().checked_sub(REUSE_FOR);
            shared.sample.at = Some(Instant::now() - Duration::from_secs(40));
        }
        let before = Instant::now();
        let reports = futures::future::join_all((0..12).map(|_| telemetry.report(Status::Pass))).await;
        assert!(before.elapsed() < Duration::from_secs(3));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(reports.iter().all(|r| r.sample_age_seconds.unwrap() >= 40));
        release.send(()).unwrap();
        telemetry.report(Status::Pass).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[rocket::async_test]
    async fn homestack_first_stalled_measurement_reports_pending() {
        let (release, blocked) = mpsc::channel();
        let telemetry = Telemetry::with_collector(Instant::now(), move || {
            let next = sample();
            blocked.recv().unwrap();
            next
        })
        .unwrap();
        let report = telemetry.report(Status::Pass).await;
        assert_eq!(report.error, Some("pending"));
        assert!(report.sample_age_seconds.is_none() && report.sample_uptime_usec.is_none());
        release.send(()).unwrap();
    }

    #[test]
    fn homestack_mount_failures_are_not_missing_mounts_or_zero_space() {
        let text = "1 0 0:1 / / rw - xfs /dev/test rw\n2 1 0:2 / /__tutor_missing_mount rw - nfs server:/share rw\n";
        let (filesystems, error) = filesystems(Some(text));
        assert!(error.is_none());
        let failed = filesystems.iter().find(|fs| fs.mount == "/__tutor_missing_mount").unwrap();
        assert!(failed.mounted);
        assert_eq!(failed.filesystem.as_deref(), Some("nfs"));
        assert_eq!(failed.error, Some("measurement_unavailable"));
        assert!(failed.available_bytes.is_none());
        let home = filesystems.iter().find(|fs| fs.mount == "/home/app").unwrap();
        assert!(!home.mounted && home.available_bytes.is_none());
        assert_eq!(home.error, Some("not_mounted"));
    }

    #[test]
    fn homestack_invalid_mount_inventory_is_an_error() {
        for inventory in [None, Some(""), Some("broken"), Some("1 0 0:1 / / rw - ext4 dev rw\nbroken")] {
            let (mounts, error) = filesystems(inventory);
            assert_eq!(error, Some("mount_inventory_unavailable"));
            assert_eq!(mounts.len(), 2);
            assert!(mounts.iter().all(|fs| !fs.mounted && fs.error == error && fs.total_bytes.is_none()));
        }
        assert_eq!(unescape("/a\\040b\\134c").as_deref(), Some("/a b\\c"));
    }

    #[test]
    fn homestack_missing_cgroup_and_psi_are_null() {
        let unavailable = serde_json::to_value(Container {
            error: Some("cgroup_unavailable"),
            ..Container::default()
        })
        .unwrap();
        for field in ["cpu", "memory", "pressure", "io"] {
            assert!(unavailable[field].is_null());
        }
        assert!(parse_psi("").is_none());
        let psi = parse_psi("some avg10=0.10 avg60=0.20 avg300=0.30 total=456\n").unwrap();
        assert!(psi.full.is_none());
        assert_eq!(psi.some.total_usec, 456);
    }

    #[test]
    fn homestack_counters_are_cumulative_and_overflow_is_unavailable() {
        let cpu = CpuStat::parse("usage_usec 7000000\nuser_usec 5000000\nsystem_usec 2000000\nnr_periods 30\nnr_throttled 4\nthrottled_usec 12345\n").unwrap();
        assert_eq!(cpu.usage_usec, 7_000_000);
        assert_eq!(cpu.throttling, Some((30, 4, 12345)));
        let io =
            IoStat::parse("8:0 rbytes=100 wbytes=200 rios=3 wios=4\n8:1 rbytes=50 wbytes=60 rios=1 wios=2\n").unwrap();
        assert_eq!((io.read_bytes, io.write_bytes, io.read_ops, io.write_ops), (150, 260, 4, 6));
        assert!(IoStat::parse("8:0 rbytes=18446744073709551615\n8:1 rbytes=1\n").is_none());
        assert!(!data_mount("/proc", "proc"));
        assert!(data_mount("/home/app", "tmpfs"));
        assert!(data_mount("/mnt/data", "fuse.sshfs"));
    }

    #[test]
    fn homestack_cgroup_reports_raw_counters_and_visible_limits() {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let top = std::env::temp_dir().join(format!("tutor-cgroup-{}-{unique}", std::process::id()));
        let dir = top.join("service");
        std::fs::create_dir_all(&dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
        let _cleanup = Cleanup(top.clone());
        for (file, text) in [
            ("cgroup.type", "domain"),
            (
                "cpu.stat",
                "usage_usec 700
user_usec 500
system_usec 200
nr_periods 30
nr_throttled 4
throttled_usec 12345
",
            ),
            ("cpu.max", "max 100000"),
            ("cpuset.cpus.effective", "0-3"),
            ("memory.current", "1234"),
            ("memory.max", "268435456"),
            (
                "memory.stat",
                "anon 1000
file 234
",
            ),
            (
                "memory.events",
                "high 0
max 1
oom 0
oom_kill 0
",
            ),
            (
                "io.stat",
                "8:0 rbytes=100 wbytes=200 rios=3 wios=4
",
            ),
        ] {
            std::fs::write(dir.join(file), text).unwrap();
        }
        std::fs::write(top.join("cpu.max"), "50000 100000").unwrap();
        std::fs::write(top.join("memory.max"), "134217728").unwrap();
        let mountinfo = format!(
            "1 0 0:1 / {} rw - cgroup2 cgroup rw
",
            top.display()
        );
        let cgroup = Cgroup::from_text(
            "0::/service
",
            &mountinfo,
        )
        .unwrap();
        let json = serde_json::to_value(container(&cgroup)).unwrap();
        assert_eq!(json["cpu"]["usage_usec"], 700);
        assert_eq!(json["cpu"]["capacity_cpus"], 0.5);
        assert_eq!(json["cpu"]["throttling"]["throttled_usec"], 12345);
        assert!(json["cpu"].get("usage_percent").is_none());
        assert_eq!(json["memory"]["usage_bytes"], 1234);
        assert_eq!(json["memory"]["limit_bytes"], 134217728);
        assert_eq!(json["memory"]["events"]["max"], 1);
        assert!(json["pressure"]["cpu"].is_null());
        assert_eq!(json["io"]["write_bytes"], 200);
        assert!(
            Cgroup::from_text(
                "0::/../sibling
",
                &mountinfo
            )
            .is_err()
        );
        assert_eq!(
            Cgroup::from_text(
                "0::/
", &mountinfo
            )
            .err(),
            Some("host_cgroup")
        );
    }

    #[test]
    fn homestack_bounded_mounts_fit_the_response_limit() {
        // Exercise worst-case JSON escaping and 20-digit byte counts at the
        // inventory's mount-count and combined text limits.
        let filesystems = (0..MAX_FILESYSTEMS)
            .map(|index| Filesystem {
                mount: format!("/{index:03}{}", "\u{1}".repeat(27)),
                filesystem: Some("x".to_owned()),
                mounted: true,
                total_bytes: Some(u64::MAX),
                used_bytes: Some(u64::MAX),
                free_bytes: Some(u64::MAX),
                available_bytes: Some(u64::MAX),
                error: None,
            })
            .collect();
        let report = Report {
            schema: 2,
            status: Status::Pass,
            uptime_seconds: u64::MAX,
            sample_uptime_usec: Some(u64::MAX),
            sample_age_seconds: Some(u64::MAX),
            filesystems,
            container: Container::default(),
            error: Some("too_many_mounts"),
        };
        assert!(serde_json::to_vec(&report).unwrap().len() < 60 * 1024);
    }
}

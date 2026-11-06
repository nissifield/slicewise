//! The daemon (design §5.1, §5.4, §5.7): one tick loop, one accept thread,
//! a short-lived thread per connection, a 256-entry ring buffer and the queue.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{self, BufReader, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::cgroup::{self, SliceStats};
use crate::config::{Class, Config};
use crate::log;
use crate::policy::{
    self, Action, Admission, Decision, Knob, RESERVATION_TTL, Snapshot, State, Write as PolicyWrite,
};
use crate::proto::{
    self, ClassRow, DecisionReply, ErrorCode, Event, Explain, ProtectedRow, Request, Response,
    Status,
};
use crate::systemd::{self, Runner};

const EVENT_CAP: usize = 256;
const CONNECTIONS: usize = 64;
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const REFRESH: Duration = Duration::from_secs(10);

/// Injectable roots, as in `cgroup.rs` (design §10).
pub struct Paths {
    pub cgroup: PathBuf,
    pub proc: PathBuf,
    pub cpu_online: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            cgroup: PathBuf::from(cgroup::CGROUP_ROOT),
            proc: PathBuf::from(cgroup::PROC_ROOT),
            cpu_online: PathBuf::from("/sys/devices/system/cpu/online"),
        }
    }
}

/// Reads the snapshot, keeping the last good value when a read fails (L14).
#[derive(Default)]
pub struct Sampler {
    paths: Paths,
    pool_rel: Option<String>,
    protected_rel: BTreeMap<String, String>,
    last_pool: SliceStats,
    last_classes: [SliceStats; 3],
    last_units: BTreeMap<String, u64>,
    mem_available: u64,
    online_cpus: u32,
    next_refresh: Duration,
}

impl Sampler {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            online_cpus: 1,
            ..Self::default()
        }
    }

    /// Test seam: the fake-cgroup test supplies its own pool path (design §10).
    pub fn with_pool(mut self, pool: &str) -> Self {
        self.pool_rel = Some(pool.to_string());
        self
    }

    pub fn snapshot(
        &mut self,
        now: Duration,
        runner: &Runner,
        config: &Config,
        units: &[String],
    ) -> Snapshot {
        let root = self.paths.cgroup.clone();
        let refresh = now >= self.next_refresh;
        if refresh {
            self.next_refresh = now + REFRESH;
        }
        if self.pool_rel.is_none() && refresh {
            match runner.show(&config.pool_slice, "ControlGroup") {
                Ok(rel) if !rel.is_empty() => self.pool_rel = Some(rel),
                Ok(_) => {}
                Err(error) => self.log_error("pool", &error),
            }
        }
        if let Some(pool) = self.pool_rel.clone() {
            match cgroup::slice_stats(&root, &pool) {
                Ok(stats) => self.last_pool = stats,
                Err(error) => self.log_error("pool", &error),
            }
            for class in Class::ALL {
                let rel = format!("{pool}/{}", config.class_slice(class));
                match cgroup::slice_stats(&root, &rel) {
                    Ok(stats) => self.last_classes[class.index()] = stats,
                    // A class slice appears with its first unit; missing is normal.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        self.last_classes[class.index()] = SliceStats::default();
                    }
                    Err(error) => self.log_error(&rel, &error),
                }
            }
        }
        match cgroup::mem_available_bytes(&self.paths.proc) {
            Ok(bytes) => self.mem_available = bytes,
            Err(error) => self.log_error("meminfo", &error),
        }
        match cgroup::online_cpus(&self.paths.cpu_online) {
            Ok(cpus) => self.online_cpus = cpus,
            Err(error) => self.log_error("online_cpus", &error),
        }
        let protected_cpu_pressure = config
            .protected
            .iter()
            .map(|protected| self.protected_psi(&root, runner, &protected.unit, refresh))
            .collect();
        let map = units
            .iter()
            .filter_map(|unit| {
                self.unit_ws(&root, config, unit)
                    .map(|working_set| (unit.clone(), working_set))
            })
            .collect();
        Snapshot {
            mem_available: self.mem_available,
            online_cpus: self.online_cpus,
            pool: self.last_pool,
            classes: self.last_classes,
            protected_cpu_pressure,
            units: map,
        }
    }

    fn protected_psi(&mut self, root: &Path, runner: &Runner, unit: &str, refresh: bool) -> f32 {
        if refresh
            && !self.protected_rel.contains_key(unit)
            && let Ok(rel) = runner.show(unit, "ControlGroup")
            && !rel.is_empty()
        {
            self.protected_rel.insert(unit.to_string(), rel);
        }
        self.protected_rel
            .get(unit)
            .and_then(|rel| cgroup::read_trimmed(root, &format!("{rel}/cpu.pressure")).ok())
            .and_then(|text| cgroup::parse_psi(&text))
            .map_or(0.0, |psi| psi.some_avg10)
    }

    /// ENOENT in every class means the unit has no cgroup; any other error keeps
    /// the last value and logs (L14).
    fn unit_ws(&mut self, root: &Path, config: &Config, unit: &str) -> Option<u64> {
        let pool = self.pool_rel.clone()?;
        let mut last_error = None;
        for class in Class::ALL {
            let rel = format!("{pool}/{}/{unit}", config.class_slice(class));
            match read_working_set(root, &rel) {
                Ok(working_set) => {
                    self.last_units.insert(unit.to_string(), working_set);
                    return Some(working_set);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => last_error = Some(error),
            }
        }
        last_error.and_then(|error| {
            self.log_error(unit, &error);
            self.last_units.get(unit).copied()
        })
    }

    fn log_error(&self, target: &str, error: &impl std::fmt::Display) {
        log::emit(
            "warn",
            "error",
            &[("target", target), ("reason", &error.to_string())],
        );
    }
}

fn read_working_set(root: &Path, rel: &str) -> io::Result<u64> {
    let current = cgroup::read_u64(root, &format!("{rel}/memory.current"))?;
    let stat = cgroup::read_trimmed(root, &format!("{rel}/memory.stat"))?;
    let inactive = cgroup::parse_memory_stat(&stat)
        .get("inactive_file")
        .copied()
        .unwrap_or(0);
    Ok(current.saturating_sub(inactive))
}

/// All mutable daemon state; the loop and the connection threads share it.
#[derive(Default)]
pub struct Daemon {
    state: State,
    snapshot: Snapshot,
    events: VecDeque<(Duration, Option<Class>, Event)>,
    /// Units the loop has admitted from the queue, waiting for their wrapper (jobs, reason).
    admitted: Vec<(String, Duration, u32, String)>,
    over_budget: bool,
}

impl Daemon {
    fn push(
        &mut self,
        now: Duration,
        event: &str,
        unit: Option<&str>,
        class: Option<Class>,
        reason: &str,
    ) {
        if self.events.len() == EVENT_CAP {
            self.events.pop_front();
        }
        let event = Event {
            age_secs: 0,
            event: event.to_string(),
            unit: unit.map(str::to_string),
            reason: reason.to_string(),
        };
        self.events.push_back((now, class, event));
    }
}

struct Shared {
    daemon: Mutex<Daemon>,
    wake: Condvar,
    config: Config,
    mem_total: u64,
    start: Instant,
    runner: Runner,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One loop iteration, without the socket and threads: sample, decide, apply.
/// Writes reach `State::apply` only after `set-property` succeeded (Q5).
pub fn tick(
    daemon: &Mutex<Daemon>,
    sampler: &mut Sampler,
    config: &Config,
    runner: &Runner,
    now: Duration,
) {
    let units = lock(daemon)
        .state
        .reservations
        .iter()
        .map(|reservation| reservation.unit.clone())
        .collect::<Vec<_>>();
    let snapshot = sampler.snapshot(now, runner, config, &units);
    let mut writes = Vec::new();
    {
        let mut daemon = lock(daemon);
        daemon.snapshot = snapshot.clone();
        daemon.over_budget = false;
        daemon
            .admitted
            .retain(|(_, at, _, _)| now.saturating_sub(*at) < RESERVATION_TTL);
        for action in policy::decide(&snapshot, &mut daemon.state, config, now) {
            match action {
                Action::Admission(admission) => {
                    if admission.from_queue && admission.decision == Decision::Admit {
                        daemon.admitted.push((
                            admission.request.unit.clone(),
                            now,
                            admission.jobs,
                            admission.reason.clone(),
                        ));
                    }
                    log_admission(&admission);
                    daemon.push(
                        now,
                        admission.event(),
                        Some(&admission.request.unit),
                        Some(admission.request.class),
                        &admission.reason,
                    );
                    daemon.state.apply(&[Action::Admission(admission)], now);
                }
                Action::Write(write) => writes.push(write),
            }
        }
    }
    for write in writes {
        match runner.set_property(&write.slice, &systemd::write_property(&write)) {
            Ok(_) => {
                let mut daemon = lock(daemon);
                daemon.state.apply(&[Action::Write(write.clone())], now);
                log_write(&write);
                daemon.push(
                    now,
                    write.event.as_str(),
                    None,
                    Some(write.class),
                    &write.reason,
                );
            }
            Err(error) => log::emit(
                "warn",
                "error",
                &[("target", &write.slice), ("reason", &error)],
            ),
        }
    }
}

fn log_admission(admission: &Admission) {
    log::emit(
        if admission.decision == Decision::Admit {
            "info"
        } else {
            "warn"
        },
        admission.event(),
        &[
            ("unit", &admission.request.unit),
            ("class", admission.request.class.as_str()),
            ("need", &admission.need.to_string()),
            ("jobs", &admission.jobs.to_string()),
            ("reason", &admission.reason),
        ],
    );
}

fn log_write(write: &PolicyWrite) {
    let knob = match write.knob {
        Knob::MemoryHigh => "MemoryHigh",
        Knob::CpuQuota => "CPUQuota",
    };
    log::emit(
        "info",
        write.event.as_str(),
        &[
            ("target", &write.slice),
            ("knob", knob),
            (
                "from",
                &write.from.map_or("infinity".to_string(), |v| v.to_string()),
            ),
            (
                "to",
                &write.to.map_or("infinity".to_string(), |v| v.to_string()),
            ),
            ("reason", &write.reason),
        ],
    );
}

/// The `status` payload (design §8); also used by `status` when the daemon is down.
pub fn status(snapshot: &Snapshot, state: &State, config: &Config, now: Duration) -> Status {
    Status {
        cap: config.pool_memory_max.0,
        working_set: policy::working_set(&snapshot.pool),
        headroom: policy::headroom(snapshot, config),
        margin: policy::margin(config),
        classes: Class::ALL
            .iter()
            .map(|&class| class_row(class, snapshot, state, config, now))
            .collect(),
        protected: config
            .protected
            .iter()
            .zip(&snapshot.protected_cpu_pressure)
            .map(|(p, &cpu_psi)| ProtectedRow {
                unit: p.unit.clone(),
                cpu_psi,
            })
            .collect(),
        queue: state.queue.len(),
    }
}

fn class_row(
    class: Class,
    snapshot: &Snapshot,
    state: &State,
    config: &Config,
    now: Duration,
) -> ClassRow {
    let stats = snapshot.class(class);
    let batch = class == Class::Batch;
    let age = state
        .last_write
        .get(&config.class_slice(class))
        .map(|at| now.saturating_sub(*at).as_secs());
    ClassRow {
        class,
        memory: stats.memory_current,
        memory_high: state.memory_high[class.index()],
        cpu_quota: state
            .clamp_cpus
            .filter(|_| batch)
            .map(|cpus| u64::from(cpus) * 100),
        cpu_psi: stats.cpu_pressure.map_or(0.0, |psi| psi.some_avg10),
        memory_psi: stats.memory_pressure.map_or(0.0, |psi| psi.some_avg10),
        pids: stats.pids_current,
        brake_secs: state.memory_high[class.index()]
            .is_some()
            .then_some(age)
            .flatten(),
        clamp_secs: (batch && state.clamp_cpus.is_some())
            .then_some(age)
            .flatten(),
    }
}

fn explain(daemon: &Daemon, config: &Config, target: &str, now: Duration) -> Explain {
    let is_class = Class::ALL.iter().any(|class| class.as_str() == target);
    let class = Class::ALL
        .into_iter()
        .find(|class| class.as_str() == target)
        .or_else(|| {
            daemon
                .events
                .iter()
                .rev()
                .find(|(_, _, event)| event.unit.as_deref() == Some(target))
                .and_then(|(_, class, _)| *class)
        });
    Explain {
        target: target.to_string(),
        class,
        slice: class.map(|class| config.class_slice(class)),
        events: daemon
            .events
            .iter()
            .filter(|(_, event_class, event)| {
                event.unit.as_deref() == Some(target)
                    || (class.is_some()
                        && *event_class == class
                        && (is_class || event.unit.is_none()))
            })
            .map(|(at, _, event)| Event {
                age_secs: now.saturating_sub(*at).as_secs(),
                event: event.event.clone(),
                unit: event.unit.clone(),
                reason: event.reason.clone(),
            })
            .collect(),
        current: class.map(|class| class_row(class, &daemon.snapshot, &daemon.state, config, now)),
    }
}

struct Deadline {
    stream: UnixStream,
    deadline: Instant,
}

impl Read for Deadline {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let left = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or(io::ErrorKind::TimedOut)?;
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buffer)
    }
}

fn serve(listener: UnixListener, shared: Arc<Shared>, connections: Arc<AtomicUsize>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if connections.fetch_add(1, Ordering::SeqCst) >= CONNECTIONS {
            connections.fetch_sub(1, Ordering::SeqCst);
            let _ = proto::write_one(
                &mut &stream,
                &Response::error(
                    ErrorCode::Busy,
                    format!("more than {CONNECTIONS} connections"),
                ),
            );
            continue;
        }
        let shared = Arc::clone(&shared);
        let connections = Arc::clone(&connections);
        thread::spawn(move || {
            let _ = handle(&stream, &shared);
            connections.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

fn handle(stream: &UnixStream, shared: &Shared) -> io::Result<()> {
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let reader = Deadline {
        stream: stream.try_clone()?,
        deadline: Instant::now() + READ_TIMEOUT,
    };
    let mut reader = BufReader::new(reader);
    let request = match proto::read_one(&mut reader, shared.mem_total) {
        Ok(request) => request,
        Err((code, detail)) => {
            return proto::write_one(&mut &*stream, &Response::error(code, detail));
        }
    };
    let now = shared.start.elapsed();
    match request {
        Request::Admit {
            unit,
            class,
            mem_hint,
            jobs_hint,
            queue,
            ..
        } => admit(
            stream,
            shared,
            policy::AdmitRequest {
                unit,
                class,
                mem_hint,
                jobs_hint,
                queue,
            },
            now,
        ),
        Request::Status { .. } => {
            let status = {
                let daemon = lock(&shared.daemon);
                status(&daemon.snapshot, &daemon.state, &shared.config, now)
            };
            proto::write_one(&mut &*stream, &Response::status(status))
        }
        Request::Explain { target, .. } => {
            let explain = {
                let daemon = lock(&shared.daemon);
                explain(&daemon, &shared.config, &target, now)
            };
            proto::write_one(&mut &*stream, &Response::explain(explain))
        }
    }
}

fn admit(
    stream: &UnixStream,
    shared: &Shared,
    request: policy::AdmitRequest,
    now: Duration,
) -> io::Result<()> {
    let (response, over_budget) = {
        let mut daemon = lock(&shared.daemon);
        let admission = policy::admit(
            &daemon.snapshot,
            &daemon.state,
            &shared.config,
            &request,
            now,
        );
        let response = match admission.decision {
            Decision::Admit | Decision::OverBudget => {
                Response::admit(admission.jobs, admission.reason.clone())
            }
            Decision::Queue { position } => Response::queued(position, admission.reason.clone()),
            Decision::WouldQueue | Decision::Reject => Response::reject(admission.reason.clone()),
        };
        let over_budget = admission.decision == Decision::OverBudget;
        if over_budget {
            daemon.over_budget = true;
        }
        log_admission(&admission);
        daemon.push(
            now,
            admission.event(),
            Some(&request.unit),
            Some(request.class),
            &admission.reason,
        );
        daemon.state.apply(&[Action::Admission(admission)], now);
        (response, over_budget)
    };
    if over_budget {
        // Wake the loop so Rule M runs at once; the loop is the only writer.
        shared.wake.notify_all();
    }
    let queued = response.decision == Some(DecisionReply::Queued);
    proto::write_one(&mut &*stream, &response)?;
    if queued {
        wait_for_admission(stream, shared, &request.unit)?;
    }
    Ok(())
}

/// A queued reply is followed by exactly one more line (design §6.2).
fn wait_for_admission(stream: &UnixStream, shared: &Shared, unit: &str) -> io::Result<()> {
    let mut out = stream;
    loop {
        let mut daemon = lock(&shared.daemon);
        if let Some(index) = daemon.admitted.iter().position(|(name, ..)| name == unit) {
            let (.., jobs, reason) = daemon.admitted.remove(index);
            drop(daemon);
            return proto::write_one(&mut out, &Response::admit(jobs, reason));
        }
        if !daemon
            .state
            .queue
            .iter()
            .any(|request| request.unit == unit)
        {
            drop(daemon);
            return proto::write_one(
                &mut out,
                &Response::reject("left the queue without admission"),
            );
        }
        let (guard, _) = shared
            .wake
            .wait_timeout(daemon, Duration::from_secs(1))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        drop(guard);
    }
}

pub fn run() -> u8 {
    run_inner().map(|()| 0).unwrap_or_else(|(code, message)| {
        eprintln!("{message}");
        code
    })
}

fn run_inner() -> Result<(), (u8, String)> {
    let mem_total = cgroup::mem_total_bytes(Path::new(cgroup::PROC_ROOT))
        .map_err(|error| (78, format!("config error: cannot read MemTotal: {error}")))?;
    let config = Config::load_or_default(mem_total).map_err(|error| (78, error))?;
    if config.protected.is_empty() {
        // design: §7 — every key is optional except [[protected]].
        log::emit(
            "warn",
            "error",
            &[("reason", "config has no [[protected]] units")],
        );
    }
    check_delegation().map_err(|error| (78, error))?;
    let listener = bind_socket().map_err(|error| (1, error))?;
    let shared = Arc::new(Shared {
        daemon: Mutex::new(Daemon::default()),
        wake: Condvar::new(),
        config,
        mem_total,
        start: Instant::now(),
        runner: Runner::real(),
    });
    let connections = Arc::new(AtomicUsize::new(0));
    let (shared2, connections2) = (Arc::clone(&shared), Arc::clone(&connections));
    thread::spawn(move || serve(listener, shared2, connections2));
    let mut sampler = Sampler::new(Paths::default());
    loop {
        tick(
            &shared.daemon,
            &mut sampler,
            &shared.config,
            &shared.runner,
            shared.start.elapsed(),
        );
        // Wake queued wrappers so an admission's second line goes out at once.
        shared.wake.notify_all();
        let daemon = lock(&shared.daemon);
        if daemon.over_budget {
            drop(daemon);
            continue;
        }
        let (_guard, _) = shared
            .wake
            .wait_timeout(daemon, Duration::from_secs(shared.config.tick_secs))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

/// design: §5.7 — refuse to run without the cpu and memory controllers.
fn check_delegation() -> Result<(), String> {
    let text = fs::read_to_string("/proc/self/cgroup")
        .map_err(|error| format!("config error: cannot read /proc/self/cgroup: {error}"))?;
    let own = cgroup::parse_proc_cgroup(&text)
        .ok_or_else(|| "config error: no cgroup v2 line in /proc/self/cgroup".to_string())?;
    let parts: Vec<&str> = own.split('/').collect();
    let manager = parts
        .iter()
        .position(|part| part.contains("user@"))
        .ok_or_else(|| "config error: cannot locate the user manager cgroup".to_string())?;
    let rel = format!("{}/cgroup.subtree_control", parts[..=manager].join("/"));
    let control = cgroup::read_trimmed(Path::new(cgroup::CGROUP_ROOT), &rel)
        .map_err(|error| format!("config error: cannot read cgroup.subtree_control: {error}"))?;
    for controller in ["cpu", "memory"] {
        if !control.split_whitespace().any(|name| name == controller) {
            return Err(format!(
                "config error: the user manager does not delegate the {controller} controller"
            ));
        }
    }
    Ok(())
}

/// design: §9 — a 0600 socket under the 0700 runtime dir, never another user's file.
fn bind_socket() -> Result<UnixListener, String> {
    let path = proto::socket_path().ok_or_else(|| {
        "config error: XDG_RUNTIME_DIR is unset; there is no /tmp fallback".to_string()
    })?;
    let uid = |path: &Path| {
        fs::metadata(path)
            .map(|meta| meta.uid())
            .map_err(|error| format!("cannot stat {}: {error}", path.display()))
    };
    if path.exists() {
        if uid(&path)? != uid(Path::new("/proc/self"))? {
            return Err(format!(
                "refusing to use {}: owned by another user",
                path.display()
            ));
        }
        if UnixStream::connect(&path).is_ok() {
            return Err("another daemon is already listening".to_string());
        }
        fs::remove_file(&path)
            .map_err(|error| format!("cannot remove {}: {error}", path.display()))?;
    }
    let listener = UnixListener::bind(&path)
        .map_err(|error| format!("cannot bind {}: {error}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("cannot chmod {}: {error}", path.display()))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_refuses_a_dribbling_reader_after_the_overall_timeout() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut reader = Deadline {
            stream: left,
            deadline: Instant::now() + Duration::from_millis(50),
        };
        // The peer keeps the connection open and sends nothing.
        let mut buffer = [0_u8; 8];
        let error = reader.read(&mut buffer).unwrap_err();
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "{error}"
        );
        drop(right);
    }

    #[test]
    fn status_reports_the_pool_and_class_rows() {
        let config = Config {
            pool_memory_max: crate::config::Size(8 << 30),
            system_reserve: crate::config::Size(1 << 30),
            ..Config::default()
        };
        let mut snapshot = Snapshot {
            mem_available: 8 << 30,
            online_cpus: 4,
            ..Snapshot::default()
        };
        snapshot.pool.memory_current = 2 << 30;
        snapshot.classes[Class::Batch.index()].memory_current = 1 << 30;
        snapshot.classes[Class::Batch.index()].pids_current = 3;
        let state = State {
            clamp_cpus: Some(2),
            ..State::default()
        };
        let status = status(&snapshot, &state, &config, Duration::from_secs(5));
        assert_eq!(status.cap, 8 << 30);
        assert_eq!(status.working_set, 2 << 30);
        assert_eq!(status.headroom, 6 << 30);
        let batch = &status.classes[Class::Batch.index()];
        assert_eq!(batch.class, Class::Batch);
        assert_eq!(batch.memory, 1 << 30);
        assert_eq!(batch.pids, 3);
        assert_eq!(batch.memory_high, None);
        assert_eq!(batch.cpu_quota, Some(200));
        assert_eq!(status.classes[Class::Normal.index()].cpu_quota, None);
        assert_eq!(status.queue, 0);
    }
}

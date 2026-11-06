//! Pure policy (design §5.4–§5.6): snapshot + state + config -> actions.
//! No I/O and no clock reads; the caller passes `now` and applies the actions.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use crate::cgroup::SliceStats;
use crate::config::{Class, Config};

pub const QUEUE_CAP: usize = 32;
pub const RESERVATION_TTL: Duration = Duration::from_secs(300);
pub const BRAKE_RELEASE_TICKS: u32 = 5;
pub const CLAMP_ENGAGE_TICKS: u32 = 2;
pub const CLAMP_RELEASE_TICKS: u32 = 15;
pub const BRAKE_HEADROOM: u64 = 64 << 20;
pub const MIN_BRAKE_MARGIN: u64 = 512 << 20;

/// Everything the loop reads in one tick; built from cgroup/proc reads by the daemon.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub mem_available: u64,
    pub online_cpus: u32,
    pub pool: SliceStats,
    pub classes: [SliceStats; 3],
    pub protected_cpu_pressure: Vec<f32>,
    /// Working set of each reserved unit present in cgroupfs.
    pub units: BTreeMap<String, u64>,
}

impl Snapshot {
    pub fn class(&self, class: Class) -> &SliceStats {
        &self.classes[class.index()]
    }

    /// Highest PSI some-avg10 of protected units and the interactive slice.
    fn cpu_pressure(&self) -> f32 {
        let protected = self
            .protected_cpu_pressure
            .iter()
            .copied()
            .fold(0.0_f32, f32::max);
        let interactive = self
            .class(Class::Interactive)
            .cpu_pressure
            .map_or(0.0, |psi| psi.some_avg10);
        protected.max(interactive)
    }
}

pub fn working_set(stats: &SliceStats) -> u64 {
    stats.memory_current.saturating_sub(stats.inactive_file)
}

pub fn margin(config: &Config) -> u64 {
    (config.pool_memory_max.0 / 20).max(MIN_BRAKE_MARGIN)
}

pub fn headroom(snapshot: &Snapshot, config: &Config) -> u64 {
    let pool_headroom = config
        .pool_memory_max
        .0
        .saturating_sub(working_set(&snapshot.pool));
    let system_headroom = snapshot
        .mem_available
        .saturating_sub(config.system_reserve.0);
    pool_headroom.min(system_headroom)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub unit: String,
    pub need: u64,
    pub bytes: u64,
    pub admitted_at: Duration,
    pub seen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmitRequest {
    pub unit: String,
    pub class: Class,
    pub mem_hint: Option<u64>,
    pub jobs_hint: Option<u32>,
    pub queue: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Admit,
    OverBudget,
    Queue { position: usize },
    WouldQueue,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    pub request: AdmitRequest,
    pub decision: Decision,
    pub need: u64,
    pub jobs: u32,
    pub from_queue: bool,
    pub reason: String,
}

impl Admission {
    pub fn event(&self) -> &'static str {
        match self.decision {
            Decision::Admit => "admit",
            Decision::OverBudget => "over_budget",
            Decision::Queue { .. } | Decision::WouldQueue => "queue",
            Decision::Reject => "reject",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knob {
    MemoryHigh,
    CpuQuota,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteEvent {
    BrakeOn,
    BrakeOff,
    Clamp,
    Unclamp,
}

impl WriteEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            WriteEvent::BrakeOn => "brake_on",
            WriteEvent::BrakeOff => "brake_off",
            WriteEvent::Clamp => "clamp",
            WriteEvent::Unclamp => "unclamp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    pub event: WriteEvent,
    pub slice: String,
    pub class: Class,
    pub knob: Knob,
    pub from: Option<u64>,
    pub to: Option<u64>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Admission(Admission),
    Write(Write),
}

impl Action {
    pub fn event(&self) -> &'static str {
        match self {
            Action::Admission(admission) => admission.event(),
            Action::Write(write) => write.event.as_str(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct State {
    pub reservations: Vec<Reservation>,
    pub queue: VecDeque<AdmitRequest>,
    pub last_write: BTreeMap<String, Duration>,
    /// `Some(bytes)` while the memory brake holds the class; `None` is `infinity`.
    pub memory_high: [Option<u64>; 3],
    /// `Some(cpus)` while clamped; `None` is no clamp (`infinity`).
    pub clamp_cpus: Option<u32>,
    pub high_headroom_ticks: u32,
    pub psi_high_ticks: u32,
    pub psi_low_ticks: u32,
    pub last_release: Option<Duration>,
    pub last_observe: Option<Duration>,
}

impl State {
    pub fn expire(&mut self, now: Duration) {
        self.reservations
            .retain(|r| now.saturating_sub(r.admitted_at) < RESERVATION_TTL);
    }

    pub fn outstanding(&self, _now: Duration) -> u64 {
        self.reservations
            .iter()
            .fold(0_u64, |sum, r| sum.saturating_add(r.bytes))
    }

    /// Consecutive-tick counters for Rules M and C; at most once per `tick_secs`.
    fn observe(&mut self, snapshot: &Snapshot, config: &Config, now: Duration) {
        let tick = Duration::from_secs(config.tick_secs);
        if self
            .last_observe
            .is_some_and(|last| now.saturating_sub(last) < tick)
        {
            return;
        }
        self.last_observe = Some(now);
        let headroom = headroom(snapshot, config);
        if headroom > margin(config).saturating_mul(2) {
            self.high_headroom_ticks = self.high_headroom_ticks.saturating_add(1);
        } else {
            self.high_headroom_ticks = 0;
        }
        let pressure = snapshot.cpu_pressure();
        if pressure >= config.tuning.cpu_psi_engage {
            self.psi_high_ticks = self.psi_high_ticks.saturating_add(1);
            self.psi_low_ticks = 0;
        } else {
            self.psi_high_ticks = 0;
            if pressure <= config.tuning.cpu_psi_release {
                self.psi_low_ticks = self.psi_low_ticks.saturating_add(1);
            } else {
                self.psi_low_ticks = 0;
            }
        }
    }

    /// Reservations follow `need - working_set`; drop once the unit has exited.
    fn update_reservations(&mut self, snapshot: &Snapshot, now: Duration) {
        self.expire(now);
        self.reservations
            .retain_mut(|r| match snapshot.units.get(&r.unit) {
                Some(&ws) => {
                    r.seen = true;
                    r.bytes = r.need.saturating_sub(ws);
                    true
                }
                None if r.seen => false,
                None => true,
            });
    }

    pub fn enqueue(&mut self, request: AdmitRequest) -> bool {
        if self.queue.len() >= QUEUE_CAP {
            return false;
        }
        self.queue.push_back(request);
        true
    }

    /// Apply the actions returned by `decide`/`admit` to the state the next tick reads.
    pub fn apply(&mut self, actions: &[Action], now: Duration) {
        self.expire(now);
        for action in actions {
            match action {
                Action::Write(write) => {
                    // design: §5.4 — a brake engage also restarts the per-slice write clock.
                    self.last_write.insert(write.slice.clone(), now);
                    match write.knob {
                        Knob::MemoryHigh => self.memory_high[write.class.index()] = write.to,
                        Knob::CpuQuota => {
                            self.clamp_cpus = write.to.map(|percent| (percent / 100).max(1) as u32);
                        }
                    }
                    if write.event == WriteEvent::BrakeOff {
                        self.last_release = Some(now);
                    }
                }
                Action::Admission(admission) => match admission.decision {
                    Decision::Admit | Decision::OverBudget => {
                        self.reserve(&admission.request.unit, admission.need, now);
                        if admission.from_queue
                            && self
                                .queue
                                .front()
                                .is_some_and(|request| request.unit == admission.request.unit)
                        {
                            self.queue.pop_front();
                        }
                    }
                    Decision::Queue { .. } => {
                        if !self
                            .queue
                            .iter()
                            .any(|request| request.unit == admission.request.unit)
                        {
                            let _ = self.enqueue(admission.request.clone());
                        }
                    }
                    Decision::WouldQueue | Decision::Reject => {}
                },
            }
        }
    }

    fn reserve(&mut self, unit: &str, need: u64, now: Duration) {
        self.reservations.retain(|r| r.unit != unit);
        if need > 0 {
            self.reservations.push(Reservation {
                unit: unit.to_string(),
                need,
                bytes: need,
                admitted_at: now,
                seen: false,
            });
        }
    }

    fn can_write(&self, slice: &str, now: Duration, config: &Config) -> bool {
        self.last_write.get(slice).is_none_or(|last| {
            now.saturating_sub(*last) >= Duration::from_secs(config.tuning.min_write_interval_secs)
        })
    }
}

fn cpus(snapshot: &Snapshot, config: &Config) -> u32 {
    snapshot
        .online_cpus
        .saturating_sub(config.reserve_cpus)
        .max(1)
}

/// §5.6 jobs hint. `available` is H' (headroom minus outstanding reservations).
pub fn jobs(
    snapshot: &Snapshot,
    config: &Config,
    need: u64,
    available: u64,
    hint: Option<u32>,
) -> u32 {
    let cpus = u64::from(cpus(snapshot, config));
    let grant = need.max(available.min(config.pool_memory_max.0));
    let by_memory = grant / config.mem_per_job.0.max(1);
    let requested = u64::from(hint.unwrap_or(u32::MAX));
    requested
        .min(cpus)
        .min(by_memory)
        .clamp(1, cpus)
        .try_into()
        .unwrap_or(1)
}

fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1 << 40, "T"),
        (1 << 30, "G"),
        (1 << 20, "M"),
        (1 << 10, "K"),
    ];
    for (scale, suffix) in UNITS {
        if bytes >= scale {
            return format!("{:.1}{suffix}", bytes as f64 / scale as f64);
        }
    }
    bytes.to_string()
}

fn admission(
    snapshot: &Snapshot,
    state: &State,
    config: &Config,
    request: &AdmitRequest,
    now: Duration,
    from_queue: bool,
) -> Admission {
    let need = request
        .mem_hint
        .unwrap_or(config.classes.default_mem(request.class).0);
    let headroom = headroom(snapshot, config);
    let reserved = state.outstanding(now);
    let available = headroom.saturating_sub(reserved);
    let margin = margin(config);
    let fits = available.saturating_sub(need) >= margin;
    let jobs = jobs(snapshot, config, need, available, request.jobs_hint);
    let comparison = if fits { ">=" } else { "<" };
    let reason = format!(
        "headroom {} - reserved {} {comparison} need {} + margin {}",
        fmt_bytes(headroom),
        fmt_bytes(reserved),
        fmt_bytes(need),
        fmt_bytes(margin)
    );

    // design: §5.4 — batch work that can never fit is rejected, never queued.
    let never_fit =
        request.class == Class::Batch && need.saturating_add(margin) > config.pool_memory_max.0;
    let decision = match request.class {
        Class::Interactive | Class::Normal => {
            if fits {
                Decision::Admit
            } else {
                Decision::OverBudget
            }
        }
        Class::Batch => {
            // design: §5.4 — a non-empty queue keeps later batch work behind it (FIFO).
            if never_fit {
                Decision::Reject
            } else if fits && (from_queue || state.queue.is_empty()) {
                Decision::Admit
            } else if !request.queue {
                Decision::WouldQueue
            } else if state.queue.len() >= QUEUE_CAP {
                Decision::Reject
            } else {
                Decision::Queue {
                    position: state.queue.len() + 1,
                }
            }
        }
    };

    let reason = match decision {
        Decision::Reject if never_fit => format!(
            "need {} + margin {} > pool cap {}: can never fit",
            fmt_bytes(need),
            fmt_bytes(margin),
            fmt_bytes(config.pool_memory_max.0)
        ),
        Decision::Queue { position } => format!("{reason}; queue position {position}"),
        Decision::WouldQueue => format!("{reason}; would queue (--no-queue)"),
        Decision::Reject => "queue full (32)".to_string(),
        Decision::Admit | Decision::OverBudget => reason,
    };

    Admission {
        request: request.clone(),
        decision,
        need,
        jobs,
        from_queue,
        reason,
    }
}

fn memory_brake(snapshot: &Snapshot, state: &State, config: &Config, now: Duration) -> Vec<Action> {
    let headroom = headroom(snapshot, config);
    let margin = margin(config);
    let mut actions = Vec::new();

    for (class, threshold) in [(Class::Batch, margin), (Class::Normal, margin / 2)] {
        if headroom < threshold && state.memory_high[class.index()].is_none() {
            // design: §5.4 — engaged once at ws + 64 MiB and not retargeted while held.
            let target = working_set(snapshot.class(class)).saturating_add(BRAKE_HEADROOM);
            let (label, bound) = if class == Class::Batch {
                ("margin", margin)
            } else {
                ("margin/2", threshold)
            };
            actions.push(Action::Write(Write {
                event: WriteEvent::BrakeOn,
                slice: config.class_slice(class),
                class,
                knob: Knob::MemoryHigh,
                from: None,
                to: Some(target),
                reason: format!(
                    "pool headroom {} < {label} {}",
                    fmt_bytes(headroom),
                    fmt_bytes(bound)
                ),
            }));
        }
    }

    if state.high_headroom_ticks >= BRAKE_RELEASE_TICKS {
        let interval = Duration::from_secs(config.tuning.min_write_interval_secs);
        let released_recently = state
            .last_release
            .is_some_and(|last| now.saturating_sub(last) < interval);
        if !released_recently
            && let Some(class) = [Class::Normal, Class::Batch]
                .into_iter()
                .find(|class| state.memory_high[class.index()].is_some())
        {
            let slice = config.class_slice(class);
            if state.can_write(&slice, now, config) {
                actions.push(Action::Write(Write {
                    event: WriteEvent::BrakeOff,
                    slice,
                    class,
                    knob: Knob::MemoryHigh,
                    from: state.memory_high[class.index()],
                    to: None,
                    reason: format!(
                        "pool headroom {} > 2 * margin {} for {} ticks",
                        fmt_bytes(headroom),
                        fmt_bytes(margin.saturating_mul(2)),
                        state.high_headroom_ticks
                    ),
                }));
            }
        }
    }

    actions
}

fn cpu_clamp(snapshot: &Snapshot, state: &State, config: &Config, now: Duration) -> Vec<Action> {
    let cpus = cpus(snapshot, config);
    let batch = Class::Batch;
    let slice = config.class_slice(batch);
    let pressure = snapshot.cpu_pressure();
    let mut actions = Vec::new();

    if state.psi_high_ticks >= CLAMP_ENGAGE_TICKS {
        let next = state.clamp_cpus.unwrap_or(cpus).saturating_sub(1).max(1);
        if Some(next) != state.clamp_cpus && state.can_write(&slice, now, config) {
            actions.push(Action::Write(Write {
                event: WriteEvent::Clamp,
                slice,
                class: batch,
                knob: Knob::CpuQuota,
                from: state.clamp_cpus.map(|cpus| u64::from(cpus) * 100),
                to: Some(u64::from(next) * 100),
                reason: format!(
                    "cpu pressure {pressure:.1}% >= {}% for {} ticks",
                    config.tuning.cpu_psi_engage, state.psi_high_ticks
                ),
            }));
        }
    } else if state.psi_low_ticks >= CLAMP_RELEASE_TICKS
        && let Some(current) = state.clamp_cpus
    {
        let next = if current + 1 >= cpus {
            None
        } else {
            Some(current + 1)
        };
        if state.can_write(&slice, now, config) {
            actions.push(Action::Write(Write {
                event: WriteEvent::Unclamp,
                slice,
                class: batch,
                knob: Knob::CpuQuota,
                from: Some(u64::from(current) * 100),
                to: next.map(|cpus| u64::from(cpus) * 100),
                reason: format!(
                    "cpu pressure {pressure:.1}% <= {}% for {} ticks",
                    config.tuning.cpu_psi_release, state.psi_low_ticks
                ),
            }));
        }
    }

    actions
}

/// Tick decisions: Rule M, Rule C and the queue head. One batch admission per call.
pub fn decide(
    snapshot: &Snapshot,
    state: &mut State,
    config: &Config,
    now: Duration,
) -> Vec<Action> {
    state.observe(snapshot, config, now);
    state.update_reservations(snapshot, now);
    let mut actions = memory_brake(snapshot, state, config, now);
    let batch = config.class_slice(Class::Batch);
    let batch_written = actions.iter().any(|action| {
        matches!(action, Action::Write(write) if write.slice == batch && write.event != WriteEvent::BrakeOn)
    });
    if !batch_written {
        actions.extend(cpu_clamp(snapshot, state, config, now));
    }
    if let Some(request) = state.queue.front() {
        let admission = admission(snapshot, state, config, request, now, true);
        if admission.decision == Decision::Admit {
            actions.push(Action::Admission(admission));
        }
    }
    actions
}

/// Rule A for one launch request. Rule M runs on the loop after an over-budget reply.
pub fn admit(
    snapshot: &Snapshot,
    state: &State,
    config: &Config,
    request: &AdmitRequest,
    now: Duration,
) -> Admission {
    admission(snapshot, state, config, request, now, false)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const GIB: u64 = 1 << 30;

    fn config() -> Config {
        let mut config = Config::default();
        config.pool_memory_max = crate::config::Size(8 * GIB);
        config.system_reserve = crate::config::Size(GIB);
        config.mem_per_job = crate::config::Size(GIB);
        config.reserve_cpus = 1;
        config
    }

    fn stats(current: u64, inactive: u64, cpu: f32) -> SliceStats {
        SliceStats {
            memory_current: current,
            inactive_file: inactive,
            cpu_pressure: Some(crate::cgroup::Psi {
                some_avg10: cpu,
                full_avg10: None,
            }),
            memory_pressure: None,
        }
    }

    fn make_snapshot(pool_used: u64, mem_available: u64, batch_used: u64) -> Snapshot {
        let mut snapshot = Snapshot {
            mem_available,
            online_cpus: 5,
            pool: stats(pool_used, 0, 0.0),
            ..Snapshot::default()
        };
        snapshot.classes[Class::Batch.index()] = stats(batch_used, 0, 0.0);
        snapshot
    }

    fn batch_request(need: u64) -> AdmitRequest {
        AdmitRequest {
            unit: "slicewise-batch-1-1.service".into(),
            class: Class::Batch,
            mem_hint: Some(need),
            jobs_hint: None,
            queue: true,
        }
    }

    fn writes(actions: &[Action]) -> impl Iterator<Item = &Write> {
        actions.iter().filter_map(|action| match action {
            Action::Write(write) => Some(write),
            Action::Admission(_) => None,
        })
    }

    #[test]
    fn headroom_uses_pool_and_system_minimum() {
        let config = config();
        let snapshot = make_snapshot(4 * GIB, 7 * GIB, 0);
        assert_eq!(working_set(&snapshot.pool), 4 * GIB);
        assert_eq!(headroom(&snapshot, &config), 4 * GIB);
        let snapshot = make_snapshot(2 * GIB, GIB / 2, 0);
        assert_eq!(headroom(&snapshot, &config), 0);
    }

    #[test]
    fn margin_has_a_512m_floor() {
        let mut config = config();
        config.pool_memory_max = crate::config::Size(4 * GIB);
        assert_eq!(margin(&config), MIN_BRAKE_MARGIN);
    }

    #[test]
    fn admission_admits_when_headroom_fits() {
        let config = config();
        let state = State::default();
        let snapshot = make_snapshot(4 * GIB, 8 * GIB, 0);
        let admission = admit(
            &snapshot,
            &state,
            &config,
            &batch_request(2 * GIB),
            Duration::ZERO,
        );
        assert_eq!(admission.decision, Decision::Admit);
        assert_eq!(admission.need, 2 * GIB);
        assert_eq!(admission.jobs, 4);
        assert!(admission.reason.contains(">="), "{}", admission.reason);
    }

    #[test]
    fn admission_queues_no_queue_and_full() {
        let config = config();
        let state = State::default();
        let snapshot = make_snapshot(7 * GIB, 2 * GIB, 0);
        let admission = admit(
            &snapshot,
            &state,
            &config,
            &batch_request(4 * GIB),
            Duration::ZERO,
        );
        assert_eq!(admission.decision, Decision::Queue { position: 1 });

        let mut no_queue = batch_request(4 * GIB);
        no_queue.queue = false;
        let admission = admit(&snapshot, &state, &config, &no_queue, Duration::ZERO);
        assert_eq!(admission.decision, Decision::WouldQueue);
        assert!(admission.reason.contains("--no-queue"));

        let mut full = State::default();
        for n in 0..QUEUE_CAP {
            assert!(full.enqueue(AdmitRequest {
                unit: format!("slicewise-batch-1-{n}.service"),
                ..batch_request(4 * GIB)
            }));
        }
        let admission = admit(
            &snapshot,
            &full,
            &config,
            &batch_request(4 * GIB),
            Duration::ZERO,
        );
        assert_eq!(admission.decision, Decision::Reject);
        assert_eq!(admission.reason, "queue full (32)");
    }

    #[test]
    fn batch_that_can_never_fit_is_rejected() {
        let config = config();
        let admission = admit(
            &make_snapshot(0, 8 * GIB, 0),
            &State::default(),
            &config,
            &batch_request(8 * GIB),
            Duration::ZERO,
        );
        assert_eq!(admission.decision, Decision::Reject);
        assert_eq!(
            admission.reason,
            "need 8.0G + margin 512.0M > pool cap 8.0G: can never fit"
        );
    }

    #[test]
    fn interactive_and_normal_are_over_budget_but_admitted() {
        let config = config();
        let state = State::default();
        let snapshot = make_snapshot(8 * GIB - 100 * (1 << 20), 8 * GIB, 0);
        for class in [Class::Interactive, Class::Normal] {
            let request = AdmitRequest {
                unit: format!("slicewise-{}-1-1.service", class.as_str()),
                class,
                mem_hint: Some(4 * GIB),
                jobs_hint: None,
                queue: true,
            };
            let admission = admit(&snapshot, &state, &config, &request, Duration::ZERO);
            assert_eq!(admission.decision, Decision::OverBudget);
            assert_eq!(admission.event(), "over_budget");
        }
    }

    #[test]
    fn queue_is_fifo_and_reservations_expire() {
        let config = config();
        let mut state = State::default();
        let snapshot = make_snapshot(7 * GIB, 2 * GIB, 0);
        assert!(state.enqueue(batch_request(4 * GIB)));
        assert!(state.enqueue(AdmitRequest {
            unit: "slicewise-batch-1-2.service".into(),
            ..batch_request(4 * GIB)
        }));
        let actions = decide(&snapshot, &mut state, &config, Duration::ZERO);
        assert!(writes(&actions).next().is_none());
        assert_eq!(state.queue.len(), 2);

        state.reservations.push(Reservation {
            unit: "old.service".into(),
            need: GIB,
            bytes: GIB,
            admitted_at: Duration::ZERO,
            seen: false,
        });
        assert_eq!(state.outstanding(Duration::from_secs(299)), GIB);
        assert_eq!(state.outstanding(RESERVATION_TTL), GIB);
        state.expire(RESERVATION_TTL);
        assert!(state.reservations.is_empty());
    }

    #[test]
    fn jobs_hint_clamps_by_cpus_memory_and_request() {
        let config = config();
        let snapshot = make_snapshot(0, 8 * GIB, 0);
        assert_eq!(jobs(&snapshot, &config, 3 * GIB, 8 * GIB, None), 4);
        assert_eq!(jobs(&snapshot, &config, 3 * GIB, 8 * GIB, Some(2)), 2);
        assert_eq!(jobs(&snapshot, &config, 3 * GIB, GIB, None), 3);
        assert_eq!(jobs(&snapshot, &config, 0, 0, Some(4096)), 1);
        let mut small = snapshot.clone();
        small.online_cpus = 1;
        assert_eq!(jobs(&small, &config, 8 * GIB, 8 * GIB, None), 1);
    }

    #[test]
    fn brake_engages_batch_then_normal_and_never_interactive() {
        let config = config();
        let mut state = State::default();
        let snapshot = make_snapshot(8 * GIB - 400 * (1 << 20), 8 * GIB, 200 * (1 << 20));
        let actions = decide(&snapshot, &mut state, &config, Duration::ZERO);
        let brakes: Vec<_> = writes(&actions)
            .filter(|write| write.event == WriteEvent::BrakeOn)
            .collect();
        assert_eq!(brakes.len(), 1);
        assert_eq!(brakes[0].class, Class::Batch);
        assert_eq!(brakes[0].to, Some(200 * (1 << 20) + BRAKE_HEADROOM));
        state.apply(&actions, Duration::ZERO);

        let snapshot = make_snapshot(8 * GIB - 200 * (1 << 20), 8 * GIB, 0);
        let actions = decide(&snapshot, &mut state, &config, Duration::from_secs(2));
        let brakes: Vec<_> = writes(&actions)
            .filter(|write| write.event == WriteEvent::BrakeOn)
            .collect();
        assert_eq!(brakes.len(), 1);
        assert_eq!(brakes[0].class, Class::Normal);
        assert!(state.memory_high[Class::Interactive.index()].is_none());
    }

    #[test]
    fn brake_target_is_never_below_the_working_set() {
        let config = config();
        let mut state = State::default();
        let used = 1234 * (1 << 20);
        let snapshot = make_snapshot(8 * GIB - 100 * (1 << 20), 8 * GIB, used);
        let actions = decide(&snapshot, &mut state, &config, Duration::ZERO);
        let write = writes(&actions).next().unwrap();
        let target = write.to.expect("brake target is bytes");
        assert!(target >= working_set(snapshot.class(Class::Batch)));
    }

    #[test]
    fn brake_releases_normal_first_one_class_per_interval() {
        let config = config();
        let mut state = State::default();
        state.memory_high[Class::Normal.index()] = Some(GIB);
        state.memory_high[Class::Batch.index()] = Some(GIB);
        let snapshot = make_snapshot(0, 8 * GIB, 0);
        for tick in 0..BRAKE_RELEASE_TICKS - 1 {
            let actions = decide(
                &snapshot,
                &mut state,
                &config,
                Duration::from_secs(u64::from(tick) * 2),
            );
            assert!(writes(&actions).next().is_none(), "tick {tick}");
        }
        let actions = decide(&snapshot, &mut state, &config, Duration::from_secs(8));
        let releases: Vec<_> = writes(&actions)
            .filter(|write| write.event == WriteEvent::BrakeOff)
            .collect();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].class, Class::Normal);
        state.apply(&actions, Duration::from_secs(8));

        let actions = decide(&snapshot, &mut state, &config, Duration::from_secs(12));
        assert!(
            writes(&actions).all(|write| write.event != WriteEvent::BrakeOff),
            "second release waits one interval"
        );
        let actions = decide(&snapshot, &mut state, &config, Duration::from_secs(18));
        let releases: Vec<_> = writes(&actions)
            .filter(|write| write.event == WriteEvent::BrakeOff)
            .collect();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].class, Class::Batch);
    }

    #[test]
    fn rate_limit_blocks_a_second_write_but_not_brake_engage() {
        let config = config();
        let snapshot = make_snapshot(8 * GIB, 8 * GIB, 0);
        let mut state = State::default();
        let slice = config.class_slice(Class::Batch);
        assert!(state.can_write(&slice, Duration::ZERO, &config));
        state.last_write.insert(slice.clone(), Duration::ZERO);
        assert!(!state.can_write(&slice, Duration::from_secs(9), &config));
        assert!(state.can_write(&slice, Duration::from_secs(10), &config));
        assert_eq!(working_set(&snapshot.pool), 8 * GIB);
    }

    #[test]
    fn clamp_engages_after_two_ticks_and_never_below_one_cpu() {
        let config = config();
        let mut state = State::default();
        let mut snapshot = make_snapshot(0, 8 * GIB, 0);
        snapshot.online_cpus = 4;
        snapshot.classes[Class::Interactive.index()] = stats(0, 0, 25.0);
        let actions = decide(&snapshot, &mut state, &config, Duration::ZERO);
        assert!(writes(&actions).next().is_none(), "one tick is not enough");
        let actions = decide(&snapshot, &mut state, &config, Duration::from_secs(2));
        let clamp = writes(&actions).next().unwrap();
        assert_eq!(clamp.event, WriteEvent::Clamp);
        assert_eq!(clamp.from, None);
        assert_eq!(clamp.to, Some(200));
        state.apply(&actions, Duration::from_secs(2));

        let actions = decide(&snapshot, &mut state, &config, Duration::from_secs(12));
        assert_eq!(writes(&actions).next().unwrap().to, Some(100));
        state.apply(&actions, Duration::from_secs(12));
        let actions = decide(&snapshot, &mut state, &config, Duration::from_secs(22));
        assert!(writes(&actions).next().is_none(), "100% is the floor");
        assert_eq!(state.clamp_cpus, Some(1));
    }

    #[test]
    fn clamp_releases_after_fifteen_low_ticks_then_every_interval() {
        let config = config();
        let mut state = State {
            clamp_cpus: Some(1),
            ..State::default()
        };
        let mut snapshot = make_snapshot(0, 8 * GIB, 0);
        snapshot.online_cpus = 5;
        snapshot.classes[Class::Interactive.index()] = stats(0, 0, 1.0);
        let step = Duration::from_secs(config.tick_secs);
        let mut now = Duration::ZERO;
        for _ in 0..CLAMP_RELEASE_TICKS - 1 {
            now += step;
            let actions = decide(&snapshot, &mut state, &config, now);
            assert!(writes(&actions).all(|write| write.event != WriteEvent::Unclamp));
        }
        now += step;
        let actions = decide(&snapshot, &mut state, &config, now);
        let unclamp = writes(&actions).next().unwrap();
        assert_eq!(unclamp.event, WriteEvent::Unclamp);
        assert_eq!(unclamp.from, Some(100));
        assert_eq!(unclamp.to, Some(200));
        state.apply(&actions, now);

        now += Duration::from_secs(10);
        let actions = decide(&snapshot, &mut state, &config, now);
        assert_eq!(writes(&actions).next().unwrap().to, Some(300));
        state.apply(&actions, now);
        now += Duration::from_secs(10);
        let actions = decide(&snapshot, &mut state, &config, now);
        assert_eq!(writes(&actions).next().unwrap().to, None);
    }

    #[test]
    fn queue_head_is_admitted_once_it_fits() {
        let config = config();
        let mut state = State::default();
        assert!(state.enqueue(batch_request(2 * GIB)));
        let low = make_snapshot(8 * GIB, 8 * GIB, 0);
        let actions = decide(&low, &mut state, &config, Duration::ZERO);
        assert!(
            actions
                .iter()
                .all(|action| !matches!(action, Action::Admission(_))),
            "no fit, no admission"
        );
        let high = make_snapshot(0, 8 * GIB, 0);
        let actions = decide(&high, &mut state, &config, Duration::from_secs(2));
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].event(), "admit");
        state.apply(&actions, Duration::from_secs(2));
        assert!(state.queue.is_empty());
        assert_eq!(state.outstanding(Duration::from_secs(2)), 2 * GIB);
    }

    #[test]
    fn one_write_per_tick_to_the_batch_slice() {
        let config = config();
        let now = Duration::from_secs(10);
        let mut state = State {
            memory_high: [None, None, Some(GIB)],
            clamp_cpus: Some(1),
            high_headroom_ticks: BRAKE_RELEASE_TICKS,
            psi_low_ticks: CLAMP_RELEASE_TICKS,
            last_observe: Some(now),
            ..State::default()
        };
        state
            .last_write
            .insert(config.class_slice(Class::Batch), Duration::ZERO);
        let mut snapshot = make_snapshot(0, 8 * GIB, 0);
        snapshot.classes[Class::Interactive.index()] = stats(0, 0, 1.0);

        let actions = decide(&snapshot, &mut state, &config, now);
        let writes: Vec<_> = writes(&actions).collect();
        assert_eq!(writes.len(), 1, "only the brake release may be written");
        assert_eq!(writes[0].event, WriteEvent::BrakeOff);
        assert_eq!(writes[0].slice, config.class_slice(Class::Batch));
    }

    #[test]
    fn reservation_shrinks_as_unit_grows_and_drops_on_exit() {
        let config = config();
        let tick = Duration::from_secs(config.tick_secs);
        let mut state = State::default();
        let request = batch_request(2 * GIB);
        let mut snapshot = make_snapshot(0, 8 * GIB, 0);
        let admission = admit(&snapshot, &state, &config, &request, Duration::ZERO);
        assert_eq!(admission.decision, Decision::Admit);
        state.apply(&[Action::Admission(admission)], Duration::ZERO);
        assert_eq!(state.reservations[0].bytes, 2 * GIB);
        assert!(!state.reservations[0].seen);

        snapshot.units.insert(request.unit.clone(), GIB / 2);
        let _ = decide(&snapshot, &mut state, &config, tick);
        assert!(state.reservations[0].seen);
        assert_eq!(state.reservations[0].bytes, 2 * GIB - GIB / 2);

        snapshot.units.insert(request.unit.clone(), 3 * GIB / 2);
        let _ = decide(&snapshot, &mut state, &config, 2 * tick);
        assert_eq!(state.reservations[0].bytes, GIB / 2);

        snapshot.units.clear();
        let _ = decide(&snapshot, &mut state, &config, 3 * tick);
        assert!(state.reservations.is_empty());
    }

    #[test]
    fn invariant_loop_over_ten_thousand_lcg_sequences() {
        let config = config();
        let tick = Duration::from_secs(config.tick_secs);
        let margin = margin(&config);
        let mut lcg = 0x5eed_u64;
        let next = |lcg: &mut u64| {
            *lcg = lcg
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *lcg >> 33
        };

        for sequence in 0..10_000_u32 {
            let mut state = State::default();
            if sequence % 2 == 0 {
                state.enqueue(batch_request(2 * GIB));
            }
            let mut now = Duration::ZERO;
            let mut seen: BTreeMap<String, Duration> = BTreeMap::new();
            let mut brake_engaged: Option<Duration> = None;
            let mut clamp_engaged: Option<Duration> = None;
            let mut high = next(&mut lcg) % 2 == 0;
            let mut run = 0_u32;
            let mut run_ticks = 1 + (next(&mut lcg) % 20) as u32;
            let admit_tick = next(&mut lcg) % 60;

            for tick_index in 0..60_u64 {
                now += tick;
                if run >= run_ticks {
                    high = !high;
                    run = 0;
                    run_ticks = 1 + (next(&mut lcg) % 20) as u32;
                }
                run += 1;

                let target = if high {
                    margin
                        .saturating_mul(3)
                        .saturating_add(next(&mut lcg) % (1024 * 1024))
                } else {
                    next(&mut lcg) % margin.max(1)
                };
                let pool_used = config.pool_memory_max.0.saturating_sub(target);
                let mem_available = config.system_reserve.0.saturating_add(target);
                let mut snapshot =
                    make_snapshot(pool_used, mem_available, next(&mut lcg) % (2 * GIB));
                snapshot.online_cpus = 2 + (next(&mut lcg) % 7) as u32;
                snapshot.classes[Class::Interactive.index()] =
                    stats(0, 0, if high { 25.0 } else { 1.0 });
                snapshot.classes[Class::Normal.index()] = stats(next(&mut lcg) % GIB, 0, 0.0);

                let actions = decide(&snapshot, &mut state, &config, now);

                for write in writes(&actions) {
                    if write.knob == Knob::MemoryHigh {
                        assert_ne!(
                            write.class,
                            Class::Interactive,
                            "interactive is never braked"
                        );
                        if let Some(bytes) = write.to {
                            assert!(
                                bytes >= working_set(snapshot.class(write.class)),
                                "MemoryHigh {bytes} below working set"
                            );
                        }
                    }
                    if write.knob == Knob::CpuQuota
                        && let Some(percent) = write.to
                    {
                        assert!(percent >= 100, "clamp below one CPU");
                    }
                    if write.event != WriteEvent::BrakeOn {
                        if let Some(last) = seen.get(&write.slice) {
                            assert!(
                                now.saturating_sub(*last) >= Duration::from_secs(10),
                                "two writes to {} within 10 s",
                                write.slice
                            );
                        }
                        seen.insert(write.slice.clone(), now);
                    }
                    match write.event {
                        WriteEvent::BrakeOn => brake_engaged = Some(now),
                        WriteEvent::Clamp => clamp_engaged = Some(now),
                        // A short run that still releases means a counter leaked across runs.
                        WriteEvent::BrakeOff if run < BRAKE_RELEASE_TICKS => {
                            if let Some(engaged) = brake_engaged {
                                assert!(
                                    now.saturating_sub(engaged) >= Duration::from_secs(30),
                                    "brake released {} after engage on a short run",
                                    write.slice
                                );
                            }
                        }
                        WriteEvent::Unclamp if run < CLAMP_RELEASE_TICKS => {
                            if let Some(engaged) = clamp_engaged {
                                assert!(
                                    now.saturating_sub(engaged) >= Duration::from_secs(30),
                                    "clamp released {} after engage on a short run",
                                    write.slice
                                );
                            }
                        }
                        _ => {}
                    }
                }
                state.apply(&actions, now);

                if tick_index == admit_tick {
                    let admission = admit(
                        &snapshot,
                        &state,
                        &config,
                        &batch_request(1 + next(&mut lcg) % (4 * GIB)),
                        now,
                    );
                    state.apply(&[Action::Admission(admission)], now);
                }
            }
        }
    }
}

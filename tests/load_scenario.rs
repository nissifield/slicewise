//! M1c load scenario (design §10, criteria 1–7). Live-gated: `#[ignore]` and a
//! `SKIP` unless both `SLICEWISE_LIVE=1` and `SLICEWISE_LOAD=1`.
//!
//! Every test builds its own test-named static frame (`app-slicewisetest…`),
//! private `$XDG_RUNTIME_DIR`, rendered unit files and a daemon child, then
//! measures cgroupfs, the daemon's log and `examples/hog.rs`. The frame uses the
//! production default config: `C = online − Config::default().reserve_cpus`, and
//! `P = min(50 % of MemTotal, 4 GiB)` becomes the pool's `pool_memory_max`, so
//! the kernel enforces every size below. `Drop` reverts the slices and removes
//! the unit files on every path.

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use slicewise::cgroup;
use slicewise::systemd::systemctl;

const MARK: &str = "slicewisetest";
const BIN: &str = env!("CARGO_BIN_EXE_slicewise");
const TICK: u64 = 2;

static NEXT: AtomicU32 = AtomicU32::new(0);

fn skip(test: &str) -> bool {
    if std::env::var("SLICEWISE_LIVE").as_deref() != Ok("1")
        || std::env::var("SLICEWISE_LOAD").as_deref() != Ok("1")
    {
        println!("SKIP {test}: set SLICEWISE_LIVE=1 and SLICEWISE_LOAD=1");
        return true;
    }
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        println!("SKIP {test}: XDG_RUNTIME_DIR is unset");
        return true;
    }
    false
}

/// `cargo test` builds examples as test harnesses, not executables; build the
/// load generator once, in the profile `BIN` came from.
fn hog() -> &'static Path {
    static HOG: OnceLock<PathBuf> = OnceLock::new();
    HOG.get_or_init(|| {
        let profile = Path::new(BIN).parent().expect("binary parent");
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut build = Command::new(cargo);
        build.args(["build", "--example", "hog", "--offline", "--locked"]);
        if profile.file_name().is_some_and(|name| name == "release") {
            build.arg("--release");
        }
        let status = build
            .arg("--target-dir")
            .arg(profile.parent().expect("target dir"))
            .status()
            .expect("build examples/hog");
        assert!(status.success(), "cargo build --example hog failed");
        let path = profile.join("examples/hog");
        assert!(path.is_file(), "missing {}", path.display());
        path
    })
}

/// Any mutating systemctl call must name a unit this test owns.
fn mutate(args: &[&str]) -> Result<String, String> {
    assert!(
        args.iter().any(|arg| arg.contains(MARK)),
        "refusing mutating systemctl call without a {MARK} unit"
    );
    systemctl(args)
}

fn control_group(unit: &str) -> Option<String> {
    systemctl(&["--user", "show", "--value", "-p", "ControlGroup", unit])
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn wait_for(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    wait_value(timeout, || ready().then_some(())).is_some()
}

fn wait_value<T>(timeout: Duration, mut value: impl FnMut() -> Option<T>) -> Option<T> {
    let started = Instant::now();
    loop {
        if let Some(value) = value() {
            return Some(value);
        }
        if started.elapsed() >= timeout {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Whitespace split, for `examples/hog` argv.
fn args(spec: &str) -> Vec<String> {
    spec.split_whitespace().map(str::to_string).collect()
}

fn online_cpus() -> u32 {
    cgroup::online_cpus(Path::new("/sys/devices/system/cpu/online")).unwrap_or(1)
}

fn find_systemctl() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("systemctl"))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("/usr/bin/systemctl"))
}

/// One `key value` line from a cgroup stats file.
fn key(rel: &str, file: &str, name: &str) -> u64 {
    cgroup::read_trimmed(Path::new(cgroup::CGROUP_ROOT), &format!("{rel}/{file}"))
        .ok()
        .map(|text| {
            cgroup::parse_memory_stat(&text)
                .get(name)
                .copied()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

struct Logs {
    lines: Arc<Mutex<Vec<(Duration, String)>>>,
}

impl Logs {
    fn spawn(stderr: std::process::ChildStderr, start: Instant) -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                sink.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push((start.elapsed(), line));
            }
        });
        Self { lines }
    }

    fn entries(&self) -> Vec<(Duration, String)> {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn actions(&self) -> Vec<(Duration, String)> {
        self.entries()
            .into_iter()
            .filter_map(|(at, line)| {
                let event = line
                    .split_whitespace()
                    .find_map(|field| field.strip_prefix("event="))?;
                matches!(event, "clamp" | "unclamp" | "brake_on" | "brake_off")
                    .then(|| (at, event.to_string()))
            })
            .collect()
    }
}

struct Fixture {
    pool: String,
    classes: [String; 3],
    class_rel: [String; 3],
    unit_dir: PathBuf,
    rendered: common::TempDir,
    config_home: common::TempDir,
    runtime: common::TempDir,
    daemon: Option<Child>,
    daemon_unit: String,
    jobs: Vec<String>,
    files: Vec<PathBuf>,
    logs: Option<Logs>,
    c: u32,
    p: u64,
}

impl Fixture {
    fn new(test: &str) -> Option<Self> {
        let mem_total = cgroup::mem_total_bytes(Path::new(cgroup::PROC_ROOT)).expect("MemTotal");
        let p = (mem_total / 2).min(4 << 30);
        assert!(
            p > (512 << 20),
            "{test}: host too small; a pool of {p} bytes is below the 512 MiB margin"
        );
        // The suite drives the pool to P; never let a full host trip its global OOM killer.
        let available =
            cgroup::mem_available_bytes(Path::new(cgroup::PROC_ROOT)).unwrap_or_default();
        if available < p.saturating_add(1 << 30) {
            println!("SKIP {test}: MemAvailable {available} < P + 1 GiB");
            return None;
        }
        let c = online_cpus()
            .saturating_sub(slicewise::config::Config::default().reserve_cpus)
            .max(1);

        let real = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .expect("skip() checked XDG_RUNTIME_DIR");
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let stem = format!("{MARK}{}x{n}", std::process::id());
        let pool = format!("app-{stem}.slice");
        let runtime = common::TempDir::new("load-runtime");
        fs::create_dir_all(runtime.path().join("systemd")).expect("runtime systemd dir");
        symlink(real.join("bus"), runtime.path().join("bus")).expect("link bus");
        symlink(
            real.join("systemd/private"),
            runtime.path().join("systemd/private"),
        )
        .expect("link systemd/private");
        let config_home = common::TempDir::new("load-config");
        let config_dir = config_home.path().join("slicewise");
        fs::create_dir_all(&config_dir).expect("config dir");
        fs::write(
            config_dir.join("config.toml"),
            format!(
                "pool_slice = \"{pool}\"\npool_memory_max = {p}\nsystem_reserve = \"256M\"\n\
                 tick_secs = {TICK}\nmem_per_job = \"1G\"\n"
            ),
        )
        .expect("write config");

        // Criterion 7: the daemon's own systemctl calls go through this shim, so
        // actual `set-property` invocations are countable without touching `src/`.
        let shim = runtime.path().join("shim");
        fs::create_dir_all(&shim).expect("shim dir");
        let script = "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SLICEWISE_TEST_SHIM_LOG\"\n\
                      exec \"$SLICEWISE_TEST_REAL_SYSTEMCTL\" \"$@\"\n";
        let path = shim.join("systemctl");
        fs::write(&path, script).expect("write systemctl shim");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod shim");

        let classes = [
            format!("app-{stem}-interactive.slice"),
            format!("app-{stem}-normal.slice"),
            format!("app-{stem}-batch.slice"),
        ];
        Some(Self {
            pool,
            classes,
            class_rel: [String::new(), String::new(), String::new()],
            unit_dir: common::user_unit_dir(),
            rendered: common::TempDir::new("load-units"),
            config_home,
            runtime,
            daemon: None,
            daemon_unit: String::new(),
            jobs: Vec::new(),
            files: Vec::new(),
            logs: None,
            c,
            p,
        })
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command.env("XDG_RUNTIME_DIR", self.runtime.path());
        command.env("XDG_CONFIG_HOME", self.config_home.path());
        command
    }

    fn slicewise(&self, args: &[&str]) -> std::process::Output {
        self.command(BIN)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("run slicewise {args:?}: {error}"))
    }

    fn assert_config(&self) {
        let output = self.slicewise(&["check-config"]);
        assert!(
            output.status.success(),
            "check-config failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&self.pool),
            "check-config does not print the test pool {}",
            self.pool
        );
    }

    fn install_units(&mut self) {
        let rendered = self.rendered.path().to_str().expect("utf-8 path");
        let output = self.slicewise(&["units", rendered]);
        assert!(
            output.status.success(),
            "units failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::create_dir_all(&self.unit_dir).expect("user unit dir");
        for name in std::iter::once(&self.pool).chain(self.classes.iter()) {
            let target = self.unit_dir.join(name);
            fs::copy(self.rendered.path().join(name), &target).expect("copy rendered unit");
            self.files.push(target);
        }
        systemctl(&["--user", "daemon-reload"]).expect("daemon-reload after writing units");
        let mut start = vec!["--user", "start", self.pool.as_str()];
        start.extend(self.classes.iter().map(String::as_str));
        mutate(&start).expect("start test slices");
        for (index, class) in self.classes.iter().enumerate() {
            self.class_rel[index] = wait_value(Duration::from_secs(5), || control_group(class))
                .unwrap_or_else(|| panic!("no cgroup for {class}"));
        }
        // Without the unit files systemd creates implicit slices and silently
        // drops every rendered property, so prove the manager loaded them.
        let expected = slicewise::config::Config::default()
            .classes
            .cpu_weight(slicewise::config::Class::Batch)
            .to_string();
        let weight = systemctl(&[
            "--user",
            "show",
            "--value",
            "-p",
            "CPUWeight",
            &self.classes[2],
        ])
        .unwrap_or_default();
        assert_eq!(
            weight, expected,
            "the manager did not load {}",
            self.classes[2]
        );
    }

    fn start_child_daemon(&mut self) {
        let start = Instant::now();
        let mut command = self.command(BIN);
        command
            .arg("daemon")
            .env("PATH", self.runtime.path().join("shim"))
            .env(
                "SLICEWISE_TEST_SHIM_LOG",
                self.runtime.path().join("systemctl.log"),
            )
            .env("SLICEWISE_TEST_REAL_SYSTEMCTL", find_systemctl())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn daemon");
        self.logs = Some(Logs::spawn(
            child.stderr.take().expect("daemon stderr"),
            start,
        ));
        self.daemon = Some(child);
        let socket = self.runtime.path().join("slicewise.sock");
        assert!(
            wait_for(Duration::from_secs(5), || socket.exists()),
            "no socket"
        );
    }

    fn start_service_daemon(&mut self) {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let unit = format!("{MARK}-daemon-{}x{n}", std::process::id());
        let stop = format!("ExecStopPost={BIN} reset");
        let runtime = format!("XDG_RUNTIME_DIR={}", self.runtime.path().display());
        let config = format!("XDG_CONFIG_HOME={}", self.config_home.path().display());
        let output = self
            .command("systemd-run")
            .args([
                "--user",
                "--unit",
                &unit,
                "-p",
                &stop,
                &format!("--setenv={runtime}"),
                &format!("--setenv={config}"),
                "--",
                BIN,
                "daemon",
            ])
            .output()
            .expect("systemd-run the daemon");
        assert!(
            output.status.success(),
            "systemd-run daemon failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.daemon_unit = format!("{unit}.service");
        let socket = self.runtime.path().join("slicewise.sock");
        assert!(
            wait_for(Duration::from_secs(5), || socket.exists()),
            "service daemon socket did not appear"
        );
    }

    fn job(&mut self, tag: &str) -> (String, String) {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("{MARK}-{tag}-{}-{n}", std::process::id());
        let unit = format!("{name}.service");
        self.jobs.push(unit.clone());
        (name, unit)
    }

    /// `slicewise run` argv shared by launch, spawn_run and spawn_probe.
    fn run_command(
        &self,
        class: &str,
        mem_hint: u64,
        systemd_args: &[&str],
        name: &str,
        hog_args: &[String],
    ) -> Command {
        let mem_hint = mem_hint.to_string();
        let mut command = self.command(BIN);
        command
            .args(["run", "--class", class, "--mem-hint", &mem_hint, "--"])
            .args(systemd_args)
            .arg(format!("--unit={name}"))
            .arg(hog())
            .args(hog_args)
            .stdin(Stdio::null());
        command
    }

    /// Launch a hog in `class` and wait for `systemd-run` to return (admitted).
    fn launch(&mut self, tag: &str, class: &str, mem_hint: u64, hog_args: &[String]) -> String {
        let (name, unit) = self.job(tag);
        let output = self
            .run_command(class, mem_hint, &[], &name, hog_args)
            .output()
            .unwrap_or_else(|error| panic!("launch {tag}: {error}"));
        assert!(
            output.status.success(),
            "run {tag} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        unit
    }

    fn spawn_run(
        &mut self,
        tag: &str,
        class: &str,
        mem_hint: u64,
        hog_args: &[String],
    ) -> (Child, String) {
        let (name, unit) = self.job(tag);
        let child = self
            .run_command(class, mem_hint, &[], &name, hog_args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {tag}: {error}"));
        (child, unit)
    }

    fn spawn_probe(&mut self, tag: &str, secs: u64) -> (Child, String) {
        let (name, unit) = self.job(tag);
        let child = self
            .run_command(
                "interactive",
                64 << 20,
                &["--pipe"],
                &name,
                &args(&format!("probe --secs {secs}")),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn probe {tag}: {error}"));
        (child, unit)
    }

    fn probe(&mut self, tag: &str, secs: u64) -> (u64, u64) {
        let (child, unit) = self.spawn_probe(tag, secs);
        assert!(
            wait_value(Duration::from_secs(5), || control_group(&unit)).is_some(),
            "probe {tag} did not start"
        );
        finish_probe(child)
    }

    fn read(&self, class: usize, file: &str) -> Option<String> {
        cgroup::read_trimmed(
            Path::new(cgroup::CGROUP_ROOT),
            &format!("{}/{file}", self.class_rel[class]),
        )
        .ok()
    }

    fn number(&self, class: usize, file: &str) -> u64 {
        self.read(class, file)
            .and_then(|text| text.parse().ok())
            .unwrap_or(0)
    }

    fn key(&self, class: usize, file: &str, name: &str) -> u64 {
        key(&self.class_rel[class], file, name)
    }

    fn psi(&self, class: usize) -> f32 {
        self.read(class, "cpu.pressure")
            .and_then(|text| cgroup::parse_psi(&text))
            .map_or(0.0, |psi| psi.some_avg10)
    }

    fn memory_psi(&self, class: usize) -> f32 {
        self.read(class, "memory.pressure")
            .and_then(|text| cgroup::parse_psi(&text))
            .map_or(0.0, |psi| psi.some_avg10)
    }

    fn systemctl_calls(&self, needle: &str) -> usize {
        fs::read_to_string(self.runtime.path().join("systemctl.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(needle))
            .count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if !self.daemon_unit.is_empty() {
            let _ = mutate(&["--user", "stop", &self.daemon_unit]);
            let _ = mutate(&["--user", "reset-failed", &self.daemon_unit]);
        }
        for unit in &self.jobs {
            let _ = mutate(&[
                "--user",
                "kill",
                "--signal=SIGKILL",
                "--kill-whom=all",
                unit,
            ]);
            let _ = mutate(&["--user", "stop", unit]);
            let _ = mutate(&["--user", "reset-failed", unit]);
        }
        for unit in std::iter::once(&self.pool).chain(self.classes.iter()) {
            let _ = mutate(&["--user", "revert", unit]);
            let _ = mutate(&["--user", "stop", unit]);
            let _ = mutate(&["--user", "reset-failed", unit]);
        }
        for file in &self.files {
            let _ = fs::remove_file(file);
        }
        let _ = systemctl(&["--user", "daemon-reload"]);
    }
}

fn finish_probe(child: Child) -> (u64, u64) {
    let output = child.wait_with_output().expect("wait for probe");
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "probe failed: {}\n{text}",
        String::from_utf8_lossy(&output.stderr)
    );
    let line = text
        .lines()
        .find(|line| line.starts_with("PROBE "))
        .unwrap_or_else(|| panic!("probe printed no PROBE line: {text}"));
    let field = |name: &str| -> u64 {
        line.split_whitespace()
            .find_map(|part| part.strip_prefix(name)?.strip_prefix('=')?.parse().ok())
            .unwrap_or_else(|| panic!("probe line has no {name}: {line}"))
    };
    (field("p50_us"), field("p95_us"))
}

fn result(number: u8, pass: bool, numbers: impl std::fmt::Display) {
    println!(
        "RESULT {number} {} {numbers}",
        if pass { "PASS" } else { "FAIL" }
    );
}

#[test]
#[ignore]
fn load1_u1_lone_batch_gets_c_cpus() {
    if skip("load1") {
        return;
    }
    let Some(mut fixture) = Fixture::new("load1") else {
        return;
    };
    let c = fixture.c;
    fixture.assert_config();
    fixture.install_units();
    fixture.start_child_daemon();
    let unit = fixture.launch(
        "u1",
        "batch",
        64 << 20,
        &args(&format!("cpu {c} --secs 30")),
    );
    assert!(
        wait_value(Duration::from_secs(5), || control_group(&unit)).is_some(),
        "batch hog did not start"
    );
    thread::sleep(Duration::from_secs(2));
    let start = Instant::now();
    let before = fixture.key(2, "cpu.stat", "usage_usec");
    thread::sleep(Duration::from_secs(20));
    let after = fixture.key(2, "cpu.stat", "usage_usec");
    let elapsed = start.elapsed().as_secs_f64();
    let got = (after.saturating_sub(before)) as f64 / elapsed / 1_000_000.0;
    let pass = got >= 0.9 * f64::from(c);
    result(1, pass, format!("cpus={got:.2} C={c}"));
    assert!(pass, "a lone batch `cpu C` got {got:.2} of C={c} CPUs");
}

#[test]
#[ignore]
fn load2_u2_interactive_is_protected() {
    if skip("load2") {
        return;
    }
    let Some(mut fixture) = Fixture::new("load2") else {
        return;
    };
    let c = fixture.c;
    fixture.assert_config();
    fixture.install_units();
    fixture.start_child_daemon();

    let pool_rel = fixture.class_rel[0]
        .rsplit_once('/')
        .map(|(pool, _)| pool.to_string())
        .unwrap_or_default();
    let read = |name: &str| key(&pool_rel, "cpu.stat", name);
    let throttled_before = read("nr_throttled");
    let (idle_p50, idle_p95) = fixture.probe("idle", 8);

    let mut hogs = Vec::new();
    for tag in ["hoga", "hogb"] {
        hogs.push(fixture.launch(tag, "batch", 64 << 20, &args(&format!("cpu {c} --secs 40"))));
    }
    for unit in &hogs {
        assert!(
            wait_value(Duration::from_secs(5), || control_group(unit)).is_some(),
            "batch hog {unit} did not start"
        );
    }
    thread::sleep(Duration::from_secs(1));

    let (probe_child, probe_unit) = fixture.spawn_probe("load", 30);
    assert!(
        wait_value(Duration::from_secs(5), || control_group(&probe_unit)).is_some(),
        "contended probe did not start"
    );
    let start = Instant::now();
    let before = fixture.key(2, "cpu.stat", "usage_usec");
    let mut samples = Vec::new();
    while start.elapsed() < Duration::from_secs(30) {
        samples.push((start.elapsed(), fixture.psi(0)));
        thread::sleep(Duration::from_millis(250));
    }
    let after = fixture.key(2, "cpu.stat", "usage_usec");
    let elapsed = start.elapsed().as_secs_f64();
    let (load_p50, load_p95) = finish_probe(probe_child);
    let throttled = read("nr_throttled").saturating_sub(throttled_before);

    let settled: Vec<f32> = samples
        .iter()
        .filter(|(at, _)| *at >= Duration::from_secs(5))
        .map(|(_, psi)| *psi)
        .collect();
    let psi_max = settled.iter().copied().fold(0.0_f32, f32::max);
    let batch_cpus = (after.saturating_sub(before)) as f64 / elapsed / 1_000_000.0;
    let pass =
        psi_max <= 10.0 && load_p95 <= 4 * idle_p95.max(1) && batch_cpus >= 0.5 * f64::from(c);
    result(
        2,
        pass,
        format!(
            "psi_max={psi_max:.2}% p50_idle_us={idle_p50} p95_idle_us={idle_p95} p50_load_us={load_p50} p95_load_us={load_p95} batch_cpus={batch_cpus:.2} pool_throttles={throttled} C={c}"
        ),
    );
    assert!(
        pass,
        "interactive protection missed: psi {psi_max:.2} %, p95 {load_p95} us (idle {idle_p95}), batch {batch_cpus:.2} CPUs"
    );
}

#[test]
#[ignore]
fn load3_no_oscillation() {
    if skip("load3") {
        return;
    }
    let Some(mut fixture) = Fixture::new("load3") else {
        return;
    };
    let c = fixture.c;
    fixture.assert_config();
    fixture.install_units();
    fixture.start_child_daemon();

    // A batch hog keeps the machine contended, so an interactive burst really
    // waits; `cpu 2C` clears its 400-weight share and trips Rule C. Three 5 s
    // bursts, 100 s apart, over the 5 min window. A longer burst keeps PSI above
    // the 20 % engage threshold into the next 10 s write interval and clamps
    // twice; 5 s clamps one CPU and still leaves 100 s for the release.
    let batch = fixture.launch(
        "steady",
        "batch",
        64 << 20,
        &args(&format!("cpu {c} --secs 330")),
    );
    assert!(
        wait_value(Duration::from_secs(5), || control_group(&batch)).is_some(),
        "steady batch hog did not start"
    );
    let start = Instant::now();
    for _ in 0..3 {
        let unit = fixture.launch(
            "pressure",
            "interactive",
            64 << 20,
            &args(&format!("cpu {} --secs 5", 2 * c)),
        );
        assert!(
            wait_value(Duration::from_secs(5), || control_group(&unit)).is_some(),
            "interactive pressure hog did not start"
        );
        thread::sleep(Duration::from_secs(100));
    }
    thread::sleep(Duration::from_secs(300).saturating_sub(start.elapsed()));

    let actions = fixture.logs.as_ref().expect("child daemon logs").actions();
    let clamps = actions.iter().filter(|(_, event)| event == "clamp").count();
    let unclamps = actions
        .iter()
        .filter(|(_, event)| event == "unclamp")
        .count();
    let mut reversed: Vec<f64> = Vec::new();
    for pair in actions.windows(2) {
        if pair[0].1 != pair[1].1 {
            reversed.push(pair[1].0.saturating_sub(pair[0].0).as_secs_f64());
        }
    }
    let min_gap = reversed.iter().copied().fold(f64::INFINITY, f64::min);
    let cpu_max = fixture.read(2, "cpu.max").unwrap_or_default();
    let released = cgroup::parse_cpu_max(&cpu_max).is_some_and(|max| max.quota_us.is_none());
    let calls = fixture.systemctl_calls("set-property");
    let pass = actions.len() <= 6
        && clamps >= 1
        && clamps == unclamps
        && reversed.iter().all(|gap| *gap >= 30.0)
        && released
        && calls == actions.len();
    result(
        3,
        pass,
        format!(
            "actions={} clamps={clamps} unclamps={unclamps} min_reverse_gap_s={min_gap:.1} released={released} cpu_max={cpu_max:?} set_property_calls={calls}",
            actions.len()
        ),
    );
    assert!(
        pass,
        "clamp/unclamp oscillation: {} actions, gaps {reversed:?}, released {released} ({cpu_max:?}), calls/actions {calls}/{} ({actions:?})",
        actions.len(),
        actions.len()
    );
}

#[test]
#[ignore]
fn load4_u3_u4_queue_admits_after_exit() {
    if skip("load4") {
        return;
    }
    let Some(mut fixture) = Fixture::new("load4") else {
        return;
    };
    let need = fixture.p * 6 / 10;
    fixture.assert_config();
    fixture.install_units();
    fixture.start_child_daemon();

    let first = fixture.launch(
        "mema",
        "batch",
        need,
        &args(&format!("mem {need} --secs 10")),
    );
    assert!(
        wait_value(Duration::from_secs(10), || control_group(&first)).is_some(),
        "first 0.6P batch hog was not admitted"
    );
    thread::sleep(Duration::from_secs(1));

    let (mut second_child, second) = fixture.spawn_run(
        "memb",
        "batch",
        need,
        &args(&format!("mem {need} --secs 5")),
    );
    // The daemon logs `event=queue` for this unit; the cgroup stays absent until admitted.
    let queued = wait_for(Duration::from_secs(3), || {
        fixture.logs.as_ref().is_some_and(|logs| {
            logs.entries()
                .iter()
                .any(|(_, line)| line.contains("event=queue") && line.contains(&second))
        })
    });
    let mut admitted = false;
    let mut gap = -1.0;
    if queued && wait_for(Duration::from_secs(20), || control_group(&first).is_none()) {
        let gone_at = Instant::now();
        admitted = wait_value(Duration::from_secs(20), || control_group(&second)).is_some();
        gap = gone_at.elapsed().as_secs_f64();
    }
    if admitted {
        let status = second_child.wait().expect("wait for the queued wrapper");
        assert!(status.success(), "the queued wrapper exited {status}");
    } else {
        let _ = second_child.kill();
        let _ = second_child.wait();
    }

    let ooms: Vec<u64> = (0..3)
        .map(|class| fixture.key(class, "memory.events", "oom_kill"))
        .collect();
    // + 1 s: `systemd-run` start-up after the daemon's admission, not a tick of slack.
    let pass = queued
        && admitted
        && gap <= 2.0 * TICK as f64 + 1.0
        && ooms.iter().all(|count| *count == 0);
    result(
        4,
        pass,
        format!(
            "queued={queued} admitted={admitted} admit_gap_s={gap:.2} ticks={:.1} oom_kill={ooms:?} P={}GiB",
            2.0 * TICK as f64,
            fixture.p >> 30
        ),
    );
    assert!(
        pass,
        "queued {queued}, admitted {admitted}, gap {gap:.2} s; oom_kill {ooms:?}"
    );
}

#[test]
#[ignore]
fn load5_memory_brake_protects_interactive() {
    if skip("load5") {
        return;
    }
    let Some(mut fixture) = Fixture::new("load5") else {
        return;
    };
    let p = fixture.p;
    let batch_target = p * 45 / 100;
    let interactive_target = p * 30 / 100;
    fixture.assert_config();
    fixture.install_units();
    fixture.start_child_daemon();

    // Batch starts at 0.45P and grows slowly; interactive then grows to 0.3P
    // during that contention. Rule M engages at 2 * margin headroom (P here is
    // 4 GiB, so 0.25P), before the pool's static high (P - margin = 0.875P).
    let batch = fixture.launch(
        "brake",
        "batch",
        batch_target,
        &args(&format!(
            "mem {batch_target} --grow-to {} --step {} --every 1000 --secs 60",
            p * 60 / 100,
            p / 200
        )),
    );
    assert!(
        wait_value(Duration::from_secs(10), || control_group(&batch)).is_some(),
        "batch memory hog did not start"
    );
    assert!(
        wait_for(Duration::from_secs(20), || {
            fixture.number(2, "memory.current") >= batch_target * 9 / 10
        }),
        "batch did not reach 0.45P"
    );

    let interactive = fixture.launch(
        "grow",
        "interactive",
        interactive_target,
        &args(&format!(
            "mem 0 --grow-to {interactive_target} --step {} --every 500 --secs 100",
            p / 50
        )),
    );
    assert!(
        wait_value(Duration::from_secs(10), || control_group(&interactive)).is_some(),
        "interactive memory hog did not start"
    );

    let started = Instant::now();
    let mut brake = None;
    let mut reached = false;
    let mut psi_max = 0.0_f32;
    while started.elapsed() < Duration::from_secs(60) {
        if brake.is_none() {
            brake = fixture.read(2, "memory.high").filter(|high| high != "max");
        }
        psi_max = psi_max.max(fixture.memory_psi(0));
        if brake.is_some() && fixture.number(0, "memory.current") >= interactive_target {
            reached = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let interactive_current = fixture.number(0, "memory.current");
    let interactive_alive = control_group(&interactive).is_some();
    let batch_oom = fixture.key(2, "memory.events", "oom_kill");
    let interactive_oom = fixture.key(0, "memory.events", "oom_kill");
    let normal_oom = fixture.key(1, "memory.events", "oom_kill");

    // Batch exits first; the brake must release (headroom returns to ~0.7P) and
    // the live `MemoryHigh=infinity` write must put the slice back to `max`.
    let batch_gone = wait_for(Duration::from_secs(60), || control_group(&batch).is_none());
    let released = wait_for(Duration::from_secs(40), || {
        fixture
            .logs
            .as_ref()
            .is_some_and(|logs| logs.actions().iter().any(|(_, event)| event == "brake_off"))
    });
    let high_max = wait_for(Duration::from_secs(20), || {
        fixture
            .read(2, "memory.high")
            .is_some_and(|high| high == "max")
    });

    let actions = fixture.logs.as_ref().expect("child daemon logs").actions();
    let calls = fixture.systemctl_calls("set-property");
    let pass = batch_gone
        && reached
        && brake.is_some()
        && psi_max <= 10.0
        && interactive_current >= interactive_target
        && interactive_alive
        && interactive_oom == 0
        && normal_oom == 0
        && released
        && high_max
        && calls == actions.len();
    result(
        5,
        pass,
        format!(
            "batch_gone={} batch_memory_high={} batch_oom={batch_oom} interactive_current={interactive_current} interactive_target={interactive_target} interactive_mem_psi_max={psi_max:.2}% interactive_oom={interactive_oom} normal_oom={normal_oom} interactive_alive={interactive_alive} released={released} memory_high_max={high_max} set_property_calls={calls} actions={} P={}GiB",
            batch_gone,
            brake.as_deref().unwrap_or("unset"),
            actions.len(),
            p >> 30
        ),
    );
    assert!(
        pass,
        "brake {brake:?}; interactive {interactive_current} of {interactive_target} bytes, psi {psi_max:.2}%, oom {interactive_oom}/{normal_oom}, alive {interactive_alive}, batch gone {batch_gone}, released {released}, high max {high_max}, calls/actions {calls}/{}",
        actions.len()
    );
}

#[test]
#[ignore]
fn load6_fail_safe_after_daemon_kill() {
    if skip("load6") {
        return;
    }
    let Some(mut fixture) = Fixture::new("load6") else {
        return;
    };
    fixture.assert_config();
    fixture.install_units();
    fixture.start_service_daemon();

    let hog_unit = fixture.launch("survivor", "batch", 64 << 20, &args("cpu 1 --secs 60"));
    assert!(
        wait_value(Duration::from_secs(5), || control_group(&hog_unit)).is_some(),
        "survivor hog did not start"
    );
    mutate(&[
        "--user",
        "set-property",
        "--runtime",
        &fixture.classes[2],
        "CPUQuota=150%",
        "MemoryHigh=134217728",
    ])
    .expect("set runtime properties");
    assert!(
        wait_for(Duration::from_secs(2), || fixture
            .read(2, "cpu.max")
            .is_some_and(|value| value == "150000 100000")),
        "runtime quota was not applied"
    );

    let killed = Instant::now();
    mutate(&[
        "--user",
        "kill",
        "--signal=SIGKILL",
        "--kill-whom=main",
        &fixture.daemon_unit,
    ])
    .expect("kill -9 the daemon");
    let restored = wait_for(Duration::from_secs(10), || {
        fixture
            .read(2, "cpu.max")
            .is_some_and(|value| value == "max 100000")
            && fixture
                .read(2, "memory.high")
                .is_some_and(|value| value == "max")
    });
    let revert_secs = killed.elapsed().as_secs_f64();
    let alive = control_group(&hog_unit).is_some();
    assert!(alive, "the hog died with the daemon");
    assert!(restored, "class slices were not reverted within 10 s");

    let _ = mutate(&["--user", "stop", &fixture.daemon_unit]);
    let (name, fallback) = fixture.job("fallback");
    let output = fixture
        .command(BIN)
        .args(["run", "--class", "batch", "--mem-hint", "67108864", "--"])
        .arg(format!("--unit={name}"))
        .arg("true")
        .output()
        .expect("fallback run");
    let warned = String::from_utf8_lossy(&output.stderr)
        .contains("daemon unreachable, launching with static defaults");
    let pass = restored && alive && output.status.success() && warned;
    result(
        6,
        pass,
        format!("revert_s={revert_secs:.2} hog_alive={alive} fallback_warning={warned}"),
    );
    assert!(
        pass,
        "fail-safe missed: restored {restored}, alive {alive}, fallback {warned}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = wait_for(Duration::from_secs(5), || {
        control_group(&fallback).is_none()
    });
}

#[test]
#[ignore]
fn load7_set_property_calls_match_actions() {
    if skip("load7") {
        return;
    }
    let Some(mut fixture) = Fixture::new("load7") else {
        return;
    };
    let c = fixture.c;
    fixture.assert_config();
    fixture.install_units();
    fixture.start_child_daemon();
    let unit = fixture.launch(
        "count",
        "interactive",
        64 << 20,
        &args(&format!("cpu {} --secs 8", 2 * c)),
    );
    assert!(
        wait_value(Duration::from_secs(5), || control_group(&unit)).is_some(),
        "pressure hog did not start"
    );
    thread::sleep(Duration::from_secs(60));

    let calls = fixture.systemctl_calls("set-property");
    let actions = fixture.logs.as_ref().expect("child daemon logs").actions();
    let pass = calls == actions.len() && !actions.is_empty();
    result(
        7,
        pass,
        format!("set_property_calls={calls} action_lines={}", actions.len()),
    );
    assert!(
        pass,
        "set-property calls {calls} != action lines {} (evidence: PATH shim)",
        actions.len()
    );
}

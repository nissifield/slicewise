//! M1 live gate (design §5.7, §10, §12): daemon, run, explain, reset, and the
//! `ExecStopPost` fail-safe after `kill -9`.
//!
//! `#[ignore]` and a `SKIP` unless `SLICEWISE_LIVE=1`. Every unit this test
//! creates contains `slicewisetest`; `mutate()` refuses anything else. The
//! daemon gets a private `$XDG_RUNTIME_DIR` whose `bus` and `systemd/private`
//! symlinks point at the real user manager, so the socket is isolated but
//! `systemctl --user` still works. Unit files are restored/removed in `Drop`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use slicewise::cgroup;

const MARK: &str = "slicewisetest";
const BIN: &str = env!("CARGO_BIN_EXE_slicewise");

static NEXT: AtomicU32 = AtomicU32::new(0);

fn skip(test: &str) -> bool {
    if std::env::var("SLICEWISE_LIVE").as_deref() != Ok("1") {
        println!("SKIP {test}: set SLICEWISE_LIVE=1 to run live tests");
        return true;
    }
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        println!("SKIP {test}: XDG_RUNTIME_DIR is unset");
        return true;
    }
    false
}

fn show(unit: &str, property: &str) -> String {
    slicewise::systemd::systemctl(&["--user", "show", "--value", "-p", property, unit])
        .unwrap_or_else(|error| panic!("show {property} for {unit}: {error}"))
}

fn reload() -> Result<String, String> {
    slicewise::systemd::systemctl(&["--user", "daemon-reload"])
}

/// Any mutating systemctl call must name a unit this test owns.
fn mutate(args: &[&str]) -> Result<String, String> {
    assert!(
        args.iter().any(|arg| arg.contains(MARK)),
        "refusing mutating systemctl call without a {MARK} unit"
    );
    slicewise::systemd::systemctl(args)
}

fn wait_for(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if ready() {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
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

fn cpu_max(unit: &str) -> String {
    let rel = show(unit, "ControlGroup");
    if rel.is_empty() {
        return String::new();
    }
    cgroup::read_trimmed(Path::new(cgroup::CGROUP_ROOT), &format!("{rel}/cpu.max"))
        .unwrap_or_default()
}

struct Fixture {
    pool: String,
    classes: [String; 3],
    unit_dir: PathBuf,
    rendered: common::TempDir,
    config_home: common::TempDir,
    runtime: common::TempDir,
    daemon_child: Option<Child>,
    daemon_unit: String,
    jobs: Vec<String>,
    files: Vec<PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        let real = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .expect("skip() checked XDG_RUNTIME_DIR");
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        // No dash in the stem: a dash would nest the pool under an auto-created slice.
        let stem = format!("{MARK}{}x{n}", std::process::id());
        let pool = format!("app-{stem}.slice");
        let runtime = common::TempDir::new("live-runtime");
        fs::create_dir_all(runtime.path().join("systemd")).expect("runtime systemd dir");
        std::os::unix::fs::symlink(real.join("bus"), runtime.path().join("bus")).expect("link bus");
        std::os::unix::fs::symlink(
            real.join("systemd/private"),
            runtime.path().join("systemd/private"),
        )
        .expect("link systemd/private");
        let config_home = common::TempDir::new("live-config");
        let config_dir = config_home.path().join("slicewise");
        fs::create_dir_all(&config_dir).expect("config dir");
        fs::write(
            config_dir.join("config.toml"),
            format!(
                "pool_slice = \"{pool}\"\nsystem_reserve = \"256M\"\nmem_per_job = \"256M\"\nreserve_cpus = 1\n"
            ),
        )
        .expect("write config");

        let classes = [
            format!("app-{stem}-interactive.slice"),
            format!("app-{stem}-normal.slice"),
            format!("app-{stem}-batch.slice"),
        ];
        Self {
            pool,
            classes,
            unit_dir: common::user_unit_dir(),
            rendered: common::TempDir::new("live-units"),
            config_home,
            runtime,
            daemon_child: None,
            daemon_unit: String::new(),
            jobs: Vec::new(),
            files: Vec::new(),
        }
    }

    fn socket(&self) -> PathBuf {
        self.runtime.path().join("slicewise.sock")
    }

    /// design: §5.7 — prove the config is the test pool before any unit is touched.
    fn assert_config(&self) {
        let output = self.slicewise(&["check-config"]);
        assert!(
            output.status.success(),
            "check-config failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains(&self.pool),
            "check-config does not print the test pool {}: {text}",
            self.pool
        );
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
            let source = self.rendered.path().join(name);
            let target = self.unit_dir.join(name);
            fs::copy(&source, &target).expect("copy rendered unit");
            self.files.push(target);
        }
        reload().expect("daemon-reload after writing units");
        let mut start = vec!["--user", "start", self.pool.as_str()];
        start.extend(self.classes.iter().map(String::as_str));
        mutate(&start).expect("start test slices");
    }

    fn start_child_daemon(&mut self) {
        let child = self
            .command(BIN)
            .arg("daemon")
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn daemon");
        self.daemon_child = Some(child);
        let socket = self.socket();
        assert!(
            wait_for(Duration::from_secs(5), || socket.exists()),
            "daemon socket did not appear"
        );
        let mode = fs::metadata(&socket)
            .expect("stat socket")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "socket mode");
    }

    fn kill_child_daemon(&mut self) {
        if let Some(mut child) = self.daemon_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
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
        let socket = self.socket();
        assert!(
            wait_for(Duration::from_secs(5), || UnixStream::connect(&socket)
                .is_ok()),
            "service daemon socket did not become reachable"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.kill_child_daemon();
        if !self.daemon_unit.is_empty() {
            let _ = mutate(&["--user", "stop", &self.daemon_unit]);
            let _ = mutate(&["--user", "reset-failed", &self.daemon_unit]);
        }
        for unit in &self.jobs {
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
        let _ = reload();
    }
}

#[test]
#[ignore]
fn daemon_serves_run_explain_reset_and_stop_post() {
    if skip("daemon_serves_run_explain_reset_and_stop_post") {
        return;
    }
    let mut fixture = Fixture::new();
    fixture.assert_config();
    fixture.install_units();
    fixture.start_child_daemon();

    // A tiny batch job lands in the batch class slice and gets a reason.
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let job = format!("{MARK}-job-{}-{n}", std::process::id());
    let unit = format!("{job}.service");
    fixture.jobs.push(unit.clone());
    let output = fixture.slicewise(&[
        "run",
        "--class",
        "batch",
        "--mem-hint",
        "128M",
        "--",
        &format!("--unit={job}"),
        "sleep",
        "10",
    ]);
    assert!(
        output.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let batch = fixture.classes[2].clone();
    assert!(
        wait_for(Duration::from_secs(5), || {
            show(&unit, "ControlGroup").contains(&format!("/{batch}/"))
        }),
        "job did not land in {}: {}",
        batch,
        show(&unit, "ControlGroup")
    );

    let output = fixture.slicewise(&["explain", &unit]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(output.status.success(), "explain failed: {text}");
    assert!(
        text.contains(&unit),
        "explain does not name the unit: {text}"
    );
    assert!(text.contains("admit"), "explain has no admission: {text}");
    assert!(text.contains("headroom"), "explain has no reason: {text}");

    // A scope inherits the wrapper's raised oom_score_adj and gets oom.group (m0 ruling).
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let scope_job = format!("{MARK}-scope-{}-{n}", std::process::id());
    let scope = format!("{scope_job}.scope");
    let scope_unit = format!("--unit={scope_job}");
    fixture.jobs.push(scope.clone());
    let mut scope_run = fixture
        .command(BIN)
        .args([
            "run",
            "--class",
            "batch",
            "--mem-hint",
            "128M",
            "--",
            "--scope",
            &scope_unit,
            "sleep",
            "5",
        ])
        .spawn()
        .expect("spawn scope run");
    let rel = wait_value(Duration::from_secs(5), || {
        let rel = show(&scope, "ControlGroup");
        (!rel.is_empty()).then_some(rel)
    })
    .expect("scope cgroup did not appear");
    let procs = Path::new(cgroup::CGROUP_ROOT)
        .join(rel.trim_start_matches('/'))
        .join("cgroup.procs");
    let pid: u32 = fs::read_to_string(procs)
        .expect("read scope cgroup.procs")
        .lines()
        .next()
        .expect("scope has a process")
        .trim()
        .parse()
        .expect("pid");
    let adjust: i32 = fs::read_to_string(format!("/proc/{pid}/oom_score_adj"))
        .expect("read oom_score_adj")
        .trim()
        .parse()
        .expect("parse oom_score_adj");
    assert_eq!(
        adjust, 800,
        "scope did not inherit the raised oom_score_adj"
    );
    assert_eq!(
        cgroup::read_trimmed(
            Path::new(cgroup::CGROUP_ROOT),
            &format!("{rel}/memory.oom.group")
        )
        .unwrap_or_default(),
        "1",
        "scope OOMPolicy=kill did not set memory.oom.group"
    );
    assert!(
        scope_run.wait().expect("wait for the scope run").success(),
        "scope run failed"
    );

    // reset reverts a runtime property to the unit-file value.
    mutate(&[
        "--user",
        "set-property",
        "--runtime",
        &fixture.classes[2],
        "CPUQuota=150%",
    ])
    .expect("set a runtime quota");
    assert!(
        wait_for(Duration::from_secs(1), || cpu_max(&fixture.classes[2])
            == "150000 100000"),
        "runtime quota not applied"
    );
    let output = fixture.slicewise(&["reset"]);
    assert!(
        output.status.success(),
        "reset failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        wait_for(Duration::from_secs(2), || cpu_max(&fixture.classes[2])
            == "max 100000"),
        "reset did not restore the unit-file quota: {}",
        cpu_max(&fixture.classes[2])
    );

    // The daemon dies hard; ExecStopPost=reset must restore the runtime property.
    fixture.kill_child_daemon();
    fixture.start_service_daemon();
    mutate(&[
        "--user",
        "set-property",
        "--runtime",
        &fixture.classes[2],
        "CPUQuota=150%",
    ])
    .expect("set a runtime quota before the kill");
    assert!(
        wait_for(Duration::from_secs(1), || cpu_max(&fixture.classes[2])
            == "150000 100000"),
        "runtime quota not applied before the kill"
    );
    mutate(&[
        "--user",
        "kill",
        "--signal=SIGKILL",
        "--kill-whom=main",
        &fixture.daemon_unit,
    ])
    .expect("kill -9 the daemon");
    assert!(
        wait_for(Duration::from_secs(10), || cpu_max(&fixture.classes[2])
            == "max 100000"),
        "ExecStopPost reset did not restore the unit-file quota: {}",
        cpu_max(&fixture.classes[2])
    );
    println!("RESULT live_systemd PASS unit={unit} batch={batch}");
}

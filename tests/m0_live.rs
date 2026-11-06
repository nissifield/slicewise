//! M0 live spike (design §12): prove the systemd/cgroup assumptions unprivileged.
//!
//! Every test is `#[ignore]` and returns early printing `SKIP` unless
//! `SLICEWISE_LIVE=1`. Live tests touch only units whose names contain
//! `slicewisetest`; `mutate()` refuses anything else. Run with
//! `--test-threads=1` and `XDG_RUNTIME_DIR` pointing at the user manager.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use slicewise::{cgroup, systemd};

const MARK: &str = "slicewisetest";
const CGROUP_ROOT: &str = cgroup::CGROUP_ROOT;
const PROC_ROOT: &str = cgroup::PROC_ROOT;

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

/// Keep host-specific cgroup prefixes (`user@$UID.service`) out of output.
fn generic(rel: &str) -> String {
    match rel.find("/app.slice") {
        Some(i) => format!("<manager>{}", &rel[i..]),
        None => rel.to_string(),
    }
}

fn user_unit_dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir).join("systemd/user"),
        None => PathBuf::from(std::env::var_os("HOME").expect("HOME unset"))
            .join(".config/systemd/user"),
    }
}

fn find_program(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn show(unit: &str, property: &str) -> String {
    systemd::systemctl(&["--user", "show", "--value", "-p", property, unit])
        .unwrap_or_else(|e| panic!("show {property} for {unit} failed: {e}"))
}

fn reload() -> Result<String, String> {
    systemd::systemctl(&["--user", "daemon-reload"])
}

/// Any mutating systemctl call must name a unit this test owns.
fn mutate(args: &[&str]) -> Result<String, String> {
    assert!(
        args.iter().any(|arg| arg.contains(MARK)),
        "refusing mutating systemctl call without a {MARK} unit"
    );
    systemd::systemctl(args)
}

fn proc_cgroup(pid: u32) -> String {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).expect("read /proc/<pid>/cgroup");
    cgroup::parse_proc_cgroup(&text)
        .expect("cgroup v2 line")
        .to_string()
}

fn wait_for(timeout: Duration, mut ready: impl FnMut() -> bool) -> Option<Duration> {
    let started = Instant::now();
    loop {
        if ready() {
            return Some(started.elapsed());
        }
        if started.elapsed() >= timeout {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

struct SliceFixture {
    stem: String,
    pool: String,
    class: String,
    dir: PathBuf,
    files: Vec<PathBuf>,
    units: Vec<String>,
    tmp: common::TempDir,
}

impl SliceFixture {
    fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        // No dash in the stem: a dash would nest the pool under an auto-created
        // ancestor slice that outlives this test.
        let stem = format!("{MARK}{}x{n}", std::process::id());
        Self {
            pool: format!("app-{stem}.slice"),
            class: format!("app-{stem}-batch.slice"),
            stem,
            dir: user_unit_dir(),
            files: Vec::new(),
            units: Vec::new(),
            tmp: common::TempDir::new("m0-live"),
        }
    }

    fn tmp(&self) -> &Path {
        self.tmp.path()
    }

    fn service_unit(&self, suffix: &str) -> String {
        format!("{}-{suffix}.service", self.stem)
    }

    fn scope_unit(&self, suffix: &str) -> String {
        format!("{}-{suffix}.scope", self.stem)
    }

    fn start_frame(&mut self) {
        fs::create_dir_all(&self.dir).expect("create user unit dir");
        let pool_text = "[Unit]\nDescription=slicewise M0 test pool\n\n[Slice]\nCPUWeight=100\n";
        let class_text = concat!(
            "[Unit]\nDescription=slicewise M0 test class\n\n[Slice]\n",
            "CPUWeight=25\nCPUQuota=100%\nMemoryHigh=128M\n"
        );
        for (name, text) in [(&self.pool, pool_text), (&self.class, class_text)] {
            let file = self.dir.join(name);
            fs::write(&file, text).expect("write unit file");
            self.files.push(file);
        }
        reload().expect("daemon-reload after writing units");
        mutate(&["--user", "start", &self.pool, &self.class]).expect("start test slices");
    }

    fn control_group(&self, unit: &str) -> String {
        show(unit, "ControlGroup")
    }

    fn main_pid(&self, unit: &str) -> u32 {
        show(unit, "MainPID")
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("no MainPID for {unit}"))
    }

    fn service(&mut self, suffix: &str, properties: &[&str], argv: &[&str]) -> String {
        let unit = self.service_unit(suffix);
        let class = self.class.clone();
        let mut args = vec![
            "--user",
            "--unit",
            unit.as_str(),
            "--slice",
            class.as_str(),
            "-p",
            "TasksMax=64",
        ];
        for property in properties {
            args.push("-p");
            args.push(property);
        }
        args.push("--");
        args.extend_from_slice(argv);
        systemd::systemd_run(&args).unwrap_or_else(|e| panic!("systemd-run {suffix}: {e}"));
        self.units.push(unit.clone());
        unit
    }
}

impl Drop for SliceFixture {
    fn drop(&mut self) {
        for unit in &self.units {
            let _ = mutate(&["--user", "stop", unit]);
            let _ = mutate(&["--user", "reset-failed", unit]);
        }
        let _ = mutate(&["--user", "stop", &self.class]);
        let _ = mutate(&["--user", "stop", &self.pool]);
        let _ = mutate(&["--user", "reset-failed", &self.class]);
        let _ = mutate(&["--user", "reset-failed", &self.pool]);
        for file in &self.files {
            let _ = fs::remove_file(file);
        }
        let _ = reload();
    }
}

#[test]
#[ignore]
fn assumption_1_static_frame_slices_nest() {
    if skip("assumption_1") {
        return;
    }
    let mut fx = SliceFixture::new();
    fx.start_frame();

    let pool = fx.control_group(&fx.pool);
    let class = fx.control_group(&fx.class);
    assert!(
        pool.ends_with(&fx.pool),
        "pool ControlGroup {}",
        generic(&pool)
    );
    assert!(
        class.starts_with(&format!("{pool}/")),
        "class slice is not nested under the pool: {}",
        generic(&class)
    );
    assert!(
        Path::new(CGROUP_ROOT)
            .join(class.trim_start_matches('/'))
            .is_dir(),
        "class cgroup directory missing"
    );

    let unit = fx.service("frame-probe", &[], &["sleep", "30"]);
    let placement = proc_cgroup(fx.main_pid(&unit));
    assert!(
        placement.starts_with(&format!("{class}/")),
        "process not in class slice: {}",
        generic(&placement)
    );

    println!(
        "RESULT 1 PASS pool={} class={} pid_cgroup={}",
        generic(&pool),
        generic(&class),
        generic(&placement)
    );
}

#[test]
#[ignore]
fn assumption_2_transient_service_and_scope_land_in_class() {
    if skip("assumption_2") {
        return;
    }
    let mut fx = SliceFixture::new();
    fx.start_frame();
    let class = fx.control_group(&fx.class);

    let service = fx.service("placement", &[], &["sleep", "30"]);
    let service_placement = proc_cgroup(fx.main_pid(&service));
    assert!(
        service_placement.starts_with(&format!("{class}/")),
        "service not in class slice: {}",
        generic(&service_placement)
    );

    let scope = fx.scope_unit("placement");
    let scope_for_thread = scope.clone();
    let class_for_thread = fx.class.clone();
    let handle = thread::spawn(move || {
        systemd::systemd_run(&[
            "--user",
            "--scope",
            "--unit",
            &scope_for_thread,
            "--slice",
            &class_for_thread,
            "-p",
            "TasksMax=64",
            "--",
            "sleep",
            "5",
        ])
    });

    let started = wait_for(Duration::from_secs(3), || {
        !show(&scope, "ControlGroup").is_empty()
    });
    let scope_rel = show(&scope, "ControlGroup");
    assert!(started.is_some(), "scope did not start");
    let procs = fs::read_to_string(
        Path::new(CGROUP_ROOT)
            .join(scope_rel.trim_start_matches('/'))
            .join("cgroup.procs"),
    )
    .expect("read scope cgroup.procs");
    let pid: u32 = procs
        .lines()
        .next()
        .expect("scope has a process")
        .parse()
        .expect("pid");
    let scope_placement = proc_cgroup(pid);
    assert!(
        scope_placement.starts_with(&format!("{class}/")),
        "scope not in class slice: {}",
        generic(&scope_placement)
    );
    handle
        .join()
        .expect("scope thread panicked")
        .expect("scope run");

    println!(
        "RESULT 2 PASS service={} scope={}",
        generic(&service_placement),
        generic(&scope_placement)
    );
}

#[test]
#[ignore]
fn assumption_3_runtime_properties_reach_cgroupfs_within_one_second() {
    if skip("assumption_3") {
        return;
    }
    let mut fx = SliceFixture::new();
    fx.start_frame();
    let class = fx.control_group(&fx.class);
    let cpu_max = format!("{class}/cpu.max");
    let memory_high = format!("{class}/memory.high");
    let root = Path::new(CGROUP_ROOT);

    let started = Instant::now();
    mutate(&[
        "--user",
        "set-property",
        "--runtime",
        &fx.class,
        "CPUQuota=150%",
        "MemoryHigh=256M",
    ])
    .expect("set-property --runtime");

    let latency = wait_for(Duration::from_secs(1), || {
        let cpu = cgroup::read_trimmed(root, &cpu_max)
            .ok()
            .and_then(|text| cgroup::parse_cpu_max(&text));
        let high = cgroup::read_trimmed(root, &memory_high).ok();
        cpu.is_some_and(|c| c.quota_us == Some(150_000) && c.period_us == 100_000)
            && high.as_deref() == Some("268435456")
    });
    let elapsed = started.elapsed();
    assert!(latency.is_some(), "properties did not appear within 1 s");

    let cpu = cgroup::read_trimmed(root, &cpu_max).expect("cpu.max");
    let high = cgroup::read_trimmed(root, &memory_high).expect("memory.high");
    println!(
        "RESULT 3 PASS latency_ms={} cpu.max={} memory.high={}",
        elapsed.as_millis(),
        cpu,
        high
    );
}

#[test]
#[ignore]
fn assumption_4_revert_restores_unit_file_values() {
    if skip("assumption_4") {
        return;
    }
    let mut fx = SliceFixture::new();
    fx.start_frame();
    let class = fx.control_group(&fx.class);
    let root = Path::new(CGROUP_ROOT);
    let cpu_max = format!("{class}/cpu.max");
    let memory_high = format!("{class}/memory.high");

    mutate(&[
        "--user",
        "set-property",
        "--runtime",
        &fx.class,
        "CPUQuota=150%",
        "MemoryHigh=256M",
    ])
    .expect("set-property --runtime");
    let changed = wait_for(Duration::from_secs(1), || {
        cgroup::read_trimmed(root, &cpu_max).is_ok_and(|v| v == "150000 100000")
            && cgroup::read_trimmed(root, &memory_high).is_ok_and(|v| v == "268435456")
    });
    assert!(changed.is_some(), "runtime properties not applied");

    let at_unit_file_values = || {
        cgroup::read_trimmed(root, &cpu_max).is_ok_and(|v| v == "100000 100000")
            && cgroup::read_trimmed(root, &memory_high).is_ok_and(|v| v == "134217728")
    };

    mutate(&["--user", "revert", &fx.class]).expect("revert class slice");
    if let Some(ms) = wait_for(Duration::from_secs(1), at_unit_file_values) {
        println!(
            "RESULT 4 PASS revert restored unit-file values in {} ms",
            ms.as_millis()
        );
        return;
    }

    // Record the fallback if revert did not do it.
    mutate(&[
        "--user",
        "set-property",
        "--runtime",
        &fx.class,
        "CPUQuota=100%",
        "MemoryHigh=128M",
    ])
    .expect("set-property back to unit-file values");
    let fallback = wait_for(Duration::from_secs(1), at_unit_file_values);
    assert!(
        fallback.is_some(),
        "neither revert nor set-property restored the unit-file values"
    );
    println!("RESULT 4 SURPRISE revert did not restore; set-property --runtime back did");
}

#[test]
#[ignore]
fn assumption_5_slice_memory_psi_proc_reads() {
    if skip("assumption_5") {
        return;
    }
    let mut fx = SliceFixture::new();
    fx.start_frame();
    fx.service("stats", &[], &["sleep", "30"]);
    let class = fx.control_group(&fx.class);
    let root = Path::new(CGROUP_ROOT);

    let stats = cgroup::slice_stats(root, &class).expect("slice stats");
    assert!(stats.memory_current > 0, "memory.current is zero");
    let cpu = stats.cpu_pressure.expect("cpu.pressure");
    let memory = stats.memory_pressure.expect("memory.pressure");

    let stat_text =
        cgroup::read_trimmed(root, &format!("{class}/memory.stat")).expect("memory.stat");
    let stat = cgroup::parse_memory_stat(&stat_text);
    assert!(stat.contains_key("anon"), "memory.stat has no anon");
    assert!(stat.contains_key("file"), "memory.stat has no file");

    let available = cgroup::mem_available_bytes(Path::new(PROC_ROOT)).expect("MemAvailable");
    let cpus =
        cgroup::online_cpus(Path::new("/sys/devices/system/cpu/online")).expect("online CPUs");
    assert!(available > 0 && cpus >= 1);

    println!(
        "RESULT 5 PASS memory.current={} inactive_file={} anon={} file={} cpu_some_avg10={} mem_some_avg10={} mem_full_avg10={:?} MemAvailable={} online_cpus={}",
        stats.memory_current,
        stats.inactive_file,
        stat.get("anon").copied().unwrap_or(0),
        stat.get("file").copied().unwrap_or(0),
        cpu.some_avg10,
        memory.some_avg10,
        memory.full_avg10,
        available,
        cpus
    );
}

#[test]
#[ignore]
fn assumption_6_oom_score_adjust_raise_and_lower() {
    if skip("assumption_6") {
        return;
    }
    let mut fx = SliceFixture::new();
    fx.start_frame();

    let raised = fx.service("oom-raise", &["OOMScoreAdjust=800"], &["sleep", "30"]);
    let pid = fx.main_pid(&raised);
    // MainPID can be read before the executor has applied the setting and exec'd: poll up to 2 s.
    let read_adj = || -> i32 {
        fs::read_to_string(format!("/proc/{pid}/oom_score_adj"))
            .expect("read oom_score_adj")
            .trim()
            .parse()
            .expect("parse oom_score_adj")
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut applied = read_adj();
    while applied != 800 && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
        applied = read_adj();
    }
    assert_eq!(applied, 800, "raising OOMScoreAdjust was not applied");

    let class = fx.class.clone();
    let lower = fx.service_unit("oom-lower");
    let result = systemd::systemd_run(&[
        "--user",
        "--unit",
        lower.as_str(),
        "--slice",
        class.as_str(),
        "-p",
        "TasksMax=64",
        "-p",
        "OOMScoreAdjust=-100",
        "--",
        "sleep",
        "5",
    ]);
    match result {
        Err(e) => println!("RESULT 6 PASS raise=800; lowering rejected: {e}"),
        Ok(_) => {
            fx.units.push(lower.clone());
            thread::sleep(Duration::from_millis(200));
            let lower_pid = fx.main_pid(&lower);
            let actual: i32 = fs::read_to_string(format!("/proc/{lower_pid}/oom_score_adj"))
                .expect("read lower oom_score_adj")
                .trim()
                .parse()
                .expect("parse lower oom_score_adj");
            if actual == -100 {
                println!("RESULT 6 SURPRISE raise=800; lowering to -100 was accepted and applied");
            } else {
                println!(
                    "RESULT 6 SURPRISE raise=800; lowering to -100 was accepted but kernel kept inherited oom_score_adj={actual}"
                );
            }
        }
    }
}

#[test]
#[ignore]
fn assumption_7_oom_group_and_scope_properties() {
    if skip("assumption_7") {
        return;
    }
    let mut fx = SliceFixture::new();
    fx.start_frame();

    let unit = fx.service("oom-group", &["OOMPolicy=kill"], &["sleep", "30"]);
    let unit_cgroup = fx.control_group(&unit);
    let oom_group = cgroup::read_trimmed(
        Path::new(CGROUP_ROOT),
        &format!("{unit_cgroup}/memory.oom.group"),
    )
    .expect("memory.oom.group");
    assert_eq!(
        oom_group, "1",
        "OOMPolicy=kill did not set memory.oom.group"
    );

    println!("RESULT 7 service OOMPolicy=kill memory.oom.group={oom_group}");
    for (n, property) in [
        "OOMPolicy=kill",
        "MemoryHigh=64M",
        "CPUWeight=50",
        "OOMScoreAdjust=800",
    ]
    .into_iter()
    .enumerate()
    {
        let scope = fx.scope_unit(&format!("props{n}"));
        let class = fx.class.clone();
        let result = systemd::systemd_run(&[
            "--user",
            "--scope",
            "--unit",
            scope.as_str(),
            "--slice",
            class.as_str(),
            "-p",
            "TasksMax=64",
            "-p",
            property,
            "--",
            "sleep",
            "1",
        ]);
        match result {
            Ok(_) => println!("RESULT 7 scope {property} ACCEPT"),
            Err(e) => println!("RESULT 7 scope {property} REJECT {e}"),
        }
    }
}

#[test]
#[ignore]
fn assumption_8_exec_stop_post_runs_after_sigkill() {
    if skip("assumption_8") {
        return;
    }
    let mut fx = SliceFixture::new();
    fx.start_frame();
    let marker = fx.tmp().join("stop-post-marker");
    let touch = find_program("touch").expect("touch not on PATH");
    let exec = format!("ExecStopPost={} {}", touch.display(), marker.display());

    let unit = fx.service("stop-post", &[&exec], &["sleep", "300"]);
    let pid = fx.main_pid(&unit);
    assert!(pid > 0, "no main pid");

    let started = Instant::now();
    mutate(&[
        "--user",
        "kill",
        "--signal=SIGKILL",
        "--kill-whom=main",
        &unit,
    ])
    .expect("kill -9 main process");
    let appeared = wait_for(Duration::from_secs(5), || marker.exists());
    assert!(
        appeared.is_some(),
        "ExecStopPost marker did not appear after SIGKILL"
    );
    let gone = wait_for(Duration::from_secs(2), || {
        !Path::new(&format!("/proc/{pid}")).exists()
    });
    assert!(gone.is_some(), "main process survived SIGKILL");

    println!(
        "RESULT 8 PASS marker_after_ms={} main_pid_gone=1",
        started.elapsed().as_millis()
    );
}

#[test]
#[ignore]
fn assumption_9_setenv_visible_inside_unit() {
    if skip("assumption_9") {
        return;
    }
    let mut fx = SliceFixture::new();
    fx.start_frame();
    let unit = fx.service_unit("setenv");
    let class = fx.class.clone();
    let printenv = find_program("printenv").expect("printenv not on PATH");

    let output = systemd::systemd_run(&[
        "--user",
        "--unit",
        unit.as_str(),
        "--slice",
        class.as_str(),
        "-p",
        "TasksMax=64",
        "--setenv=SLICEWISE_JOBS=3",
        "--wait",
        "--pipe",
        printenv.to_str().expect("utf-8 path"),
        "SLICEWISE_JOBS",
    ])
    .unwrap_or_else(|e| panic!("systemd-run --setenv: {e}"));
    fx.units.push(unit);
    assert_eq!(output.trim(), "3", "SLICEWISE_JOBS not visible");
    println!("RESULT 9 PASS SLICEWISE_JOBS={output}");
}

#[test]
#[ignore]
fn assumption_10_musl_static_binary() {
    if skip("assumption_10") {
        return;
    }
    let manifest = env!("CARGO_MANIFEST_DIR");
    let built = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            "x86_64-unknown-linux-musl",
        ])
        .current_dir(manifest)
        .status()
        .expect("run cargo build");
    assert!(built.success(), "musl release build failed");

    let binary = Path::new(manifest).join("target/x86_64-unknown-linux-musl/release/slicewise");
    let file = Command::new("file")
        .arg(&binary)
        .output()
        .expect("run file");
    let description = String::from_utf8_lossy(&file.stdout).to_string();
    assert!(
        description.contains("static"),
        "binary is not statically linked: {description}"
    );

    let output = Command::new(&binary)
        .arg("--version")
        .output()
        .expect("run musl binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("slicewise "),
        "unexpected version output"
    );
    println!("RESULT 10 PASS static=1 version={}", stdout.trim());
}

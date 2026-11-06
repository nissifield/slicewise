//! Fake cgroupfs (design §10): daemon ticks over real file formats, with
//! `systemd.rs` in `Record` or `Fail` mode, asserting the `set-property` argv.

pub mod common;

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use slicewise::config::{Class, Config, Size};
use slicewise::daemon::{self, Daemon, Paths, Sampler};
use slicewise::policy::BRAKE_HEADROOM;
use slicewise::systemd::{Mode, Runner};

const MEM: u64 = 8 << 30;
const MIB: u64 = 1 << 20;
const POOL_REL: &str = "app.slice/app-slicewise.slice";

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

fn write_slice(root: &Path, rel: &str, memory: u64, tasks: u64) {
    let dir = root.join(rel);
    write(&dir.join("memory.current"), &format!("{memory}\n"));
    write(
        &dir.join("memory.stat"),
        "anon 1048576\nfile 8192\ninactive_file 0\n",
    );
    write(&dir.join("pids.current"), &format!("{tasks}\n"));
    write(
        &dir.join("cpu.pressure"),
        "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
    );
    write(
        &dir.join("memory.pressure"),
        "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
    );
}

fn scenario() -> (Config, common::TempDir, Sampler, Mutex<Daemon>) {
    let config = Config {
        pool_memory_max: Size(MEM),
        system_reserve: Size(1 << 30),
        mem_per_job: Size(1 << 30),
        ..Config::default()
    };
    let tmp = common::TempDir::new("fake-cgroup");
    let base = tmp.path();
    let root = base.join("cgroup");
    let proc_root = base.join("proc");
    write_slice(&root, POOL_REL, MEM - 200 * MIB, 4);
    write_slice(
        &root.join(POOL_REL),
        &config.class_slice(Class::Batch),
        100 * MIB,
        2,
    );
    write_slice(
        &root.join(POOL_REL),
        &config.class_slice(Class::Normal),
        50 * MIB,
        1,
    );
    write_slice(
        &root.join(POOL_REL),
        &config.class_slice(Class::Interactive),
        20 * MIB,
        1,
    );
    // Real meminfo format; 100 MiB available is below the 512 MiB margin.
    write(
        &proc_root.join("meminfo"),
        "MemTotal: 8388608 kB\nMemFree: 16384 kB\nMemAvailable: 102400 kB\n",
    );
    write(&base.join("online"), "0-3\n");

    let paths = Paths {
        cgroup: root.clone(),
        proc: proc_root,
        cpu_online: base.join("online"),
    };
    let sampler = Sampler::new(paths).with_pool(POOL_REL);
    (config, tmp, sampler, Mutex::new(Daemon::default()))
}

#[test]
fn one_tick_engages_both_brakes_and_records_the_argv() {
    let (config, _tmp, mut sampler, daemon) = scenario();
    let runner = Runner::new(Mode::Record);
    daemon::tick(
        &daemon,
        &mut sampler,
        &config,
        &runner,
        Duration::from_secs(2),
    );

    let recorded = runner.recorded();
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    assert_eq!(
        recorded[0],
        vec![
            "--user".to_string(),
            "set-property".to_string(),
            "--runtime".to_string(),
            config.class_slice(Class::Batch),
            format!("MemoryHigh={}", 100 * MIB + BRAKE_HEADROOM),
        ]
    );
    assert_eq!(
        recorded[1],
        vec![
            "--user".to_string(),
            "set-property".to_string(),
            "--runtime".to_string(),
            config.class_slice(Class::Normal),
            format!("MemoryHigh={}", 50 * MIB + BRAKE_HEADROOM),
        ]
    );

    // A held brake is not written again, and the state was applied after success.
    daemon::tick(
        &daemon,
        &mut sampler,
        &config,
        &runner,
        Duration::from_secs(4),
    );
    assert_eq!(runner.recorded().len(), 2, "a held brake must not retarget");
}

#[test]
fn a_failed_brake_write_is_emitted_again_next_tick() {
    let (config, _tmp, mut sampler, daemon) = scenario();
    let runner = Runner::new(Mode::Fail);
    daemon::tick(
        &daemon,
        &mut sampler,
        &config,
        &runner,
        Duration::from_secs(2),
    );
    assert_eq!(runner.recorded().len(), 2, "{:?}", runner.recorded());

    daemon::tick(
        &daemon,
        &mut sampler,
        &config,
        &runner,
        Duration::from_secs(4),
    );
    let recorded = runner.recorded();
    assert_eq!(recorded.len(), 4, "a failed write must be retried");
    assert_eq!(recorded[0], recorded[2]);
    assert_eq!(recorded[1], recorded[3]);
}

#[test]
fn missing_unit_cgroup_omits_it_and_other_errors_keep_the_last_value() {
    let (config, tmp, mut sampler, _daemon) = scenario();
    let runner = Runner::new(Mode::Record);
    let unit = "slicewisetest-unit.service";
    let class = tmp
        .path()
        .join("cgroup")
        .join(POOL_REL)
        .join(config.class_slice(Class::Batch));
    write_slice(&class, unit, 10 * MIB, 1);
    let units = [unit.to_string()];

    let snapshot = sampler.snapshot(Duration::from_secs(2), &runner, &config, &units);
    assert_eq!(snapshot.units.get(unit), Some(&(10 * MIB)));

    fs::remove_dir_all(class.join(unit)).expect("remove unit cgroup");
    let snapshot = sampler.snapshot(Duration::from_secs(4), &runner, &config, &units);
    assert_eq!(snapshot.units.get(unit), None, "ENOENT omits the unit");

    fs::create_dir_all(class.join(unit).join("memory.current")).expect("bad memory.current");
    let snapshot = sampler.snapshot(Duration::from_secs(6), &runner, &config, &units);
    assert_eq!(
        snapshot.units.get(unit),
        Some(&(10 * MIB)),
        "a non-ENOENT read error keeps the last value"
    );
}

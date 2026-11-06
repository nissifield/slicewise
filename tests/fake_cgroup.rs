//! Fake cgroupfs (design §10): one daemon tick over real file formats, with
//! `systemd.rs` in `Record` mode, asserting the `set-property` argv.

mod common;

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

#[test]
fn one_tick_engages_both_brakes_and_records_the_argv() {
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
    let mut sampler = Sampler::new(paths).with_pool(POOL_REL);
    let runner = Runner::new(Mode::Record);
    let daemon = Mutex::new(Daemon::default());
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

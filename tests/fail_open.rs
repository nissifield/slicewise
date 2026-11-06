//! Non-live fail-open of `run` and `reset` on a bad config (M1b re-review F1).
//! The child sees an env with only fake `systemctl`/`systemd-run` on `PATH`,
//! so a mistake cannot reach the real systemd.

pub mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_slicewise");

struct Fixture {
    _tmp: common::TempDir,
    bin: PathBuf,
    log: PathBuf,
    cfg_home: PathBuf,
    run_dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let tmp = common::TempDir::new(tag);
        let bin = tmp.path().join("bin");
        let log = tmp.path().join("argv.log");
        fs::create_dir_all(&bin).expect("create fake bin");
        for program in ["systemctl", "systemd-run"] {
            let path = bin.join(program);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf '%s %s\\n' \"$0\" \"$*\" >> {}\nexit 0\n",
                    sh_quote(&log)
                ),
            )
            .expect("write fake program");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let cfg_home = tmp.path().join("cfg");
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&run_dir).expect("create runtime dir");
        Self {
            _tmp: tmp,
            bin,
            log,
            cfg_home,
            run_dir,
        }
    }

    fn write_config(&self, text: &str) {
        let path = self.cfg_home.join("slicewise").join("config.toml");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir config");
        fs::write(path, text).expect("write config");
    }

    fn slicewise(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env_clear()
            .env("PATH", &self.bin)
            .env("XDG_RUNTIME_DIR", &self.run_dir)
            .env("XDG_CONFIG_HOME", &self.cfg_home)
            .output()
            .expect("run slicewise")
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn systemctl(fx: &Fixture) -> PathBuf {
    fx.bin.join("systemctl")
}

fn assert_reverted(log: &str, systemctl: &Path, slice: &str) {
    let expected = format!("{} --user revert {slice}", systemctl.display());
    assert!(
        log.lines().any(|line| line == expected),
        "missing {expected:?} in {log:?}"
    );
}

#[test]
fn run_fails_open_on_invalid_config() {
    let fx = Fixture::new("fail-open-run");
    fx.write_config("not = [valid\n");
    let output = fx.slicewise(&["run", "--", "true"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stderr.contains("config error, launching with static defaults"),
        "stderr: {stderr}"
    );
    let log = fx.log();
    let prefix = format!("{} ", fx.bin.join("systemd-run").display());
    let line = log
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no systemd-run line in {log:?}"));
    assert!(
        line.contains("--slice=app-slicewise-normal.slice"),
        "{line:?}"
    );
}

#[test]
fn reset_falls_back_to_default_slices_on_invalid_config() {
    let fx = Fixture::new("fail-open-reset");
    fx.write_config("not = [valid\n");
    let output = fx.slicewise(&["reset"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fx.log();
    let systemctl = systemctl(&fx);
    for class in ["interactive", "normal", "batch"] {
        assert_reverted(&log, &systemctl, &format!("app-slicewise-{class}.slice"));
    }
}

#[test]
fn reset_uses_the_pool_name_when_validation_fails() {
    let fx = Fixture::new("fail-open-reset-valid-name");
    fx.write_config("pool_slice = \"app-slicewisetest-x.slice\"\npool_memory_max = \"1M\"\n");
    let output = fx.slicewise(&["reset"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fx.log();
    let systemctl = systemctl(&fx);
    for class in ["interactive", "normal", "batch"] {
        assert_reverted(
            &log,
            &systemctl,
            &format!("app-slicewisetest-x-{class}.slice"),
        );
    }
}

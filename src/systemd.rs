//! The only process spawner (design §5.3, §9): argv only, never a shell.
//! Every call has a deadline so a hung manager cannot freeze the caller.

use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::policy::{Knob, Write};

pub const SYSTEMCTL_DEADLINE: Duration = Duration::from_secs(5);
pub const SYSTEMD_RUN_DEADLINE: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(10);

/// design: §11 — `Real` spawns systemctl; `Record` stores the argv for fake-cgroup tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Real,
    Record,
}

#[derive(Debug)]
pub struct Runner {
    mode: Mode,
    recorded: Mutex<Vec<Vec<String>>>,
}

impl Runner {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            recorded: Mutex::new(Vec::new()),
        }
    }

    pub fn real() -> Self {
        Self::new(Mode::Real)
    }

    fn call(&self, args: &[&str]) -> Result<String, String> {
        if self.mode == Mode::Record {
            let argv = args.iter().map(|arg| (*arg).to_string()).collect();
            self.recorded.lock().unwrap().push(argv);
            return Ok(String::new());
        }
        systemctl(args)
    }

    pub fn set_property(&self, unit: &str, property: &str) -> Result<String, String> {
        self.call(&["--user", "set-property", "--runtime", unit, property])
    }

    pub fn revert(&self, unit: &str) -> Result<String, String> {
        self.call(&["--user", "revert", unit])
    }

    pub fn show(&self, unit: &str, property: &str) -> Result<String, String> {
        self.call(&["--user", "show", "--value", "-p", property, unit])
    }

    pub fn recorded(&self) -> Vec<Vec<String>> {
        self.recorded.lock().unwrap().clone()
    }
}

/// The `set-property` value for one policy write; `None` is `infinity` (design §5.4).
pub fn write_property(write: &Write) -> String {
    match (write.knob, write.to) {
        (Knob::CpuQuota, Some(percent)) => format!("CPUQuota={percent}%"),
        (Knob::CpuQuota, None) => "CPUQuota=infinity".to_string(),
        (Knob::MemoryHigh, Some(bytes)) => format!("MemoryHigh={bytes}"),
        (Knob::MemoryHigh, None) => "MemoryHigh=infinity".to_string(),
    }
}

pub fn systemctl(args: &[&str]) -> Result<String, String> {
    run("systemctl", args, SYSTEMCTL_DEADLINE)
}

pub fn systemd_run(args: &[&str]) -> Result<String, String> {
    run("systemd-run", args, SYSTEMD_RUN_DEADLINE)
}

/// Returns trimmed stdout on success; on failure includes the exit status and
/// stderr, never the argv (it can carry `--setenv` secrets, design §8).
// shortcut: output is buffered until exit; systemctl output is small, use reader
// threads if a caller can emit more than a pipe buffer.
pub fn run(program: &str, args: &[&str], deadline: Duration) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program}: spawn failed: {e}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < deadline => thread::sleep(POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program}: no exit within {deadline:?}"));
            }
            Err(e) => return Err(format!("{program}: wait failed: {e}")),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("{program}: output failed: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("{program}: {}: {}", output.status, stderr.trim()))
    }
}

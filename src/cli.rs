//! CLI and static frame (design §6.1, §6.2, §8): hand-rolled argv dispatch is in
//! `main.rs`; this module owns each subcommand, the socket client and `units`.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::cgroup;
use crate::config::{self, Class, Config};
use crate::daemon;
use crate::policy;
use crate::proto::{self, DecisionReply, Explain, Request, Response, Status};
use crate::systemd;

pub const EX_USAGE: u8 = 64;
pub const EX_TEMPFAIL: u8 = 75;
pub const EX_CONFIG: u8 = 78;
const EX_UNAVAILABLE: u8 = 3;
const NAME: &str = env!("CARGO_PKG_NAME");
const CPU_ONLINE: &str = "/sys/devices/system/cpu/online";

pub fn main(args: &[String]) -> u8 {
    let Some(command) = args.first() else {
        return usage("missing command");
    };
    let rest = &args[1..];
    match command.as_str() {
        "--version" | "-V" => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            0
        }
        "run" => run(rest),
        "daemon" => daemon::run(),
        "status" => status(rest),
        "explain" => explain(rest),
        "reset" => reset(),
        "check-config" => check_config(rest),
        "units" => units(rest),
        other => usage(&format!("unknown command {other:?}")),
    }
}

fn usage(message: &str) -> u8 {
    eprintln!("{NAME}: {message}");
    eprintln!("usage: {NAME} run|daemon|status|explain|reset|check-config|units [--version]");
    EX_USAGE
}

fn mem_total() -> Result<u64, String> {
    cgroup::mem_total_bytes(Path::new(cgroup::PROC_ROOT))
        .map_err(|error| format!("config error: cannot read MemTotal: {error}"))
}

fn load_config() -> Result<Config, String> {
    Config::load_or_default(mem_total()?)
}

struct RunArgs {
    class: Class,
    mem_hint: Option<u64>,
    jobs: Option<u32>,
    queue: bool,
    passthrough: Vec<String>,
}

fn parse_run(args: &[String]) -> Result<RunArgs, String> {
    let mut parsed = RunArgs {
        class: Class::Normal,
        mem_hint: None,
        jobs: None,
        queue: true,
        passthrough: Vec::new(),
    };
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        if option == "--" {
            parsed.passthrough = args[index + 1..].to_vec();
            break;
        }
        match option {
            "--class" => {
                index += 1;
                parsed.class = class_arg(args.get(index))?;
            }
            "--mem-hint" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--mem-hint needs a size".to_string())?;
                parsed.mem_hint = Some(config::parse_size(value)?);
            }
            "--jobs" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--jobs needs a number".to_string())?;
                let jobs: u32 = value
                    .parse()
                    .map_err(|_| format!("--jobs {value:?}: expected a number"))?;
                if !(1..=proto::JOBS_HINT_MAX).contains(&jobs) {
                    return Err(format!(
                        "--jobs {jobs}: must be 1..={}",
                        proto::JOBS_HINT_MAX
                    ));
                }
                parsed.jobs = Some(jobs);
            }
            "--no-queue" => parsed.queue = false,
            _ => {
                parsed.passthrough = args[index..].to_vec();
                break;
            }
        }
        index += 1;
    }
    if parsed.passthrough.is_empty() {
        return Err("run needs a systemd-run argument list after --".to_string());
    }
    Ok(parsed)
}

fn class_arg(value: Option<&String>) -> Result<Class, String> {
    let value = value.ok_or_else(|| "--class needs interactive, normal or batch".to_string())?;
    Class::ALL
        .into_iter()
        .find(|class| class.as_str() == value)
        .ok_or_else(|| format!("--class {value:?}: expected interactive, normal or batch"))
}

/// Flags `run` owns, and the caller-supplied unit name (design §6.1).
fn check_passthrough(passthrough: &[String]) -> Result<(bool, Option<String>), String> {
    let (mut scope, mut unit) = (false, None);
    let mut index = 0;
    while index < passthrough.len() {
        let arg = passthrough[index].as_str();
        if arg == "--" {
            break;
        }
        for (flag, why) in [
            ("--slice", "--slice is set by slicewise"),
            ("--system", "--system is not supported"),
            ("-M", "-M is not supported"),
            ("-H", "-H is not supported"),
        ] {
            if arg == flag
                || arg.starts_with(&format!("{flag}="))
                || (flag.len() == 2 && arg.starts_with(flag))
            {
                return Err(why.to_string());
            }
        }
        if arg == "--scope" {
            scope = true;
        }
        if arg == "--unit" {
            index += 1;
            unit = passthrough.get(index).cloned();
        } else if let Some(value) = arg.strip_prefix("--unit=") {
            unit = Some(value.to_string());
        }
        index += 1;
    }
    Ok((scope, unit))
}

fn request_unit(class: Class, scope: bool, given: Option<String>) -> (String, bool) {
    let suffix = if scope { "scope" } else { "service" };
    match given {
        Some(name) if config::valid_unit_name(&name) => (name, false),
        Some(name) => (format!("{name}.{suffix}"), false),
        None => (
            format!(
                "{NAME}-{}-{}-1.{suffix}",
                class.as_str(),
                std::process::id()
            ),
            true,
        ),
    }
}

fn systemd_run_args(
    config: &Config,
    class: Class,
    jobs: u32,
    unit: &str,
    add_unit: bool,
    scope: bool,
    passthrough: &[String],
) -> Vec<String> {
    let mut argv = vec![
        "--user".to_string(),
        format!("--slice={}", config.class_slice(class)),
        "-p".to_string(),
        "OOMPolicy=kill".to_string(),
    ];
    for name in &config.jobs_env {
        argv.push(format!("--setenv={name}={jobs}"));
    }
    if add_unit {
        argv.push(format!("--unit={unit}"));
    }
    // design: §6.1 — scopes reject OOMScoreAdjust; the wrapper raises its own instead.
    if !scope && let Some(adjust) = config.classes.oom_score_adj(class) {
        argv.push("-p".to_string());
        argv.push(format!("OOMScoreAdjust={adjust}"));
    }
    argv.extend(passthrough.iter().cloned());
    argv
}

enum Ask {
    Admitted(u32),
    Rejected(String),
    Unavailable,
}

fn ask(request: &Request) -> Ask {
    let Ok(mut client) = Client::connect() else {
        return Ask::Unavailable;
    };
    if client.send(request).is_err() {
        return Ask::Unavailable;
    }
    let response = match client.receive() {
        Ok(response) if response.ok => response,
        _ => return Ask::Unavailable,
    };
    match response.decision {
        Some(DecisionReply::Admit) => Ask::Admitted(response.jobs.unwrap_or(1)),
        Some(DecisionReply::Queued) => {
            client.wait();
            match client.receive() {
                Ok(admitted) if admitted.decision == Some(DecisionReply::Admit) => {
                    Ask::Admitted(admitted.jobs.unwrap_or(1))
                }
                Ok(rejected) => Ask::Rejected(
                    rejected
                        .reason
                        .unwrap_or_else(|| "not admitted".to_string()),
                ),
                Err(_) => Ask::Unavailable,
            }
        }
        _ => Ask::Rejected(
            response
                .reason
                .unwrap_or_else(|| "not admitted".to_string()),
        ),
    }
}

fn run(args: &[String]) -> u8 {
    let parsed = match parse_run(args) {
        Ok(parsed) => parsed,
        Err(message) => return usage(&message),
    };
    let (scope, given) = match check_passthrough(&parsed.passthrough) {
        Ok(found) => found,
        Err(message) => return usage(&message),
    };
    let mem_total = match mem_total() {
        Ok(bytes) => bytes,
        Err(message) => {
            eprintln!("{message}");
            return EX_CONFIG;
        }
    };
    if parsed.mem_hint.is_some_and(|hint| hint > mem_total) {
        return usage("--mem-hint must be <= MemTotal");
    }
    let config = match Config::load_or_default(mem_total) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            return EX_CONFIG;
        }
    };
    let (unit, add_unit) = request_unit(parsed.class, scope, given);
    let request = Request::Admit {
        v: 1,
        unit: unit.clone(),
        class: parsed.class,
        mem_hint: parsed.mem_hint,
        jobs_hint: parsed.jobs,
        queue: parsed.queue,
    };
    let jobs = match ask(&request) {
        Ask::Admitted(jobs) => jobs,
        Ask::Rejected(reason) => {
            eprintln!("{NAME}: {reason}");
            return EX_TEMPFAIL;
        }
        Ask::Unavailable => {
            // design: §5.7 — fail open for launch; the orchestrator is never blocked.
            eprintln!("{NAME}: daemon unreachable, launching with static defaults");
            local_jobs(&config, &parsed)
        }
    };
    if scope && let Some(adjust) = config.classes.oom_score_adj(parsed.class) {
        raise_own_oom(adjust);
    }
    let argv = systemd_run_args(
        &config,
        parsed.class,
        jobs,
        &unit,
        add_unit,
        scope,
        &parsed.passthrough,
    );
    let error = Command::new("systemd-run").args(&argv).exec();
    eprintln!("{NAME}: cannot exec systemd-run: {error}");
    127
}

fn local_jobs(config: &Config, parsed: &RunArgs) -> u32 {
    let snapshot = policy::Snapshot {
        mem_available: cgroup::mem_available_bytes(Path::new(cgroup::PROC_ROOT)).unwrap_or(0),
        online_cpus: cgroup::online_cpus(Path::new(CPU_ONLINE)).unwrap_or(1),
        ..policy::Snapshot::default()
    };
    let available = policy::headroom(&snapshot, config);
    let need = parsed
        .mem_hint
        .unwrap_or(config.classes.default_mem(parsed.class).0);
    policy::jobs(&snapshot, config, need, available, parsed.jobs)
}

/// design: §5.3, §6.1 — procfs, raise-only, and never fatal.
fn raise_own_oom(target: i32) {
    let path = "/proc/self/oom_score_adj";
    let current = fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok());
    match current {
        Some(current) if current >= target => {}
        Some(_) => {
            if let Err(error) = fs::write(path, target.to_string()) {
                eprintln!("{NAME}: cannot raise oom_score_adj to {target}: {error}");
            }
        }
        None => eprintln!("{NAME}: cannot read {path}"),
    }
}

fn status(args: &[String]) -> u8 {
    let mut json = false;
    for arg in args {
        if arg == "--json" {
            json = true;
        } else {
            return usage(&format!("status: unexpected {arg:?}"));
        }
    }
    let config = match load_config() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            return EX_CONFIG;
        }
    };
    let online = Client::connect().and_then(|mut client| {
        client.send(&Request::Status { v: 1 })?;
        client.receive()
    });
    let status = match online {
        Ok(response) if response.ok => match response.status {
            Some(status) => status,
            None => return 1,
        },
        _ => {
            // design: §8 — unreachable exits 3 and shows the cgroupfs view only.
            eprintln!("{NAME}: daemon unreachable, showing cgroupfs only");
            let mut sampler = daemon::Sampler::new(daemon::Paths::default());
            let snapshot = sampler.snapshot(Duration::ZERO, &systemd::Runner::real(), &config, &[]);
            let status = daemon::status(
                &snapshot,
                &policy::State::default(),
                &config,
                Duration::ZERO,
            );
            print_status(&status, json);
            return EX_UNAVAILABLE;
        }
    };
    print_status(&status, json);
    0
}

fn print_status(status: &Status, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(status).unwrap_or_else(|_| "null".to_string())
        );
        return;
    }
    println!(
        "pool cap {} working_set {} headroom {} margin {}",
        status.cap, status.working_set, status.headroom, status.margin
    );
    for row in &status.classes {
        println!(
            "class {} memory {} memory_high {} cpu_quota {} cpu_psi {:.1} memory_psi {:.1} tasks {} brake_secs {} clamp_secs {}",
            row.class.as_str(),
            row.memory,
            limit(row.memory_high),
            limit(row.cpu_quota),
            row.cpu_psi,
            row.memory_psi,
            row.pids,
            limit(row.brake_secs),
            limit(row.clamp_secs),
        );
    }
    for row in &status.protected {
        println!("protected {} cpu_psi {:.1}", row.unit, row.cpu_psi);
    }
    println!("queue {}", status.queue);
}

fn limit(value: Option<u64>) -> String {
    value.map_or_else(|| "infinity".to_string(), |value| value.to_string())
}

fn explain(args: &[String]) -> u8 {
    let mut json = false;
    let mut target = None;
    for arg in args {
        if arg == "--json" {
            json = true;
        } else if target.is_none() {
            target = Some(arg.clone());
        } else {
            return usage("explain takes exactly one unit or class");
        }
    }
    let Some(target) = target else {
        return usage("explain needs a unit or class");
    };
    let online = Client::connect().and_then(|mut client| {
        client.send(&Request::Explain {
            v: 1,
            target: target.clone(),
        })?;
        client.receive()
    });
    match online {
        Ok(response) if response.ok => match response.explain {
            Some(explain) => {
                print_explain(&explain, json);
                0
            }
            None => 1,
        },
        _ => {
            eprintln!("{NAME}: daemon unreachable");
            EX_UNAVAILABLE
        }
    }
}

fn print_explain(explain: &Explain, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(explain).unwrap_or_else(|_| "null".to_string())
        );
        return;
    }
    println!(
        "target {} class {} slice {}",
        explain.target,
        explain.class.map_or("-", Class::as_str),
        explain.slice.as_deref().unwrap_or("-")
    );
    for event in &explain.events {
        println!(
            "-{}s {} {} unit={}",
            event.age_secs,
            event.event,
            event.reason,
            event.unit.as_deref().unwrap_or("-")
        );
    }
    if let Some(current) = &explain.current {
        println!(
            "now memory {} CPUQuota {} cpu_psi {:.1} memory_psi {:.1}",
            current.memory,
            limit(current.cpu_quota),
            current.cpu_psi,
            current.memory_psi
        );
    }
}

/// design: §5.7 — revert the three class slices only, success by exit status alone.
fn reset() -> u8 {
    let config = match load_config() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            return EX_CONFIG;
        }
    };
    let runner = systemd::Runner::real();
    let mut code = 0;
    for class in Class::ALL {
        if let Err(error) = runner.revert(&config.class_slice(class)) {
            eprintln!("{error}");
            code = 1;
        }
    }
    code
}

fn check_config(args: &[String]) -> u8 {
    if args.len() > 1 {
        return usage("check-config takes at most one path");
    }
    let mem_total = match mem_total() {
        Ok(bytes) => bytes,
        Err(message) => {
            eprintln!("{message}");
            return EX_CONFIG;
        }
    };
    let config = match args.first() {
        Some(path) => Config::load(Path::new(path), mem_total),
        None => Config::load_or_default(mem_total),
    };
    let config = match config {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            return EX_CONFIG;
        }
    };
    println!("tick_secs = {}", config.tick_secs);
    println!("pool_slice = {}", config.pool_slice);
    println!("pool_memory_max = {}", config.pool_memory_max.0);
    println!("system_reserve = {}", config.system_reserve.0);
    println!("reserve_cpus = {}", config.reserve_cpus);
    println!("mem_per_job = {}", config.mem_per_job.0);
    for class in Class::ALL {
        println!(
            "class.{}.cpu_weight = {}",
            class.as_str(),
            config.classes.cpu_weight(class)
        );
        println!(
            "class.{}.default_mem = {}",
            class.as_str(),
            config.classes.default_mem(class).0
        );
        if let Some(adjust) = config.classes.oom_score_adj(class) {
            println!("class.{}.oom_score_adj = {adjust}", class.as_str());
        }
    }
    for protected in &config.protected {
        println!(
            "protected.unit = {} cpu_weight = {} memory_low = {}",
            protected.unit, protected.cpu_weight, protected.memory_low.0
        );
    }
    0
}

fn units(args: &[String]) -> u8 {
    if args.len() != 1 {
        return usage("units takes exactly one directory");
    }
    let config = match load_config() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            return EX_CONFIG;
        }
    };
    let binary = match std::env::current_exe() {
        Ok(binary) => binary,
        Err(error) => {
            eprintln!("{NAME}: cannot find own binary: {error}");
            return 1;
        }
    };
    let online = cgroup::online_cpus(Path::new(CPU_ONLINE)).unwrap_or(config.reserve_cpus.max(1));
    for (name, text) in unit_files(&config, online, &binary) {
        let path = Path::new(&args[0]).join(name);
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            eprintln!("{NAME}: cannot create {}: {error}", parent.display());
            return 1;
        }
        if let Err(error) = fs::write(&path, text) {
            eprintln!("{NAME}: cannot write {}: {error}", path.display());
            return 1;
        }
        println!("{}", path.display());
    }
    0
}

/// The static frame (design §5.2, §5.3): text only; the caller writes it.
fn unit_files(config: &Config, online_cpus: u32, binary: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut pool = format!("[Unit]\nDescription={NAME} pool\n\n[Slice]\nCPUWeight=100\n");
    let quotable = online_cpus.saturating_sub(config.reserve_cpus);
    if config.reserve_cpus > 0 && quotable > 0 {
        pool.push_str(&format!("CPUQuota={}%\n", quotable * 100));
    }
    pool.push_str(&format!("MemoryMax={}\n", config.pool_memory_max.0));
    let high = config
        .pool_memory_max
        .0
        .saturating_sub(policy::margin(config));
    if high > 0 {
        pool.push_str(&format!("MemoryHigh={high}\n"));
    }
    files.push((config.pool_slice.clone(), pool));
    for class in Class::ALL {
        files.push((
            config.class_slice(class),
            format!(
                "[Unit]\nDescription={NAME} {} class\n\n[Slice]\nCPUWeight={}\n",
                class.as_str(),
                config.classes.cpu_weight(class)
            ),
        ));
    }
    files.push((
        format!("{NAME}.service"),
        format!(
            "[Unit]\nDescription={NAME} daemon\n\n[Service]\nExecStart={0} daemon\nExecStopPost={0} reset\nRestart=on-failure\nRestartSec=5\nMemoryMax=64M\n\n[Install]\nWantedBy=default.target\n",
            binary.display()
        ),
    ));
    for protected in &config.protected {
        files.push((
            format!("{}.d/10-{NAME}.conf", protected.unit),
            format!(
                "[Service]\nCPUWeight={}\nMemoryLow={}\n",
                protected.cpu_weight, protected.memory_low.0
            ),
        ));
    }
    files
}

struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    fn connect() -> Result<Self, String> {
        let path = proto::socket_path().ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_string())?;
        let stream =
            UnixStream::connect(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|error| format!("{}: {error}", path.display()))?,
        );
        Ok(Self { stream, reader })
    }

    fn send(&mut self, request: &Request) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(request).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        self.stream
            .write_all(&bytes)
            .map_err(|error| error.to_string())
    }

    fn receive(&mut self) -> Result<Response, String> {
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("daemon closed the connection".to_string());
        }
        serde_json::from_str(&line).map_err(|error| error.to_string())
    }

    fn wait(&mut self) {
        let _ = self.reader.get_ref().set_read_timeout(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Protected, Size};

    fn argv_for(scope: bool, add_unit: bool, passthrough: &[&str]) -> Vec<String> {
        let config = Config::default();
        let passthrough: Vec<String> = passthrough.iter().map(|arg| arg.to_string()).collect();
        systemd_run_args(
            &config,
            Class::Batch,
            3,
            "slicewise-batch-1-1.service",
            add_unit,
            scope,
            &passthrough,
        )
    }

    #[test]
    fn run_argv_for_a_service_adds_jobs_env_unit_and_oom() {
        let argv = argv_for(false, true, &["--wait", "true"]);
        assert_eq!(
            &argv[..4],
            [
                "--user",
                "--slice=app-slicewise-batch.slice",
                "-p",
                "OOMPolicy=kill"
            ]
        );
        assert!(argv.contains(&"--setenv=SLICEWISE_JOBS=3".to_string()));
        assert!(argv.contains(&"--setenv=CARGO_BUILD_JOBS=3".to_string()));
        assert!(argv.contains(&"--unit=slicewise-batch-1-1.service".to_string()));
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["-p", "OOMScoreAdjust=800"])
        );
        assert_eq!(&argv[argv.len() - 2..], ["--wait", "true"]);
    }

    #[test]
    fn run_argv_for_a_scope_has_no_oom_and_keeps_the_unit_out_when_given() {
        let argv = argv_for(true, false, &["--scope", "sleep", "1"]);
        assert!(!argv.iter().any(|arg| arg.starts_with("OOMScoreAdjust")));
        assert!(!argv.iter().any(|arg| arg.starts_with("--unit=")));
        assert_eq!(&argv[argv.len() - 3..], ["--scope", "sleep", "1"]);
    }

    #[test]
    fn refused_flags_and_scope_and_unit_are_found_before_the_command() {
        for bad in ["--slice=x.slice", "--system", "-M", "-Hhost"] {
            let args = vec![bad.to_string(), "true".to_string()];
            assert!(check_passthrough(&args).is_err(), "{bad}");
            // After the systemd-run `--`, the same token is the command, not an option.
            let args = vec!["--".to_string(), bad.to_string()];
            assert!(check_passthrough(&args).is_ok(), "{bad}");
        }
        let args: Vec<String> = ["--scope", "--unit=job.scope", "sleep", "1"]
            .iter()
            .map(|arg| arg.to_string())
            .collect();
        assert_eq!(
            check_passthrough(&args).unwrap(),
            (true, Some("job.scope".to_string()))
        );
    }

    #[test]
    fn request_unit_generates_and_normalizes_names() {
        let (generated, add) = request_unit(Class::Batch, false, None);
        assert!(add);
        assert!(generated.starts_with("slicewise-batch-"));
        assert!(generated.ends_with(".service"));
        let (name, add) = request_unit(Class::Normal, true, Some("job".to_string()));
        assert_eq!(name, "job.scope");
        assert!(!add);
        let (name, _) = request_unit(Class::Normal, false, Some("job".to_string()));
        assert_eq!(name, "job.service");
    }

    #[test]
    fn unit_files_render_generic_names_and_golden_text() {
        let mut config = Config {
            pool_memory_max: Size(8 << 30),
            system_reserve: Size(1 << 30),
            ..Config::default()
        };
        config.protected.push(Protected {
            unit: "orchestrator.service".to_string(),
            cpu_weight: 200,
            memory_low: Size(2 << 30),
        });
        let files = unit_files(&config, 5, Path::new("/usr/bin/slicewise"));
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "app-slicewise.slice",
                "app-slicewise-interactive.slice",
                "app-slicewise-normal.slice",
                "app-slicewise-batch.slice",
                "slicewise.service",
                "orchestrator.service.d/10-slicewise.conf",
            ]
        );
        assert_eq!(
            files[0].1,
            "[Unit]\nDescription=slicewise pool\n\n[Slice]\nCPUWeight=100\nCPUQuota=400%\nMemoryMax=8589934592\nMemoryHigh=8053063680\n"
        );
        assert_eq!(
            files[3].1,
            "[Unit]\nDescription=slicewise batch class\n\n[Slice]\nCPUWeight=25\n"
        );
        let service = &files[4].1;
        assert!(service.contains("ExecStart=/usr/bin/slicewise daemon"));
        assert!(service.contains("ExecStopPost=/usr/bin/slicewise reset"));
        assert!(service.contains("Restart=on-failure"));
        assert!(service.contains("RestartSec=5"));
        assert_eq!(
            files[5].1,
            "[Service]\nCPUWeight=200\nMemoryLow=2147483648\n"
        );
    }
}

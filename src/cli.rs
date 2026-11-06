//! CLI and static frame (design §6.1, §6.2, §8): hand-rolled argv dispatch is in
//! `main.rs`; this module owns each subcommand, the socket client and `units`.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::cgroup;
use crate::config::{self, Class, Config};
use crate::daemon;
use crate::policy;
use crate::proto::{self, DecisionReply, Request, Response};
use crate::systemd;

pub const EX_USAGE: u8 = 64;
pub const EX_TEMPFAIL: u8 = 75;
pub const EX_CONFIG: u8 = 78;
const EX_UNAVAILABLE: u8 = 3;
const NAME: &str = env!("CARGO_PKG_NAME");
const CPU_ONLINE: &str = "/sys/devices/system/cpu/online";

pub fn main(args: &[String]) -> u8 {
    let result = match args.split_first() {
        None => Err((EX_USAGE, "missing command".to_string())),
        Some((command, rest)) => match command.as_str() {
            "--version" | "-V" => {
                println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                Ok(0)
            }
            "run" => run(rest),
            "daemon" => Ok(daemon::run()),
            "status" => status(rest),
            "explain" => explain(rest),
            "reset" => Ok(reset()),
            "check-config" => check_config(rest),
            "units" => units(rest),
            other => Err((EX_USAGE, format!("unknown command {other:?}"))),
        },
    };
    match result {
        Ok(code) => code,
        Err((code, message)) => {
            eprintln!("{NAME}: {message}");
            if code == EX_USAGE {
                eprintln!(
                    "usage: {NAME} run|daemon|status|explain|reset|check-config|units [--version]"
                );
            }
            code
        }
    }
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
                parsed.class = class_arg(next_value(args, &mut index, "--class needs a value")?)?;
            }
            "--mem-hint" => {
                let value = next_value(args, &mut index, "--mem-hint needs a size")?;
                parsed.mem_hint = Some(config::parse_size(value)?);
            }
            "--jobs" => {
                let value = next_value(args, &mut index, "--jobs needs a number")?;
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

fn next_value<'a>(args: &'a [String], index: &mut usize, missing: &str) -> Result<&'a str, String> {
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .ok_or_else(|| missing.to_string())
}

fn class_arg(value: &str) -> Result<Class, String> {
    Class::ALL
        .into_iter()
        .find(|class| class.as_str() == value)
        .ok_or_else(|| format!("--class {value:?}: expected interactive, normal or batch"))
}

/// systemd-run options whose value is a separate argument (design §6.1).
const VALUED: &str = "\
    -p --property -E --setenv -u --unit --description --working-directory \
    --uid --gid --nice --service-type --slice -M --machine -H --host \
    --timer-property --path-property --socket-property --on-active --on-boot \
    --on-startup --on-unit-active --on-unit-inactive --on-calendar";
const REFUSED: &str = "--slice --system -M -H --machine --host";

/// Flags `run` owns, and the caller-supplied unit name (design §6.1).
fn check_passthrough(passthrough: &[String]) -> Result<(bool, Option<String>), String> {
    let (mut scope, mut unit) = (false, None);
    let mut index = 0;
    while index < passthrough.len() {
        let arg = passthrough[index].as_str();
        // systemd-run stops option parsing at the first non-option (the command).
        if arg == "--" || !arg.starts_with('-') {
            break;
        }
        if REFUSED.split(' ').any(|flag| {
            arg == flag
                || arg.starts_with(&format!("{flag}="))
                || (flag.len() == 2 && arg.starts_with(flag))
        }) {
            return Err(format!("{arg} is set by slicewise or not supported"));
        }
        if arg == "--scope" {
            scope = true;
        }
        if arg == "--unit" || arg == "-u" {
            index += 1;
            unit = passthrough.get(index).cloned();
        } else if let Some(value) = arg.strip_prefix("--unit=") {
            unit = Some(value.to_string());
        } else if arg.len() > 2 && arg.starts_with("-u") {
            unit = Some(arg[2..].to_string());
        } else if VALUED.split(' ').any(|flag| flag == arg) {
            index += 1;
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
        None => {
            let id = std::process::id();
            (format!("{NAME}-{}-{id}-1.{suffix}", class.as_str()), true)
        }
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
    argv.extend(
        config
            .jobs_env
            .iter()
            .map(|name| format!("--setenv={name}={jobs}")),
    );
    if add_unit {
        argv.push(format!("--unit={unit}"));
    }
    // design: §6.1 — scopes reject OOMScoreAdjust; the wrapper raises its own instead.
    if !scope && let Some(adjust) = config.classes.oom_score_adj(class) {
        argv.extend(["-p".to_string(), format!("OOMScoreAdjust={adjust}")]);
    }
    argv.extend(passthrough.iter().cloned());
    argv
}

enum Ask {
    Admitted(u32),
    Rejected(String),
    Unavailable,
}

const NOT_ADMITTED: &str = "not admitted";

fn ask(request: &Request) -> Ask {
    let Ok(mut reader) = connect() else {
        return Ask::Unavailable;
    };
    if send(&mut reader, request).is_err() {
        return Ask::Unavailable;
    }
    let Ok(response) = receive(&mut reader) else {
        return Ask::Unavailable;
    };
    if !response.ok {
        return Ask::Unavailable;
    }
    match response.decision {
        Some(DecisionReply::Admit) => Ask::Admitted(response.jobs.unwrap_or(1)),
        Some(DecisionReply::Queued) => {
            reader.get_mut().set_read_timeout(None).ok();
            match receive(&mut reader) {
                Ok(admitted) if admitted.decision == Some(DecisionReply::Admit) => {
                    Ask::Admitted(admitted.jobs.unwrap_or(1))
                }
                Ok(rejected) => {
                    Ask::Rejected(rejected.reason.unwrap_or_else(|| NOT_ADMITTED.to_string()))
                }
                Err(_) => Ask::Unavailable,
            }
        }
        _ => Ask::Rejected(response.reason.unwrap_or_else(|| NOT_ADMITTED.to_string())),
    }
}

fn run(args: &[String]) -> Result<u8, (u8, String)> {
    let parsed = parse_run(args).map_err(|message| (EX_USAGE, message))?;
    let (scope, given) =
        check_passthrough(&parsed.passthrough).map_err(|message| (EX_USAGE, message))?;
    let mem_total = mem_total().ok();
    if let Some(total) = mem_total
        && parsed.mem_hint.is_some_and(|hint| hint > total)
    {
        return Err((EX_USAGE, "--mem-hint must be <= MemTotal".to_string()));
    }
    let config = mem_total
        .and_then(|total| Config::load_or_default(total).ok())
        .unwrap_or_else(|| {
            // design: §5.7 — a bad config never blocks a launch.
            eprintln!("{NAME}: config error, launching with static defaults");
            mem_total.map_or_else(Config::default, |total| {
                Config::default().resolve(total).unwrap_or_default()
            })
        });
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
        Ask::Rejected(reason) => return Err((EX_TEMPFAIL, reason)),
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
    let error = systemd::exec_systemd_run(&argv);
    Err((127, format!("cannot exec systemd-run: {error}")))
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

fn query(request: &Request) -> Result<Response, (u8, String)> {
    let unavailable = || (EX_UNAVAILABLE, "daemon unreachable".to_string());
    let mut reader = connect().map_err(|_| unavailable())?;
    send(&mut reader, request).map_err(|_| unavailable())?;
    let response = receive(&mut reader).map_err(|_| unavailable())?;
    if response.ok {
        Ok(response)
    } else {
        Err((
            EX_USAGE,
            response
                .detail
                .unwrap_or_else(|| "daemon rejected the request".to_string()),
        ))
    }
}

fn status(args: &[String]) -> Result<u8, (u8, String)> {
    let mut json = false;
    for arg in args {
        if arg == "--json" {
            json = true;
        } else {
            return Err((EX_USAGE, format!("status: unexpected {arg:?}")));
        }
    }
    let config = load_config().map_err(|message| (EX_CONFIG, message))?;
    let response = match query(&Request::Status { v: 1 }) {
        Ok(response) => response,
        Err((EX_UNAVAILABLE, _)) => {
            // design: §8 — unreachable exits 3 and shows the cgroupfs view only.
            eprintln!("{NAME}: daemon unreachable, showing cgroupfs only");
            let mut sampler = daemon::Sampler::new(daemon::Paths::default());
            let snapshot = sampler.snapshot(Duration::ZERO, &systemd::Runner::real(), &config, &[]);
            let state = policy::State::default();
            let status = daemon::status(&snapshot, &state, &config, Duration::ZERO);
            print_json(&status, json);
            return Ok(EX_UNAVAILABLE);
        }
        Err(error) => return Err(error),
    };
    let Some(status) = response.status else {
        return Ok(1);
    };
    print_json(&status, json);
    Ok(0)
}

fn print_value(prefix: &str, value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .for_each(|(key, value)| print_value(&format!("{prefix}{key} "), value)),
        serde_json::Value::Array(items) => {
            items.iter().for_each(|item| print_value(prefix, item));
        }
        value => println!("{prefix}{value}"),
    }
}

fn print_json<T: serde::Serialize>(value: &T, raw: bool) {
    match serde_json::to_value(value) {
        Ok(value) if raw => println!("{value}"),
        Ok(value) => print_value("", &value),
        Err(_) => println!("null"),
    }
}

fn explain(args: &[String]) -> Result<u8, (u8, String)> {
    let mut json = false;
    let mut target = None;
    for arg in args {
        if arg == "--json" {
            json = true;
        } else if target.is_none() {
            target = Some(arg.clone());
        } else {
            return Err((
                EX_USAGE,
                "explain takes exactly one unit or class".to_string(),
            ));
        }
    }
    let Some(target) = target else {
        return Err((EX_USAGE, "explain needs a unit or class".to_string()));
    };
    let response = query(&Request::Explain { v: 1, target })?;
    let Some(explain) = response.explain else {
        return Ok(1);
    };
    print_json(&explain, json);
    Ok(0)
}

/// design: §5.7 — revert the three class slices only, success by exit status alone.
fn reset() -> u8 {
    let config = reset_config();
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

/// design: §5.7 — a bad config must still revert; only the pool name matters.
fn reset_config() -> Config {
    let Some(path) = config::discover_path().filter(|path| path.exists()) else {
        return Config::default();
    };
    match fs::read_to_string(&path)
        .ok()
        .and_then(|text| Config::parse(&text).ok())
    {
        Some(config) if config::valid_unit_name(&config.pool_slice) => config,
        _ => {
            eprintln!("{NAME}: config error, using the default pool name");
            Config::default()
        }
    }
}

fn check_config(args: &[String]) -> Result<u8, (u8, String)> {
    if args.len() > 1 {
        return Err((EX_USAGE, "check-config takes at most one path".to_string()));
    }
    let mem_total = mem_total().map_err(|message| (EX_CONFIG, message))?;
    let config = match args.first() {
        Some(path) => Config::load(Path::new(path), mem_total),
        None => Config::load_or_default(mem_total),
    }
    .map_err(|message| (EX_CONFIG, message))?;
    println!("{config:#?}");
    Ok(0)
}

fn units(args: &[String]) -> Result<u8, (u8, String)> {
    if args.len() != 1 {
        return Err((EX_USAGE, "units takes exactly one directory".to_string()));
    }
    let config = load_config().map_err(|message| (EX_CONFIG, message))?;
    let binary =
        std::env::current_exe().map_err(|error| (1, format!("cannot find own binary: {error}")))?;
    let online = cgroup::online_cpus(Path::new(CPU_ONLINE)).unwrap_or(config.reserve_cpus.max(1));
    for (name, text) in unit_files(&config, online, &binary) {
        let path = Path::new(&args[0]).join(name);
        let written = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&path, text));
        if let Err(error) = written {
            return Err((1, format!("cannot write {}: {error}", path.display())));
        }
        println!("{}", path.display());
    }
    Ok(0)
}

/// The static frame (design §5.2, §5.3): text only; the caller writes it.
fn unit_files(config: &Config, online_cpus: u32, binary: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let quotable = online_cpus.saturating_sub(config.reserve_cpus);
    let quota = if config.reserve_cpus > 0 && quotable > 0 {
        format!("CPUQuota={}%\n", quotable * 100)
    } else {
        String::new()
    };
    let high = config
        .pool_memory_max
        .0
        .saturating_sub(policy::margin(config));
    let pool = format!(
        "[Unit]\nDescription={NAME} pool\n\n[Slice]\nCPUWeight=100\n{quota}MemoryMax={}\nMemoryHigh={high}\n",
        config.pool_memory_max.0
    );
    files.push((config.pool_slice.clone(), pool));
    for class in Class::ALL {
        let text = format!(
            "[Unit]\nDescription={NAME} {} class\n\n[Slice]\nCPUWeight={}\n",
            class.as_str(),
            config.classes.cpu_weight(class)
        );
        files.push((config.class_slice(class), text));
    }
    files.push((
        format!("{NAME}.service"),
        format!(
            "[Unit]\nDescription={NAME} daemon\n\n[Service]\nExecStart={0} daemon\nExecStopPost={0} reset\nRestart=on-failure\nRestartSec=5\nRestartPreventExitStatus=78\nMemoryMax=64M\n\n[Install]\nWantedBy=default.target\n",
            binary.display()
        ),
    ));
    for protected in &config.protected {
        let text = format!(
            "[Service]\nCPUWeight={}\nMemoryLow={}\n",
            protected.cpu_weight, protected.memory_low.0
        );
        files.push((format!("{}.d/10-{NAME}.conf", protected.unit), text));
    }
    files
}

fn connect() -> Result<BufReader<UnixStream>, String> {
    let path = proto::socket_path().ok_or_else(|| "XDG_RUNTIME_DIR is unset".to_string())?;
    let stream = UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    Ok(BufReader::new(stream))
}

fn send(reader: &mut BufReader<UnixStream>, request: &Request) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    reader
        .get_mut()
        .write_all(&bytes)
        .map_err(|e| e.to_string())
}

fn receive(reader: &mut BufReader<UnixStream>) -> Result<Response, String> {
    let mut line = String::new();
    if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
        return Err("daemon closed the connection".to_string());
    }
    serde_json::from_str(&line).map_err(|e| e.to_string())
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
        // After the command, flags are the command's own arguments.
        let args: Vec<String> = ["sleep", "--slice=x", "-H"]
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(check_passthrough(&args).unwrap(), (false, None));
    }

    #[test]
    fn passthrough_option_values_do_not_hide_the_next_flag() {
        let args: Vec<String> = ["-p", "CPUWeight=50", "--scope", "sleep", "1"]
            .iter()
            .map(|arg| arg.to_string())
            .collect();
        assert!(check_passthrough(&args).unwrap().0);
    }

    #[test]
    fn passthrough_refuses_a_slice_after_an_option_value() {
        let args: Vec<String> = ["-p", "A=B", "--slice=x.slice", "cmd"]
            .iter()
            .map(|arg| arg.to_string())
            .collect();
        assert!(check_passthrough(&args).is_err());
    }

    #[test]
    fn passthrough_finds_the_short_unit_option() {
        let args: Vec<String> = ["-u", "job", "cmd"]
            .iter()
            .map(|arg| arg.to_string())
            .collect();
        assert_eq!(
            check_passthrough(&args).unwrap(),
            (false, Some("job".to_string()))
        );
    }

    #[test]
    fn passthrough_refuses_host_equals_form() {
        let args: Vec<String> = ["--host=h", "cmd"]
            .iter()
            .map(|arg| arg.to_string())
            .collect();
        assert!(check_passthrough(&args).is_err());
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
        assert!(service.contains("RestartPreventExitStatus=78"));
        assert_eq!(
            files[5].1,
            "[Service]\nCPUWeight=200\nMemoryLow=2147483648\n"
        );
    }
}

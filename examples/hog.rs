//! Load generator for the M1c load scenario (design §10); tests only, std only.
//!
//! `cpu N`, `mem SIZE [--grow-to SIZE --step SIZE --every MS]` and `probe`.
//! Every mode takes `--secs N`.

use std::hint::black_box;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("cpu") => cpu(&args[1..]),
        Some("mem") => mem(&args[1..]),
        Some("probe") => probe(&args[1..]),
        _ => Err("usage: hog cpu N|mem SIZE|probe --secs N".to_string()),
    };
    if let Err(error) = result {
        eprintln!("hog: {error}");
        std::process::exit(2);
    }
}

fn value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let index = args.iter().position(|arg| arg == name)?;
    args.get(index + 1).map(String::as_str)
}

fn secs(args: &[String]) -> Result<Duration, String> {
    let value = value(args, "--secs").ok_or("missing --secs N")?;
    let seconds: u64 = value
        .parse()
        .map_err(|_| format!("--secs {value:?}: expected a number"))?;
    Ok(Duration::from_secs(seconds))
}

/// Plain bytes or a `K/M/G/T` binary suffix.
fn size(text: &str) -> Result<u64, String> {
    let (digits, scale) = match text.trim().as_bytes().last() {
        Some(b'K') => (&text[..text.len() - 1], 1 << 10),
        Some(b'M') => (&text[..text.len() - 1], 1 << 20),
        Some(b'G') => (&text[..text.len() - 1], 1 << 30),
        Some(b'T') => (&text[..text.len() - 1], 1 << 40),
        Some(_) => (text, 1),
        None => return Err("empty size".to_string()),
    };
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("invalid size {text:?}"))?;
    value
        .checked_mul(scale)
        .ok_or_else(|| format!("size {text:?} overflows u64"))
}

fn cpu(args: &[String]) -> Result<(), String> {
    let threads: usize = args
        .first()
        .ok_or("cpu needs a thread count")?
        .parse()
        .map_err(|_| "cpu: expected a thread count".to_string())?;
    let deadline = Instant::now() + secs(args)?;
    let workers: Vec<_> = (0..threads.max(1))
        .map(|_| {
            thread::spawn(move || {
                let mut state = 0_u64;
                while Instant::now() < deadline {
                    for _ in 0..1000 {
                        state = black_box(state.wrapping_mul(6364136223846793005).wrapping_add(1));
                    }
                }
                black_box(state);
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

fn mem(args: &[String]) -> Result<(), String> {
    let start = size(args.first().ok_or("mem needs a size")?)?;
    let target = match value(args, "--grow-to") {
        Some(text) => size(text)?,
        None => start,
    }
    .max(start);
    let step = match value(args, "--step") {
        Some(text) => size(text)?.max(1),
        None => (start / 10).max(1),
    };
    let every = Duration::from_millis(match value(args, "--every") {
        Some(text) => text
            .parse()
            .map_err(|_| "--every: expected milliseconds".to_string())?,
        None => 500,
    });
    let deadline = Instant::now() + secs(args)?;
    // Reserve the final size once so growth never copies; pages are touched by resize.
    let mut pages: Vec<u8> = Vec::with_capacity(target as usize);
    pages.resize(start as usize, 0);
    let mut reached = start;
    while reached < target && Instant::now() < deadline {
        reached = reached.saturating_add(step).min(target);
        pages.resize(reached as usize, 0);
        thread::sleep(every);
    }
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    black_box(&pages);
    Ok(())
}

fn probe(args: &[String]) -> Result<(), String> {
    let deadline = Instant::now() + secs(args)?;
    let interval = Duration::from_millis(10);
    let mut lateness = Vec::new();
    while Instant::now() < deadline {
        let started = Instant::now();
        thread::sleep(interval);
        lateness.push(started.elapsed().saturating_sub(interval).as_micros() as u64);
    }
    lateness.sort_unstable();
    let percentile = |quantile: f64| -> u64 {
        if lateness.is_empty() {
            return 0;
        }
        let index = ((lateness.len() as f64 * quantile).ceil() as usize).saturating_sub(1);
        lateness[index.min(lateness.len() - 1)]
    };
    println!(
        "PROBE p50_us={} p95_us={} samples={}",
        percentile(0.50),
        percentile(0.95),
        lateness.len()
    );
    Ok(())
}

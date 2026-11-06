//! The project name lives here (design §1); dispatch is hand-rolled (§11).

pub const NAME: &str = "slicewise";

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::ExitCode::from(slicewise::cli::main(&args))
}

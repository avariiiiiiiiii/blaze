//! `blaze build|run|test|check|stop ...`, which becomes `ra-jit cargo ...`, the `ra-jit` being
//! the one installed next to it. Cargo runs it the same way as `cargo fmt` and `cargo clippy`
//! once `blaze enable` has linked them to it (`cargo-fmt fmt ...` is `blaze fmt ...`).

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(this) => this.with_file_name(format!("ra-jit{}", std::env::consts::EXE_SUFFIX)),
        Err(error) => {
            eprintln!("error: blaze does not know where it is installed: {error}");
            return ExitCode::FAILURE;
        }
    };
    match Command::new(&exe).arg("cargo").args(std::env::args_os().skip(1)).status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!(
                "error: blaze could not run {}, which is installed with it ({error}); \
                 installing blaze again puts it back",
                exe.display()
            );
            ExitCode::FAILURE
        }
    }
}

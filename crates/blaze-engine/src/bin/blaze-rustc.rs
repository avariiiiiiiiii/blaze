//! What a cargo that one of blaze's loads runs is told to run rustc through: cargo runs
//! `blaze-rustc rustc ...`, and it becomes `blaze-engine rustc-wrapper rustc ...`, the
//! `blaze-engine` being the one installed next to it (`cargo/load_wrapper.rs`). Cargo names a
//! wrapper by its path alone, so the wrapper has a path of its own.

use std::process::{Command, ExitCode};

pub fn main() -> ExitCode {
    let exe = std::env::current_exe()
        .map(|this| this.with_file_name(format!("blaze-engine{}", std::env::consts::EXE_SUFFIX)));
    let why = match exe {
        Ok(exe) => {
            let mut command = Command::new(&exe);
            command.arg("rustc-wrapper").args(std::env::args_os().skip(1));
            match ended(&mut command) {
                Ok(code) => return code,
                Err(error) => format!("{} does not run ({error})", exe.display()),
            }
        }
        Err(error) => format!("blaze-rustc does not know where it is installed ({error})"),
    };
    eprintln!("error: {why}; installing blaze again puts it back");
    ExitCode::FAILURE
}

/// Becomes the command, where a process can.
#[cfg(unix)]
fn ended(command: &mut Command) -> std::io::Result<ExitCode> {
    Err(std::os::unix::process::CommandExt::exec(command))
}

#[cfg(not(unix))]
fn ended(command: &mut Command) -> std::io::Result<ExitCode> {
    Ok(ExitCode::from(command.status()?.code().unwrap_or(1) as u8))
}

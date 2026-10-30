//! Which proc macros run as WebAssembly modules and which natively, in a sandboxed warm server
//! (docs/v2/plan/lanes/usercode.md section 6.5): every one as a module, unless the workspace's root
//! manifest lists its package in `[workspace.metadata.blaze] native-proc-macros` or
//! `BLAZE_PROC_MACRO_WASM` is `0` or `off` for the whole build. Nothing else chooses: a macro
//! that can't be built or run as a module fails, naming it ([`refused`]), and is never run
//! natively in its place.

/// The variable that runs every proc macro natively, `0` or `off`.
pub const WASM: &str = "BLAZE_PROC_MACRO_WASM";

/// The key of `[workspace.metadata.blaze]` that lists the packages run natively.
pub const NATIVE_KEY: &str = "native-proc-macros";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Runs {
    Wasm,
    /// Natively, and why.
    Native(Why),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// The workspace lists its package in [`NATIVE_KEY`].
    Listed,
    /// [`WASM`] turns modules off.
    TurnedOff,
}

/// How the proc macro of the package whose crate is `name` runs, where the workspace lists
/// `native` ([`NATIVE_KEY`], by crate name or package name) and [`WASM`] is `wasm`.
pub fn runs(name: &str, native: &[String], wasm: Option<&str>) -> Result<Runs, String> {
    match wasm {
        None | Some("1" | "on") => {}
        Some("0" | "off") => return Ok(Runs::Native(Why::TurnedOff)),
        Some(other) => {
            return Err(format!("{WASM}={other}: it is `0` or `off` to run proc macros natively"));
        }
    }
    let crate_name = |it: &str| it.replace('-', "_");
    if native.iter().any(|it| crate_name(it) == crate_name(name)) {
        return Ok(Runs::Native(Why::Listed));
    }
    Ok(Runs::Wasm)
}

/// The error of a proc macro that runs as a module and could not be built or run as one, for
/// `why`.
pub fn refused(name: &str, why: &str) -> String {
    format!(
        "the proc macro `{name}` runs as a WebAssembly module and can't: {why}. List its package \
         in `[workspace.metadata.blaze] {NATIVE_KEY}` to run it natively, in a sandboxed server"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_macro_runs_as_a_module_unless_its_package_is_listed_or_modules_are_off() {
        let native = vec!["serde-derive".to_owned()];
        assert_eq!(runs("thiserror_impl", &native, None), Ok(Runs::Wasm));
        assert_eq!(runs("serde_derive", &native, None), Ok(Runs::Native(Why::Listed)));
        assert_eq!(runs("serde-derive", &native, Some("1")), Ok(Runs::Native(Why::Listed)));
        assert_eq!(runs("thiserror_impl", &native, Some("off")), Ok(Runs::Native(Why::TurnedOff)));
        assert!(runs("thiserror_impl", &native, Some("maybe")).unwrap_err().contains(WASM));
    }

    #[test]
    fn a_module_that_fails_names_its_macro_and_the_way_out() {
        let said = refused("serde_derive", "no wasm32-wasip1 target");
        assert!(said.contains("`serde_derive`"));
        assert!(said.contains(NATIVE_KEY));
    }
}

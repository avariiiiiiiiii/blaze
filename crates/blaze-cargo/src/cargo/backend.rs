//! Which backend the workers of a build compile with when `BLAZE_BACKEND` does not say: the
//! one the profile's `opt-level` asks for (docs/cargo.md, "Backends"). A dev build is
//! Cranelift's, a release build LLVM's at the profile's level.

use anyhow::Context as _;
use blaze_manifest::{Config, Context, Workspace, metadata::root_manifest, profiles::Profiles};

use super::cli::Invocation;

const BACKEND: &str = "BLAZE_BACKEND";
const OPTIMIZATION: &str = "BLAZE_OPT";

/// The `BLAZE_OPT` LLVM compiles a profile of `opt-level` at, or `None` for Cranelift.
fn level_of(opt_level: &str) -> Option<&'static str> {
    match opt_level {
        // rustc's `-O1` has no pipeline of LLVM's that blaze runs as its own: Cranelift's code
        // is what a profile that asks for quick builds over fast code gets.
        "0" | "1" => None,
        "2" => Some("2"),
        "3" => Some("3"),
        // blaze's LLVM has no pipeline for size: `s` and `z` are built at `-O2`.
        "s" | "z" => Some("2"),
        _ => None,
    }
}

/// Has every process this one starts compile the build `asked` with the backend its profile's
/// `opt-level` asks for: LLVM at that level for 2, 3, `s` and `z`, with `BLAZE_OPT` the level
/// when it is unset. `BLAZE_BACKEND` set is the backend, whatever the profile. Before any
/// thread is started.
pub(crate) fn follow_profile(asked: &Invocation) -> Result<(), String> {
    if std::env::var_os(BACKEND).is_some() {
        return Ok(());
    }
    let profile = asked.profile();
    let opt_level = opt_level(asked)
        .map_err(|why| format!("blaze could not read the profile `{profile}`: {why:#}"))?;
    let Some(level) = level_of(&opt_level) else { return Ok(()) };
    if !cfg!(feature = "llvm") {
        return Err(format!(
            "the profile `{profile}` has `opt-level = {opt_level}`, which blaze builds with LLVM, \
             and this blaze was built without its `llvm` feature: a released blaze has it, as \
             does a build with `--features llvm`; `BLAZE_BACKEND=cranelift` builds the profile \
             with Cranelift"
        ));
    }
    // SAFETY: before any thread is started.
    unsafe {
        std::env::set_var(BACKEND, "llvm");
        if std::env::var_os(OPTIMIZATION).is_none() {
            std::env::set_var(OPTIMIZATION, level);
        }
    }
    Ok(())
}

/// The `opt-level` of the profile `asked` builds with, as cargo resolves it: the root
/// manifest's `[profile]` tables, the configuration's and `--config`'s over them, and
/// `inherits`.
fn opt_level(asked: &Invocation) -> anyhow::Result<String> {
    let mut config = Config::from_process()?;
    config.start("build", &asked.flags.config)?;
    let ctx = Context::new(config);
    let root = root_manifest(asked.flags.manifest_path.as_deref(), &ctx)?;
    let ws = Workspace::new(&root, &ctx).context("reading the workspace")?;
    let profiles =
        Profiles::new(ws.profiles(), ws.unstable_features(), &ctx.config, asked.profile(), "")?;
    Ok(profiles.base_profile().opt_level)
}

#[cfg(test)]
mod tests {
    use super::level_of;

    #[test]
    fn optimized_profiles_are_llvms() {
        assert_eq!(level_of("0"), None);
        assert_eq!(level_of("1"), None);
        assert_eq!(level_of("2"), Some("2"));
        assert_eq!(level_of("3"), Some("3"));
        assert_eq!(level_of("s"), Some("2"));
        assert_eq!(level_of("z"), Some("2"));
    }
}

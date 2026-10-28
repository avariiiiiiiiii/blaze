//! What the cargo drop-in asks of the engine (crates/blaze-engine), which depends on this crate:
//! the engine hands its functions over with [`set`] before it runs any command.

use std::{path::Path, sync::OnceLock};

pub struct Engine {
    /// `blaze doc`, which loads the workspace in its own process: its exit code.
    pub doc: fn(&[String]) -> anyhow::Result<i32>,
    /// What the stand-ins left in an executable say (`link::stand_ins` of the engine).
    pub stand_ins: fn(&Path) -> anyhow::Result<Vec<String>>,
    /// `blaze pin`, which writes this release into the workspace's manifest
    /// (`versions::pin` of the engine): its exit code.
    pub pin: fn(&[String]) -> anyhow::Result<i32>,
    /// Which blaze this is and why it is the one that runs, for `blaze --version`.
    pub why_this_one: fn() -> String,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn set(engine: Engine) {
    let _ = ENGINE.set(engine);
}

pub fn get() -> &'static Engine {
    ENGINE.get().expect("blaze-engine's main hands over the engine before any command runs")
}

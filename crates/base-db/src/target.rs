//! Information about the target.

use std::fmt;

use blaze_ids::{ContentId, Hasher};
use triomphe::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    // Only what we need is present here.
    Wasm32,
    Wasm64,
    Other,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct TargetData {
    pub data_layout: Box<str>,
    pub arch: Arch,
}

impl TargetData {
    pub const DOMAIN: &'static str = "blaze v2 target data 1";

    pub fn content(&self) -> ContentId {
        let arch = match self.arch {
            Arch::Wasm32 => 0,
            Arch::Wasm64 => 1,
            Arch::Other => 2,
        };
        Hasher::new(TargetData::DOMAIN).part(self.data_layout.as_bytes()).update(&[arch]).finish()
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TargetLoadError(Arc<str>);

impl fmt::Debug for TargetLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for TargetLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for TargetLoadError {}

impl From<String> for TargetLoadError {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl From<&str> for TargetLoadError {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

pub type TargetLoadResult = Result<TargetData, TargetLoadError>;

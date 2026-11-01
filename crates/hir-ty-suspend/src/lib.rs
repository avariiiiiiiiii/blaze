//! Values held across a call that may park a fiber (docs/v2/plan/lanes/may-suspend.md).

#![forbid(unsafe_code)]

mod check;
mod dispatch;
mod root;
mod summary;
#[cfg(test)]
mod tests;

pub use check::{Broken, Found, Held, Promise, Site, Why, suspend_check};
pub use dispatch::Unseen;

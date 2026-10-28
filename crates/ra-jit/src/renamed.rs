//! The names blaze had before it was renamed from ra-jit (#270), which it no longer reads:
//! scripts/rename.py leaves this file as it is.

/// The prefix of the settings' names before the rename, now `BLAZE_`.
const OLD: &str = "RA_JIT_";

/// Ends this process when a setting is named as it was before the rename, with a line for
/// each saying what it is called now: a setting blaze does not read would change nothing
/// without a word.
pub(crate) fn refuse_old_settings() {
    let mut old: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.to_str().filter(|it| it.starts_with(OLD)).map(str::to_owned))
        .collect();
    if old.is_empty() {
        return;
    }
    old.sort();
    for name in &old {
        eprintln!("{name} was renamed BLAZE_{}", &name[OLD.len()..]);
    }
    std::process::exit(1);
}

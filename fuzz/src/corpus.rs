//! The inputs kept, how one is picked to change, and their files: hexadecimal, whitespace
//! ignored, as `blaze_fuzz::inputs` reads a corpus directory.

use std::{
    ffi::OsString,
    fmt::Write as _,
    path::{Path, PathBuf},
};

use crate::mutate::Random;

/// The inputs that reached a feature no input before them did, each weighted by how many it
/// was first to reach, the later ones more (libFuzzer's vanilla schedule favors the new).
#[derive(Default)]
pub(crate) struct Corpus {
    inputs: Vec<Vec<u8>>,
    /// The running sum of the weights, for a pick by binary search.
    sums: Vec<u64>,
    bytes: usize,
}

impl Corpus {
    pub(crate) fn add(&mut self, input: Vec<u8>, new_features: usize) {
        let weight = new_features as u64 * (self.inputs.len() as u64 + 1);
        let sum = self.sums.last().copied().unwrap_or(0) + weight.max(1);
        self.bytes += input.len();
        self.inputs.push(input);
        self.sums.push(sum);
    }

    pub(crate) fn pick(&self, random: &mut Random) -> &[u8] {
        let total = self.sums.last().copied().unwrap_or(0);
        let at = random.below_u64(total);
        &self.inputs[self.sums.partition_point(|&sum| sum <= at)]
    }

    /// The input added last.
    #[cfg(test)]
    pub(crate) fn last(&self) -> &[u8] {
        self.inputs.last().map_or(&[], Vec::as_slice)
    }

    pub(crate) fn len(&self) -> usize {
        self.inputs.len()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
}

/// FNV-1a: a file name per input, the same on every machine.
pub(crate) fn name(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    format!("{hash:016x}")
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2 + bytes.len() / 32 + 1);
    for line in bytes.chunks(32) {
        for byte in line {
            _ = write!(text, "{byte:02x}");
        }
        text.push('\n');
    }
    text
}

pub(crate) fn from_hex(text: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<u8> = text.bytes().filter(|it| !it.is_ascii_whitespace()).collect();
    if digits.len() % 2 == 1 {
        return Err(format!("{} hexadecimal digits: an odd count", digits.len()));
    }
    digits
        .chunks(2)
        .map(|pair| {
            let digit = |d: u8| (d as char).to_digit(16).ok_or_else(|| format!("{:?} is not a hexadecimal digit", d as char));
            Ok((digit(pair[0])? << 4 | digit(pair[1])?) as u8)
        })
        .collect()
}

pub(crate) fn read(path: &Path) -> Result<Vec<u8>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    from_hex(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The names of the files in `dir`, in name order (none when it doesn't exist), but those
/// starting with `.`, which are being written. One listing, no `stat` per file.
pub(crate) fn names(dir: &Path) -> Result<Vec<OsString>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let is_file = entry.file_type().map_err(|e| format!("{}: {e}", entry.path().display()))?.is_file();
        let name = entry.file_name();
        if is_file && !name.as_encoded_bytes().starts_with(b".") {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// The inputs of every file in `dir`, in name order (none when it doesn't exist).
pub(crate) fn read_dir(dir: &Path) -> Result<Vec<Vec<u8>>, String> {
    names(dir)?.iter().map(|name| read(&dir.join(name))).collect()
}

/// The file name [`write`] gives `bytes`.
pub(crate) fn file_name(prefix: &str, bytes: &[u8]) -> OsString {
    OsString::from(format!("{prefix}{}.hex", name(bytes)))
}

/// Writes `bytes` to `dir/<prefix><name>.hex`, and says where. The file appears whole: it is
/// written under a `.` name and renamed, so a `--jobs` worker listing `dir` never reads a part.
pub(crate) fn write(dir: &Path, prefix: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let name = file_name(prefix, bytes);
    let path = dir.join(&name);
    let mut partial = OsString::from(format!(".{}-", std::process::id()));
    partial.push(&name);
    let partial = dir.join(partial);
    std::fs::write(&partial, to_hex(bytes)).map_err(|e| format!("{}: {e}", partial.display()))?;
    std::fs::rename(&partial, &path).map_err(|e| format!("{} to {}: {e}", partial.display(), path.display()))?;
    Ok(path)
}

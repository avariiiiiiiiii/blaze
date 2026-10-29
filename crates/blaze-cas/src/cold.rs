//! Cold values: zstd with a dictionary per kind. Values of one kind (the debug info of compiled
//! functions, the outputs of build-script runs) share most of their structure, which a
//! dictionary trained on them takes out of each.

/// One level for every cold value: compression is deterministic for a level and a dictionary.
const LEVEL: i32 = 6;

#[cfg(not(test))]
pub(crate) const DICTIONARY_SIZE: usize = 64 << 10;
#[cfg(test)]
pub(crate) const DICTIONARY_SIZE: usize = 4 << 10;

/// A kind's dictionary is trained once this many of its values, and [`TRAIN_BYTES`] of them,
/// are kept raw; its values from then on are compressed.
#[cfg(not(test))]
pub(crate) const TRAIN_VALUES: usize = 256;
#[cfg(test)]
pub(crate) const TRAIN_VALUES: usize = 64;

#[cfg(not(test))]
pub(crate) const TRAIN_BYTES: u64 = 2 << 20;
#[cfg(test)]
pub(crate) const TRAIN_BYTES: u64 = 48 << 10;

/// At most this many samples, and bytes of them, go into one training.
pub(crate) const SAMPLES_MOST: usize = 4096;
pub(crate) const SAMPLE_BYTES_MOST: u64 = 32 << 20;

pub(crate) fn train(samples: &[&[u8]]) -> Result<Vec<u8>, String> {
    zstd::dict::from_samples(samples, DICTIONARY_SIZE)
        .map_err(|error| format!("training a dictionary on {} values failed: {error}", samples.len()))
}

pub(crate) fn compress(dictionary: &[u8], raw: &[u8]) -> Result<Vec<u8>, String> {
    zstd::bulk::Compressor::with_dictionary(LEVEL, dictionary)
        .and_then(|mut compressor| compressor.compress(raw))
        .map_err(|error| format!("compressing {} bytes failed: {error}", raw.len()))
}

/// Exactly `raw` bytes, or why not.
pub(crate) fn decompress(dictionary: &[u8], stored: &[u8], raw: u64) -> Result<Vec<u8>, String> {
    let raw = usize::try_from(raw).map_err(|_| "its length does not fit in memory".to_owned())?;
    let bytes = zstd::bulk::Decompressor::with_dictionary(dictionary)
        .and_then(|mut decompressor| decompressor.decompress(stored, raw))
        .map_err(|error| format!("it does not decompress: {error}"))?;
    if bytes.len() != raw {
        return Err(format!("it decompresses to {} bytes, not the {raw} recorded", bytes.len()));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: usize) -> Vec<u8> {
        format!(
            "{{\"unit\": \"crate-{n}\", \"cfg\": [\"feature=\\\"std\\\"\", \"target_os=\\\"linux\\\"\"], \
             \"env\": {{\"OUT_DIR\": \"/build/out/{n}\", \"PROFILE\": \"debug\"}}, \"links\": \
             [\"z\", \"ssl\", \"crypto\"], \"rerun\": [\"build.rs\", \"src/{n}.c\"], \"n\": {}}}\n",
            n * 7919 % 1000
        )
        .repeat(4)
        .into_bytes()
    }

    #[test]
    fn a_dictionary_trained_on_a_kind_shrinks_its_values_and_round_trips() {
        let samples: Vec<Vec<u8>> = (0..200).map(sample).collect();
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
        let dictionary = train(&refs).unwrap();
        assert!(dictionary.len() <= DICTIONARY_SIZE);
        let value = sample(12_345);
        let stored = compress(&dictionary, &value).unwrap();
        assert!(stored.len() * 4 < value.len(), "{} of {}", stored.len(), value.len());
        assert_eq!(decompress(&dictionary, &stored, value.len() as u64).unwrap(), value);
        assert_eq!(compress(&dictionary, &value).unwrap(), stored, "deterministic");
    }

    #[test]
    fn a_wrong_length_or_garbage_is_refused() {
        let samples: Vec<Vec<u8>> = (0..200).map(sample).collect();
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
        let dictionary = train(&refs).unwrap();
        let value = sample(3);
        let stored = compress(&dictionary, &value).unwrap();
        assert!(decompress(&dictionary, &stored, value.len() as u64 + 1).is_err());
        assert!(decompress(&dictionary, &stored, value.len() as u64 - 1).is_err());
        assert!(decompress(&dictionary, b"not zstd at all", 10).is_err());
    }
}

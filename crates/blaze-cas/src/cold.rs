//! Compression. Cold values: zstd with a dictionary per kind. Values of one kind (the debug
//! info of compiled functions, the outputs of build-script runs) share most of their structure,
//! which a dictionary trained on them takes out of each. Long cold values instead get zstd's
//! long-distance matching over a wide window, on several threads. Chunks of hot outputs: LZ4,
//! which decompresses at memory speed.

use zstd::{
    dict::{DecoderDictionary, EncoderDictionary},
    stream::raw::{CParameter, DParameter},
};

/// One level for every cold value: compression is deterministic for a level and a dictionary.
const LEVEL: i32 = 6;

/// From this length on a cold value is compressed by [`compress_long`]: a dictionary gains a
/// long value little, a 128 MiB window and threads gain it much.
#[cfg(not(test))]
pub(crate) const LONG_FROM: usize = 8 << 20;
#[cfg(test)]
pub(crate) const LONG_FROM: usize = 64 << 10;

/// zstd's default limit for decompression, so any zstd reads these frames.
const WINDOW_LOG: u32 = 27;

/// zstd's threaded output depends on its parameters but not on the machine or the number of
/// cores, as long as the worker count is fixed and not zero: so it is fixed here.
const WORKERS: u32 = 4;

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

/// A kind's dictionary, digested once for both directions. Loading 64 KiB of dictionary into a
/// context costs more than compressing or decompressing a small value, so each put and read
/// references this instead of loading the bytes.
pub(crate) struct Prepared {
    bytes: Vec<u8>,
    encoder: EncoderDictionary<'static>,
    decoder: DecoderDictionary<'static>,
}

// SAFETY: a `ZSTD_CDict` and a `ZSTD_DDict` are read-only once created; zstd.h says each can be
// shared by many threads at once (`ZSTD_createCDict`, `ZSTD_createDDict`). `Prepared` makes them
// in `new` and never hands out a way to change them. zstd-safe 7.3 does not say so itself
// (v2/requests/store.md).
unsafe impl Send for Prepared {}
unsafe impl Sync for Prepared {}

impl Prepared {
    pub(crate) fn new(bytes: Vec<u8>) -> Prepared {
        let encoder = EncoderDictionary::copy(&bytes, LEVEL);
        let decoder = DecoderDictionary::copy(&bytes);
        Prepared { bytes, encoder, decoder }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

pub(crate) fn compress(dictionary: &Prepared, raw: &[u8]) -> Result<Vec<u8>, String> {
    Batch::new(Some(dictionary))?.compress(raw)
}

/// One compressor for a batch of values of one kind. Without a dictionary its frames are read
/// by [`decompress_long`].
pub(crate) struct Batch<'d>(zstd::bulk::Compressor<'d>);

impl<'d> Batch<'d> {
    pub(crate) fn new(dictionary: Option<&'d Prepared>) -> Result<Batch<'d>, String> {
        match dictionary {
            Some(dictionary) => zstd::bulk::Compressor::with_prepared_dictionary(&dictionary.encoder),
            None => zstd::bulk::Compressor::new(LEVEL),
        }
        .map(Batch)
        .map_err(|error| format!("starting zstd failed: {error}"))
    }

    pub(crate) fn compress(&mut self, raw: &[u8]) -> Result<Vec<u8>, String> {
        self.0.compress(raw).map_err(|error| format!("compressing {} bytes failed: {error}", raw.len()))
    }
}

pub(crate) fn compress_long(raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut compressor = zstd::bulk::Compressor::new(LEVEL)
        .map_err(|error| format!("starting zstd failed: {error}"))?;
    for parameter in [
        CParameter::EnableLongDistanceMatching(true),
        CParameter::WindowLog(WINDOW_LOG),
        CParameter::NbWorkers(WORKERS),
    ] {
        compressor
            .set_parameter(parameter)
            .map_err(|error| format!("zstd refused {parameter:?}: {error}"))?;
    }
    compressor.compress(raw).map_err(|error| format!("compressing {} bytes failed: {error}", raw.len()))
}

/// Exactly `raw` bytes, or why not.
pub(crate) fn decompress(dictionary: &Prepared, stored: &[u8], raw: u64) -> Result<Vec<u8>, String> {
    let raw = usize::try_from(raw).map_err(|_| "its length does not fit in memory".to_owned())?;
    let bytes = zstd::bulk::Decompressor::with_prepared_dictionary(&dictionary.decoder)
        .and_then(|mut decompressor| decompressor.decompress(stored, raw))
        .map_err(|error| format!("it does not decompress: {error}"))?;
    whole(bytes, raw)
}

/// What [`compress_long`] made: exactly `raw` bytes, or why not.
pub(crate) fn decompress_long(stored: &[u8], raw: u64) -> Result<Vec<u8>, String> {
    let raw = usize::try_from(raw).map_err(|_| "its length does not fit in memory".to_owned())?;
    let bytes = zstd::bulk::Decompressor::new()
        .and_then(|mut decompressor| {
            decompressor.set_parameter(DParameter::WindowLogMax(WINDOW_LOG))?;
            decompressor.decompress(stored, raw)
        })
        .map_err(|error| format!("it does not decompress: {error}"))?;
    whole(bytes, raw)
}

fn whole(bytes: Vec<u8>, raw: usize) -> Result<Vec<u8>, String> {
    if bytes.len() != raw {
        return Err(format!("it decompresses to {} bytes, not the {raw} recorded", bytes.len()));
    }
    Ok(bytes)
}

/// A chunk in LZ4, if that saves at least an eighth of it.
pub(crate) fn lz4(raw: &[u8]) -> Option<Vec<u8>> {
    let stored = lz4_flex::block::compress(raw);
    (stored.len() + raw.len() / 8 <= raw.len()).then_some(stored)
}

pub(crate) fn unlz4(stored: &[u8], raw: u64) -> Result<Vec<u8>, String> {
    let raw = usize::try_from(raw).map_err(|_| "its length does not fit in memory".to_owned())?;
    let bytes = lz4_flex::block::decompress(stored, raw)
        .map_err(|error| format!("it does not decompress: {error}"))?;
    whole(bytes, raw)
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
        let dictionary = Prepared::new(train(&refs).unwrap());
        assert!(dictionary.bytes().len() <= DICTIONARY_SIZE);
        let loaded = zstd::bulk::Decompressor::with_dictionary(dictionary.bytes());
        let value = sample(12_345);
        let stored = compress(&dictionary, &value).unwrap();
        assert!(stored.len() * 4 < value.len(), "{} of {}", stored.len(), value.len());
        assert_eq!(decompress(&dictionary, &stored, value.len() as u64).unwrap(), value);
        assert_eq!(compress(&dictionary, &value).unwrap(), stored, "deterministic");
        let read = loaded.and_then(|mut it| it.decompress(&stored, value.len())).unwrap();
        assert_eq!(read, value, "a frame of the prepared dictionary reads with its bytes loaded");
    }

    #[test]
    fn a_wrong_length_or_garbage_is_refused() {
        let samples: Vec<Vec<u8>> = (0..200).map(sample).collect();
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
        let dictionary = Prepared::new(train(&refs).unwrap());
        let value = sample(3);
        let stored = compress(&dictionary, &value).unwrap();
        assert!(decompress(&dictionary, &stored, value.len() as u64 + 1).is_err());
        assert!(decompress(&dictionary, &stored, value.len() as u64 - 1).is_err());
        assert!(decompress(&dictionary, b"not zstd at all", 10).is_err());
    }

    /// A long value whose repeats are further apart than zstd's default window reaches.
    fn long_value() -> Vec<u8> {
        let mut rng = blaze_core::sim::Rng::new(5);
        let block: Vec<u8> = (0..LONG_FROM / 2).map(|_| rng.next_u64() as u8).collect();
        let mut value = block.clone();
        value.extend((0..LONG_FROM).map(|_| rng.next_u64() as u8));
        value.extend_from_slice(&block);
        value
    }

    #[test]
    fn a_long_value_round_trips_the_same_every_time() {
        let value = long_value();
        let stored = compress_long(&value).unwrap();
        assert!(stored.len() < value.len() * 7 / 8, "{} of {}", stored.len(), value.len());
        assert_eq!(decompress_long(&stored, value.len() as u64).unwrap(), value);
        assert_eq!(compress_long(&value).unwrap(), stored, "deterministic");
        assert!(decompress_long(&stored, value.len() as u64 - 1).is_err());
    }

    #[test]
    fn a_batch_compresses_as_one_call_does_and_without_a_dictionary_reads_as_long() {
        let samples: Vec<Vec<u8>> = (0..200).map(sample).collect();
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
        let dictionary = Prepared::new(train(&refs).unwrap());
        let mut batch = Batch::new(Some(&dictionary)).unwrap();
        for n in [5, 6, 7] {
            let value = sample(n);
            let stored = batch.compress(&value).unwrap();
            assert_eq!(stored, compress(&dictionary, &value).unwrap());
            assert_eq!(decompress(&dictionary, &stored, value.len() as u64).unwrap(), value);
        }
        let mut plain = Batch::new(None).unwrap();
        let value = sample(8);
        let stored = plain.compress(&value).unwrap();
        assert_eq!(decompress_long(&stored, value.len() as u64).unwrap(), value);
    }

    #[test]
    fn lz4_keeps_only_what_it_shrinks_and_round_trips() {
        let text = sample(1).repeat(64);
        let stored = lz4(&text).unwrap();
        assert_eq!(unlz4(&stored, text.len() as u64).unwrap(), text);
        assert!(unlz4(&stored, text.len() as u64 + 1).is_err());
        let mut rng = blaze_core::sim::Rng::new(6);
        let noise: Vec<u8> = (0..64 << 10).map(|_| rng.next_u64() as u8).collect();
        assert_eq!(lz4(&noise), None);
    }
}

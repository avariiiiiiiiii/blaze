//! The global cache (docs/v2/plan/design/compile-time.md, idea 8;
//! docs/v2/plan/lanes/fastcompile/global.md): what depends only on public inputs is computed
//! once, by blaze's builders, and fetched by every machine instead of built.
//!
//! - **What is public** ([`public`]): a store [`Key`] is a hash, so whether its inputs are
//!   public is judged where it is made. A [`public::Rule`] takes how a key was made (a query's
//!   inputs, a build script's or proc macro's run with what it read, an LLVM-optimized
//!   function with the instances it builds in), makes the key again with the owning crate's
//!   own function, and checks every input: a crates.io crate version by its checksum, the
//!   pinned toolchain, the builders' system image. What passes is a [`public::Public`], the
//!   only thing this crate publishes or fetches. A workspace member, a path, git or private
//!   registry dependency, a variable of the machine's environment, or anything not traced is
//!   never public. [`public::Secrets`] refuses to publish a value holding a secret.
//! - **Trust** ([`sign`]): the cache is shared, so the content hash proves the bytes, not that
//!   the key maps to them. Each [`sign::Record`] (key, kind, content hash, length) is signed
//!   with Ed25519 by a publisher key blaze pins ([`sign::PINNED`]), and a fetched value enters
//!   the store only after its signature and then its content hash are checked. One that fails
//!   either is refused with an error naming its key ([`Refused`]), never used, never asked of
//!   another.
//! - **The protocol** ([`client`], [`server`], [`wire`], [`http`], [`tls`]): HTTP/1.1 over
//!   TLS through [`blaze_core`]'s `Connect`, `Send` and `Recv`, sans-IO. One request names many
//!   keys; requests are pipelined on each connection and spread over several. Values travel as
//!   content-defined chunks, and a chunk already sent on a connection is sent again as a
//!   reference ([`wire`]'s windows), so near-equal values share bytes on the wire. The
//!   [`client::Client`] is a [`blaze_cas::Remote`]: what it fetches arrives as the completion
//!   of [`blaze_cas::Store::fetch`] and is kept by the store.
//! - **The cold build as a download** ([`bundle`]): the values of every key a cold build of a
//!   lock file's crates.io dependencies computes, listed in one signed manifest under the
//!   lock's [`bundle::BundleId`], fetched in parallel and placed in the store.

#![forbid(unsafe_code)]

pub mod bundle;
pub mod client;
pub mod http;
pub mod public;
pub mod server;
pub mod sign;
pub mod tls;
pub mod wire;
#[cfg(test)]
mod tests;

use std::fmt;

use blaze_cas::Key;
use blaze_core::{Address, Tag};

pub use crate::{
    bundle::{BundleId, BundleSpec, Download, Downloaded, Locked, Manifest},
    client::{Client, Endpoint, GotBundle, Published},
    public::{FnSource, FunctionMade, Origin, Private, Public, QueryMade, Root, Rule, RunMade, RunPart, Secrets},
    server::{Server, record_key},
    sign::{PINNED, Publisher, PublisherKey, Record, Signed, Trust},
    tls::{Channel, Channels, Tls, TlsClient, TlsServer},
};

/// A value of the global cache that is not used: its signature or its content is not what
/// was published, or what came is not the value asked for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Refused {
    pub key: Key,
    pub what: String,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the global cache's value of {:x} is refused: {}", self.key.0, self.what)
    }
}

/// The tags of one state machine: `base..base + 2^32`.
pub(crate) struct Tags {
    base: u64,
    next: u64,
}

impl Tags {
    pub(crate) fn new(base: u64) -> Tags {
        Tags { base, next: 0 }
    }

    pub(crate) fn next(&mut self) -> Tag {
        let tag = Tag(self.base + (self.next & 0xffff_ffff));
        self.next = self.next.wrapping_add(1);
        tag
    }

    pub(crate) fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.base) < 1 << 32
    }
}

/// Keys and content IDs are BLAKE3 hashes already: their first eight bytes index a table as
/// well as any hash of them would.
#[derive(Default, Clone, Copy)]
pub(crate) struct Prehashed(u64);

impl std::hash::Hasher for Prehashed {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        if let Some(word) = bytes.first_chunk::<8>() {
            self.0 ^= u64::from_le_bytes(*word);
        }
    }

    fn write_usize(&mut self, _: usize) {}
}

/// A map on BLAKE3 hashes, indexed by the hash itself.
pub(crate) type ByHash<K, V> = std::collections::HashMap<K, V, std::hash::BuildHasherDefault<Prehashed>>;

pub(crate) fn place(at: &Address) -> String {
    match at {
        Address::Unix(name) => format!("unix:{name}"),
        Address::Tcp(addr) => format!("tcp:{addr}"),
    }
}

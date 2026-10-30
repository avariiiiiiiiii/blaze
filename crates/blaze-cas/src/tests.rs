use std::{collections::BTreeMap, time::Duration};

use blaze_core::{
    Driver, run_until_idle,
    sim::{Faults, Rng, Sim},
};

use super::*;

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn config(dir: &str) -> Config {
    Config { dir: name(dir), layers: Vec::new(), tags: 1 << 40 }
}

fn key(n: u64) -> Key {
    Key(ContentId::of("blaze v2 store test key 1", &n.to_le_bytes()))
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

/// Bytes of a run's output: alike between runs, as the outputs of build scripts are.
fn run_output(n: u64) -> Vec<u8> {
    format!(
        "cargo:rustc-link-lib=static=crate{n}\ncargo:rustc-cfg=has_feature_{}\n\
         cargo:rerun-if-changed=src/native/{n}.c\ncargo:rustc-env=BUILT_AT=/build/{n}/out\n\
         cargo:warning=compiled {} objects\n",
        n % 13,
        n * 31 % 97
    )
    .repeat(6)
    .into_bytes()
}

/// Runs everything asked for, handing the store its completions; returns the tickets made
/// durable.
fn drive(sim: &mut Sim, store: &mut Store, effects: &mut Vec<Effect>) -> Vec<Ticket> {
    let mut durable = Vec::new();
    run_until_idle(sim, effects, |completion, effects| {
        if store.owns(completion.tag) {
            durable.extend(store.complete(completion, effects));
        }
    });
    durable
}

fn open_with(sim: &mut Sim, config: Config) -> Store {
    let mut effects = Vec::new();
    let mut store = Store::open(config, &mut effects);
    drive(sim, &mut store, &mut effects);
    assert!(store.ready(), "{:?}", store.broken());
    store
}

fn open(sim: &mut Sim, dir: &str) -> Store {
    open_with(sim, config(dir))
}

fn flush(sim: &mut Sim, store: &mut Store) -> Vec<Ticket> {
    let mut effects = Vec::new();
    store.flush(&mut effects);
    drive(sim, store, &mut effects)
}

fn quiet() -> Faults {
    Faults::default()
}

fn bytes_of(store: &Store, key: &Key) -> Option<Vec<u8>> {
    store.get(key).unwrap().map(|it| it.bytes.to_vec())
}

#[test]
fn a_put_is_read_at_once_durable_after_a_flush_and_there_after_reopening() {
    let mut sim = Sim::new(1, quiet());
    let mut store = open(&mut sim, "store");
    let values: Vec<(Key, Kind, Vec<u8>)> = vec![
        (key(1), Kind::Query, b"a query's result".to_vec()),
        (key(2), Kind::Code, noise(5000, 2)),
        (key(3), Kind::Debug, b"debug info".to_vec()),
        (key(4), Kind::Run, run_output(4)),
        (key(5), Kind::Output, noise(1000, 5)),
        (key(6), Kind::Image, Vec::new()),
    ];
    let mut tickets = Vec::new();
    for (key, kind, bytes) in &values {
        tickets.push(store.put(*key, *kind, bytes.clone()).unwrap());
        let entry = store.get(key).unwrap().unwrap();
        assert_eq!((entry.kind, entry.bytes), (*kind, bytes.as_slice()));
        assert_eq!(entry.content, content_of(bytes));
    }
    assert!(tickets.iter().all(|it| !store.durable(*it)));
    let durable = flush(&mut sim, &mut store);
    assert_eq!(durable, tickets);
    assert!(tickets.iter().all(|it| store.durable(*it)));
    for (key, _, bytes) in &values {
        assert_eq!(bytes_of(&store, key).as_ref(), Some(bytes));
    }
    drop(store);
    let store = open(&mut sim, "store");
    for (key, kind, bytes) in &values {
        let entry = store.get(key).unwrap().unwrap();
        assert_eq!((entry.kind, entry.bytes), (*kind, bytes.as_slice()));
    }
    assert!(store.get(&key(99)).unwrap().is_none());
    assert!(!store.has(&key(99)));
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_second_put_keeps_the_first_and_other_bytes_are_a_mismatch() {
    let mut sim = Sim::new(2, quiet());
    let mut store = open(&mut sim, "store");
    let first = store.put(key(1), Kind::Query, b"one".to_vec()).unwrap();
    let again = store.put(key(1), Kind::Query, b"one".to_vec()).unwrap();
    assert!(again >= first);
    let mismatch = store.put(key(1), Kind::Query, b"two".to_vec()).unwrap_err();
    assert_eq!(mismatch.kept, content_of(b"one"));
    assert_eq!(mismatch.given, content_of(b"two"));
    flush(&mut sim, &mut store);
    let durable_again = store.put(key(1), Kind::Query, b"one".to_vec()).unwrap();
    assert!(store.durable(durable_again), "a value already durable needs no wait");
    assert!(store.put(key(1), Kind::Query, b"two".to_vec()).is_err());
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"one");
}

/// A put of what a commit in flight already writes is durable with that commit.
#[test]
fn a_put_of_a_value_in_flight_is_durable_with_its_commit() {
    let mut sim = Sim::new(3, quiet());
    let mut store = open(&mut sim, "store");
    let ticket = store.put(key(1), Kind::Code, b"code".to_vec()).unwrap();
    let mut effects = Vec::new();
    store.flush(&mut effects);
    let again = store.put(key(1), Kind::Code, b"code".to_vec()).unwrap();
    let durable = drive(&mut sim, &mut store, &mut effects);
    assert!(durable.contains(&ticket));
    assert!(store.durable(again));
}

/// The crash test: at any point of any number of commits, under injected IO failures, a crash
/// loses no put that was reported durable, and the store opens with only whole values.
#[test]
fn a_crash_at_any_point_loses_nothing_durable_and_serves_nothing_torn() {
    for seed in 0..120 {
        let mut sim = Sim::new(seed, Faults { fail_one_in: 9, ..Faults::default() });
        let mut expected: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
        let mut durable_keys: Vec<Key> = Vec::new();
        let mut next = 0u64;
        for round in 0..4 {
            let mut store = open(&mut sim, "store");
            for (key, bytes) in &expected {
                if let Some(found) = bytes_of(&store, key) {
                    assert_eq!(&found, bytes, "seed {seed} round {round}");
                }
            }
            for key in &durable_keys {
                assert!(store.has(key), "seed {seed} round {round}: a durable put was lost");
            }
            assert_eq!(store.verify(None), Vec::new(), "seed {seed} round {round}");
            let mut tickets = BTreeMap::new();
            let count = 5 + sim.rng().below(40);
            for _ in 0..count {
                next += 1;
                let (kind, bytes) = match next % 5 {
                    0 => (Kind::Run, run_output(next)),
                    1 => (Kind::Code, noise(200 + (next as usize * 37) % 3000, next)),
                    2 if seed % 4 == 0 => (Kind::Output, noise(300 << 10, next)),
                    3 if seed % 4 == 1 => (Kind::Query, noise(100 << 10, next)),
                    _ => (Kind::Query, noise(64, next)),
                };
                let ticket = store.put(key(next), kind, bytes.clone()).unwrap();
                tickets.insert(ticket, key(next));
                expected.insert(key(next), bytes);
            }
            let mut effects = Vec::new();
            store.flush(&mut effects);
            let polls = sim.rng().below(40);
            for _ in 0..polls {
                sim.submit(&mut effects);
                let mut out = Vec::new();
                sim.poll(&mut out, Duration::ZERO);
                for completion in out {
                    if store.owns(completion.tag) {
                        for ticket in store.complete(completion, &mut effects) {
                            durable_keys.extend(tickets.get(&ticket).copied());
                        }
                    }
                }
            }
            assert!(store.broken().is_none(), "seed {seed}: {:?}", store.broken());
            sim.crash();
        }
    }
}

#[test]
fn a_flipped_byte_of_a_value_is_never_served() {
    let mut sim = Sim::new(4, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Query, vec![b'a'; 1000]).unwrap();
    store.put(key(2), Kind::Query, vec![b'b'; 1000]).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let pack = name("store/pack-query-000000");
    let mut bytes = sim.contents(&pack);
    let at = bytes.len() - 20;
    bytes[at] ^= 1;
    sim.change(&pack, &bytes);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), vec![b'a'; 1000]);
    let corrupt = store.get(&key(2)).unwrap_err();
    assert_eq!(corrupt.pack, pack);
    assert!(corrupt.what.contains("hash"), "{corrupt}");
    let found = store.verify(None);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, key(2));
    assert_eq!(store.verify(Some(Kind::Code)), Vec::new());
}

#[test]
fn a_record_header_that_is_not_whole_stops_the_store_naming_its_pack() {
    let mut sim = Sim::new(5, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, vec![7; 100]).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let pack = name("store/pack-code-000000");
    let mut bytes = sim.contents(&pack);
    bytes[PACK_HEADER as usize + 40] ^= 0xff;
    sim.change(&pack, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(!store.ready());
    assert_eq!(store.broken().unwrap().file, pack);
}

#[test]
fn a_head_of_another_format_is_refused_by_name() {
    let mut sim = Sim::new(6, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, vec![1]).unwrap();
    flush(&mut sim, &mut store);
    let head = name("store/head");
    let mut bytes = sim.contents(&head);
    bytes[8] = 99;
    sim.change(&head, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().unwrap();
    assert_eq!(broken.file, head);
    assert!(broken.what.contains("format 99"), "{broken}");
}

#[test]
fn near_identical_outputs_share_their_chunks() {
    let mut sim = Sim::new(7, quiet());
    let mut store = open(&mut sim, "store");
    let output_bytes = |store: &Store| -> u64 {
        store.tiers[0].packs.iter().filter(|it| it.kind == Kind::Output).map(|it| it.end).sum()
    };
    let first = noise(2 << 20, 70);
    let mut second = first.clone();
    second.splice(5000..5000, *b"a patched function").for_each(drop);
    second[1 << 20] ^= 1;
    store.put(key(1), Kind::Output, first.clone()).unwrap();
    let after_first = output_bytes(&store);
    store.put(key(2), Kind::Output, second.clone()).unwrap();
    let added = output_bytes(&store) - after_first;
    assert!(added * 4 < first.len() as u64, "the second output added {added} bytes");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), first);
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), second);
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), second);
    assert_eq!(store.verify(Some(Kind::Output)), Vec::new());
}

#[test]
fn a_cold_kind_trains_its_dictionary_and_compresses_what_follows() {
    let mut sim = Sim::new(8, quiet());
    let mut store = open(&mut sim, "store");
    let count = cold::TRAIN_VALUES as u64 + 40;
    for n in 0..count {
        store.put(key(n), Kind::Run, run_output(n)).unwrap();
    }
    assert_eq!(store.problems(), &[] as &[String]);
    assert_eq!(store.tiers[0].dictionaries.len(), 1, "one dictionary for runs");
    let (loc, _) = store.tiers[0].delta[&key(count - 1)];
    assert_eq!(loc.form, Form::Zstd);
    assert!(loc.stored * 3 < loc.raw, "{} stored of {}", loc.stored, loc.raw);
    let (first, _) = store.tiers[0].delta[&key(0)];
    assert_eq!(first.form, Form::Raw);
    for n in 0..count {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), run_output(n));
    }
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    for n in 0..count {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), run_output(n), "{n}");
    }
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_large_value_is_a_file_of_its_own_placed_by_sharing_blocks() {
    let mut sim = Sim::new(9, quiet());
    let mut store = open(&mut sim, "store");
    let big = noise(EXTERNAL_FROM + 10, 90);
    store.put(key(1), Kind::Image, big.clone()).unwrap();
    store.put(key(2), Kind::Code, b"small".to_vec()).unwrap();
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), big);
    flush(&mut sim, &mut store);
    let file = name(&format!("store/{}", disk::external_file(&content_of(&big))));
    assert_eq!(sim.contents(&file), big);
    let mut effects = Vec::new();
    let a = store.place(&key(1), name("target/image"), &mut effects).unwrap();
    let b = store.place(&key(2), name("target/small"), &mut effects).unwrap();
    assert_eq!(store.place(&key(3), name("target/none"), &mut effects), None);
    drive(&mut sim, &mut store, &mut effects);
    let placed: BTreeMap<Tag, Result<(), String>> =
        store.placed().into_iter().map(|it| (it.tag, it.result)).collect();
    assert_eq!(placed[&a], Ok(()));
    assert_eq!(placed[&b], Ok(()));
    assert_eq!(sim.contents(&name("target/image")), big);
    assert_eq!(sim.contents(&name("target/small")), b"small");
    // Placing again over what is there replaces it.
    let again = store.place(&key(1), name("target/image"), &mut effects).unwrap();
    drive(&mut sim, &mut store, &mut effects);
    assert_eq!(store.placed()[0], Placed { tag: again, to: name("target/image"), result: Ok(()) });
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), big);
}

#[test]
fn where_blocks_cannot_be_shared_a_place_fails_and_copies_nothing() {
    let mut sim = Sim::new(10, Faults { no_clone: true, ..Faults::default() });
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Query, noise(EXTERNAL_FROM, 100)).unwrap();
    flush(&mut sim, &mut store);
    let mut effects = Vec::new();
    store.place(&key(1), name("out/q"), &mut effects).unwrap();
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    assert!(matches!(&placed[0].result, Err(why) if why.contains("does not copy")), "{placed:?}");
    assert!(!sim.exists(&name("out/q")));
}

#[test]
fn a_layer_is_read_after_the_store_and_never_written() {
    let mut sim = Sim::new(11, quiet());
    let mut base = open(&mut sim, "base");
    base.put(key(1), Kind::Code, b"from the base".to_vec()).unwrap();
    base.put(key(2), Kind::Output, noise(400 << 10, 2)).unwrap();
    flush(&mut sim, &mut base);
    drop(base);
    let before = sim.contents(&name("base/head"));
    let mut store = open_with(&mut sim, Config {
        dir: name("mine"),
        layers: vec![name("base")],
        tags: 1 << 40,
    });
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"from the base");
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), noise(400 << 10, 2));
    let ticket = store.put(key(1), Kind::Code, b"from the base".to_vec()).unwrap();
    assert!(store.durable(ticket));
    assert!(store.put(key(1), Kind::Code, b"other".to_vec()).is_err());
    store.put(key(3), Kind::Code, b"mine".to_vec()).unwrap();
    flush(&mut sim, &mut store);
    assert_eq!(sim.contents(&name("base/head")), before);
    assert!(sim.exists(&name("mine/head")));
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_missing_layer_stops_the_store() {
    let mut sim = Sim::new(12, quiet());
    let mut effects = Vec::new();
    let mut store = Store::open(
        Config { dir: name("mine"), layers: vec![name("nowhere")], tags: 0 },
        &mut effects,
    );
    drive(&mut sim, &mut store, &mut effects);
    assert_eq!(store.broken().unwrap().file, name("nowhere/head"));
}

#[test]
fn the_index_is_rewritten_into_the_other_slot_and_the_head_flipped() {
    let mut sim = Sim::new(13, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..INDEX_AFTER as u64 + 5 {
        store.put(key(n), Kind::Query, n.to_le_bytes().to_vec()).unwrap();
    }
    flush(&mut sim, &mut store);
    assert_eq!(store.tiers[0].head.index.unwrap().slot, 0);
    assert!(store.tiers[0].delta.is_empty(), "the new table holds every record");
    for n in 1000..1000 + INDEX_AFTER as u64 {
        store.put(key(n), Kind::Code, n.to_le_bytes().to_vec()).unwrap();
    }
    flush(&mut sim, &mut store);
    let index = store.tiers[0].head.index.unwrap();
    assert_eq!(index.slot, 1);
    assert_eq!(index.count, 2 * INDEX_AFTER as u64 + 5);
    store.put(key(5000), Kind::Code, b"after".to_vec()).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(store.tiers[0].delta.len(), 1, "only what came after the table is read at open");
    for n in (0..INDEX_AFTER as u64 + 5).chain(1000..1000 + INDEX_AFTER as u64) {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), n.to_le_bytes(), "{n}");
    }
    assert_eq!(bytes_of(&store, &key(5000)).unwrap(), b"after");
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn packs_roll_over_when_full() {
    let mut sim = Sim::new(14, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..80 {
        store.put(key(n), Kind::Code, noise(10 << 10, n)).unwrap();
    }
    let packs = store.tiers[0].packs.len();
    assert!(packs >= 3, "{packs} packs");
    assert!(store.tiers[0].packs.iter().all(|it| it.end <= PACK_MOST));
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    for n in 0..80 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), noise(10 << 10, n));
    }
}

#[test]
fn index_tables_find_every_key_and_no_other() {
    let entries: Vec<(Key, Loc)> = (0..3000)
        .map(|n| {
            (key(n), Loc {
                pack: (n % 7) as u32,
                kind: Kind::ALL[(n % 6) as usize],
                form: Form::Raw,
                offset: n * 16,
                stored: n,
                raw: n,
            })
        })
        .collect();
    let bytes = disk::build_index(&entries);
    let table = Table::new(&bytes).unwrap();
    assert_eq!(table.count(), 3000);
    for (key, loc) in &entries {
        assert_eq!(table.get(key), Some(Ok(*loc)));
    }
    assert_eq!(table.get(&key(1 << 40)), None);
    assert_eq!(table.iter().count(), 3000);
    assert!(Table::new(&bytes[..bytes.len() - 1]).is_err());
}

#[test]
fn a_head_round_trips_and_refuses_any_change() {
    let head = Head {
        generation: 7,
        index: Some(IndexRef { slot: 1, len: 64 + 1024 * 64, hash: content_of(b"x"), count: 3 }),
        packs: vec![
            PackRef { kind: Kind::Code, number: 0, committed: 4096, indexed: 16 },
            PackRef { kind: Kind::Run, number: 2, committed: 16, indexed: 16 },
        ],
        dictionaries: vec![DictionaryRef { kind: Kind::Run, pack: 1, offset: 16 }],
        externals: vec![ExternalRef { content: content_of(b"y"), len: 9 }],
    };
    let bytes = head.encode();
    assert_eq!(Head::decode(&bytes).unwrap(), head);
    for at in 0..bytes.len() {
        let mut changed = bytes.clone();
        changed[at] ^= 1;
        assert!(Head::decode(&changed).is_err(), "byte {at}");
    }
    assert!(Head::decode(&bytes[..bytes.len() - 1]).is_err());
}

#[test]
fn keys_hash_the_schema_the_producer_and_every_input() {
    let schema = SchemaHash(content_of(b"schema"));
    let code = content_of(b"code");
    let (a, b) = (content_of(b"a"), content_of(b"b"));
    let key = Key::of(schema, code, &[a, b]);
    assert_eq!(key, Key::of(schema, code, &[a, b]));
    assert_ne!(key, Key::of(schema, code, &[b, a]));
    assert_ne!(key, Key::of(schema, code, &[a]));
    assert_ne!(key, Key::of(schema, content_of(b"other code"), &[a, b]));
    assert_ne!(key, Key::of(SchemaHash(content_of(b"schema 2")), code, &[a, b]));
}

struct Asked(Vec<(Key, Tag)>);

impl Remote for Asked {
    fn fetch(&mut self, key: &Key, tag: Tag, _: &mut Vec<Effect>) {
        self.0.push((*key, tag));
    }
}

#[test]
fn a_fetched_value_is_verified_before_it_is_kept() {
    let mut sim = Sim::new(15, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(9), Kind::Code, b"here".to_vec()).unwrap();
    let mut remote = Asked(Vec::new());
    let mut effects = Vec::new();
    assert_eq!(store.fetch(&key(9), &mut remote, &mut effects), None, "it has it");
    let good = store.fetch(&key(1), &mut remote, &mut effects).unwrap();
    assert_eq!(store.fetch(&key(1), &mut remote, &mut effects), None, "in flight");
    let bad = store.fetch(&key(2), &mut remote, &mut effects).unwrap();
    let missing = store.fetch(&key(3), &mut remote, &mut effects).unwrap();
    let failed = store.fetch(&key(4), &mut remote, &mut effects).unwrap();
    assert_eq!(remote.0.len(), 4);
    let answer = |answer: remote::Answer| Done::Read(answer.encode());
    let value = |key, bytes: &[u8], content| remote::Answer::Value {
        key,
        kind: Kind::Code,
        content,
        bytes: bytes.to_vec(),
    };
    store.complete(
        Completion { tag: good, done: answer(value(key(1), b"one", content_of(b"one"))) },
        &mut effects,
    );
    store.complete(
        Completion { tag: bad, done: answer(value(key(2), b"evil", content_of(b"two"))) },
        &mut effects,
    );
    store.complete(
        Completion { tag: missing, done: answer(remote::Answer::Missing { key: key(3) }) },
        &mut effects,
    );
    store.complete(
        Completion { tag: failed, done: Done::Failed(blaze_core::IoError("no route".into())) },
        &mut effects,
    );
    let fetched: BTreeMap<Key, Fetched> = store.fetched().into_iter().collect();
    assert!(matches!(fetched[&key(1)], Fetched::Kept(_)));
    assert!(matches!(&fetched[&key(2)], Fetched::Refused(why) if why.contains("hash")));
    assert_eq!(fetched[&key(3)], Fetched::Missing);
    assert!(matches!(&fetched[&key(4)], Fetched::Refused(why) if why.contains("no route")));
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"one");
    assert!(!store.has(&key(2)));
    let durable = flush(&mut sim, &mut store);
    assert_eq!(durable.len(), 2);
}

#[test]
fn a_store_that_cannot_write_stops_and_says_where() {
    let mut sim = Sim::new(16, Faults { fail_one_in: 1, ..Faults::default() });
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().expect("every effect fails");
    assert_eq!(broken.file, name("store/head"));
    assert!(broken.what.contains("injected"), "{broken}");
}

#[test]
fn a_driver_may_hand_over_mappings_by_itself() {
    let mut sim = Sim::new(17, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, b"mapped".to_vec()).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    let mut maps = Vec::new();
    run_until_idle(&mut sim, &mut effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
        maps.extend(effects.extract_if(.., |it| matches!(it, Effect::Map { .. })));
    });
    assert!(!store.ready());
    for effect in maps {
        let Effect::Map { file, .. } = effect else { unreachable!() };
        let bytes = sim.contents(&file);
        store.mapped(file, Mapping(std::sync::Arc::new(bytes)));
    }
    assert!(store.ready(), "{:?}", store.broken());
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"mapped");
}

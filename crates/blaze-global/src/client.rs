//! A machine's side of the global cache: fetches of public keys, publishes from the builders,
//! and bundles, over up to [`CONNECTIONS`] connections to one [`Endpoint`], each carrying up
//! to [`PIPELINE`] requests at once, each request naming many keys.
//!
//! The [`Client`] is the store's [`Remote`] for public keys: [`Client::fetch`] asks
//! [`Store::fetch`] with it, and the answer is a completion the store takes. Before a value
//! is handed to the store its record's signature is checked (or, for a key a verified bundle
//! manifest lists, that its record is the one listed), then its bytes against the record's
//! content hash. A value failing either is answered `Done::Failed` naming the key and why:
//! the store refuses it ([`blaze_cas::Fetched::Refused`]), and it is asked of nothing else.
//! A malformed response, one for other keys than were asked, or a hangup fails what its
//! connection still owes, naming the endpoint and each key; nothing sent is asked again.

use std::collections::VecDeque;

use blaze_cas::{Key, Remote, Store, remote::Answer};
use blaze_core::{Address, Completion, Conn, Done, Effect, IoError, Tag};

use crate::{
    ByHash, Refused, Tags,
    bundle::{BundleId, Manifest},
    http,
    public::Public,
    sign::{Record, Signed, Trust},
    tls::{Channel, Channels, TlsClient},
    wire::{self, BATCH_MOST, LONGEST_VALUE, Reader, RecvWindow, SendWindow},
};

/// The most connections to the cache at once.
pub const CONNECTIONS: usize = 4;
/// The most requests in flight on one connection.
pub const PIPELINE: usize = 4;
/// The most keys one fetch of keys of unknown length names.
pub const BATCH_KEYS: usize = 256;
/// A fetch of listed keys, or a publish, carries about this many bytes of values.
pub const BATCH_BYTES: u64 = 16 << 20;
/// The longest response body taken: one longest value and its framing.
const RESPONSE_MOST: u64 = LONGEST_VALUE + (1 << 20);
const RECV_LEAST: usize = 1 << 20;
const RECV_MOST: usize = 1 << 26;

pub(crate) const FETCH_PATH: &str = "/v1/fetch";
pub(crate) const PUBLISH_PATH: &str = "/v1/publish";
pub(crate) const BUNDLE_PATH: &str = "/v1/bundle";
pub(crate) const BUNDLE_PUBLISH_PATH: &str = "/v1/bundle/publish";

/// Where the cache is: the address to connect to, and the host name its certificate is for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Endpoint {
    pub at: Address,
    pub host: String,
}

/// How a publish ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Published {
    pub tag: Tag,
    pub key: Key,
    pub result: Result<(), String>,
}

/// How a request for a bundle's manifest ended: `Ok(None)` when the cache has none, else
/// the manifest, its signature checked.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GotBundle {
    pub tag: Tag,
    pub id: BundleId,
    pub result: Result<Option<Manifest>, String>,
}

pub struct Client<C: Channels = TlsClient> {
    endpoint: Endpoint,
    channels: C,
    trust: Trust,
    tags: Tags,
    links: Vec<Link<C::Channel>>,
    /// Fetches not yet asked: the store's tag and the key.
    fetches: VecDeque<(Tag, Key)>,
    publishes: VecDeque<(Tag, Signed, Vec<u8>)>,
    bundles: VecDeque<BundleWork>,
    /// The key [`Client::fetch`] is asking the store to fetch through it.
    allowed: Option<Key>,
    /// In [`Client::fetch_many`]: what the store asks waits to be asked together.
    batching: bool,
    /// The records of verified manifests.
    listed: ByHash<Key, Record>,
    published: Vec<Published>,
    got: Vec<GotBundle>,
    /// The store's completions that came of a call with no completions to return (a fetch
    /// whose connection failed as it was asked): returned by the next [`Client::complete`].
    undelivered: Vec<Completion>,
    /// Connects given up while in flight: a connection one of them makes is hung up.
    abandoned: Vec<Tag>,
}

enum BundleWork {
    Get { tag: Tag, id: BundleId },
    Publish { tag: Tag, manifest: Manifest },
}

enum Link<Ch> {
    Connecting(Tag),
    Up(Box<Up<Ch>>),
}

struct Up<Ch> {
    conn: Conn,
    channel: Ch,
    sending: Option<Tag>,
    recv: Option<Tag>,
    /// Bytes for the network not yet handed to a send.
    staged: Vec<u8>,
    /// Plaintext received and not yet a whole response.
    plain: Vec<u8>,
    /// What each request in flight asked, oldest first: responses come in this order.
    asked: VecDeque<Asked>,
    send_window: SendWindow,
    recv_window: RecvWindow,
}

enum Asked {
    Fetch(Vec<(Tag, Key)>),
    Publish(Vec<(Tag, Key)>),
    Bundle(Tag, BundleId),
    BundlePublish(Tag, BundleId),
}

/// Where the outcomes of what a connection asked go.
struct Outcomes<'a> {
    host: &'a str,
    store: &'a mut Vec<Completion>,
    published: &'a mut Vec<Published>,
    got: &'a mut Vec<GotBundle>,
}

impl Outcomes<'_> {
    fn fail(&mut self, asked: Asked, why: &str) {
        let host = self.host;
        let failed = |key: &Key| format!("the global cache at {host}: {why}; asked for {:x}", key.0);
        match asked {
            Asked::Fetch(keys) => self.store.extend(
                keys.iter().map(|(tag, key)| Completion { tag: *tag, done: Done::Failed(IoError(failed(key))) }),
            ),
            Asked::Publish(keys) => self
                .published
                .extend(keys.iter().map(|(tag, key)| Published { tag: *tag, key: *key, result: Err(failed(key)) })),
            Asked::Bundle(tag, id) => self.got.push(GotBundle { tag, id, result: Err(failed(&Key(id.0))) }),
            Asked::BundlePublish(tag, id) => {
                self.published.push(Published { tag, key: Key(id.0), result: Err(failed(&Key(id.0))) })
            }
        }
    }
}

enum Role {
    Connect(usize),
    Send(usize),
    Recv(usize),
    Abandoned,
    Stale,
}

impl<C: Channels> Client<C> {
    /// Tags it asks with are `tags..tags + 2^32`.
    pub fn new(endpoint: Endpoint, channels: C, trust: Trust, tags: u64) -> Client<C> {
        Client {
            endpoint,
            channels,
            trust,
            tags: Tags::new(tags),
            links: Vec::new(),
            fetches: VecDeque::new(),
            publishes: VecDeque::new(),
            bundles: VecDeque::new(),
            allowed: None,
            batching: false,
            listed: ByHash::default(),
            published: Vec::new(),
            got: Vec::new(),
            undelivered: Vec::new(),
            abandoned: Vec::new(),
        }
    }

    pub fn owns(&self, tag: Tag) -> bool {
        self.tags.owns(tag)
    }

    /// Asks `store` to fetch `public` through the cache: `None` when it has the value or is
    /// fetching it. The store's completion is among what [`Client::complete`] returns.
    pub fn fetch(&mut self, store: &mut Store, public: &Public, effects: &mut Vec<Effect>) -> Option<Tag> {
        self.fetch_many(store, std::iter::once(public), effects).pop().map(|(_, tag)| tag)
    }

    /// [`Client::fetch`] of each of `publics`, asked together so they go in as few requests
    /// as their batches take. The keys the store asked for, with their tags.
    pub fn fetch_many<'p>(
        &mut self,
        store: &mut Store,
        publics: impl IntoIterator<Item = &'p Public>,
        effects: &mut Vec<Effect>,
    ) -> Vec<(Key, Tag)> {
        self.batching = true;
        let mut tags = Vec::new();
        for public in publics {
            let key = public.key();
            self.allowed = Some(key);
            tags.extend(store.fetch(&key, self, effects).map(|tag| (key, tag)));
        }
        self.allowed = None;
        self.batching = false;
        self.pump_later(effects);
        tags
    }

    /// Publishes `value` as `signed` says, for `public`; how it went comes through
    /// [`Client::published`] with the tag returned.
    pub fn publish(&mut self, public: &Public, signed: Signed, value: Vec<u8>, effects: &mut Vec<Effect>) -> Tag {
        assert_eq!(signed.record.key, public.key(), "a record of another key than the one shown public");
        assert_eq!(signed.record.kind, public.kind(), "a record of another kind than the key shown public");
        assert_eq!(signed.record.len, value.len() as u64, "a record of another length than its value");
        let tag = self.tags.next();
        self.publishes.push_back((tag, signed, value));
        self.pump_later(effects);
        tag
    }

    /// Asks for the manifest of `id`; it comes through [`Client::bundles`].
    pub fn bundle(&mut self, id: BundleId, effects: &mut Vec<Effect>) -> Tag {
        let tag = self.tags.next();
        self.bundles.push_back(BundleWork::Get { tag, id });
        self.pump_later(effects);
        tag
    }

    /// Publishes a signed manifest; how it went comes through [`Client::published`], keyed by
    /// the bundle's ID.
    pub fn publish_bundle(&mut self, manifest: Manifest, effects: &mut Vec<Effect>) -> Tag {
        let tag = self.tags.next();
        self.bundles.push_back(BundleWork::Publish { tag, manifest });
        self.pump_later(effects);
        tag
    }

    /// The publishes answered since the last call.
    pub fn published(&mut self) -> Vec<Published> {
        std::mem::take(&mut self.published)
    }

    /// The manifests answered since the last call.
    pub fn bundles(&mut self) -> Vec<GotBundle> {
        std::mem::take(&mut self.got)
    }

    /// The record a verified manifest lists for `key`.
    pub fn listed(&self, key: &Key) -> Option<&Record> {
        self.listed.get(key)
    }

    /// Takes a completion of its own. Returns the store's: each the completion of a tag
    /// [`Store::fetch`] asked with, for [`Store::complete`].
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Completion> {
        let mut answers = std::mem::take(&mut self.undelivered);
        let Completion { tag, done } = completion;
        match self.role(tag) {
            Role::Abandoned => {
                self.abandoned.retain(|it| *it != tag);
                if let Done::Connected { conn } = done {
                    effects.push(Effect::Hangup { conn, tag: self.tags.next() });
                }
            }
            Role::Connect(at) => match done {
                Done::Connected { conn } => match self.channels.channel() {
                    Ok(channel) => {
                        self.links[at] = Link::Up(Box::new(Up {
                            conn,
                            channel,
                            sending: None,
                            recv: None,
                            staged: Vec::new(),
                            plain: Vec::new(),
                            asked: VecDeque::new(),
                            send_window: SendWindow::default(),
                            recv_window: RecvWindow::default(),
                        }));
                        self.pump_all(&mut answers, effects);
                    }
                    Err(why) => {
                        effects.push(Effect::Hangup { conn, tag: self.tags.next() });
                        self.connect_failed(at, &why, &mut answers);
                    }
                },
                Done::Failed(error) => self.connect_failed(at, &format!("could not connect: {error}"), &mut answers),
                other => self.connect_failed(at, &format!("a connect completed as {other:?}"), &mut answers),
            },
            Role::Send(at) => match done {
                Done::Sent => {
                    if let Link::Up(up) = &mut self.links[at] {
                        up.sending = None;
                    }
                    self.pump_all(&mut answers, effects);
                }
                Done::Failed(error) => self.drop_link(at, &format!("sending failed: {error}"), &mut answers, effects),
                other => self.drop_link(at, &format!("a send completed as {other:?}"), &mut answers, effects),
            },
            Role::Recv(at) => {
                if let Link::Up(up) = &mut self.links[at] {
                    up.recv = None;
                }
                match done {
                    Done::Read(bytes) if bytes.is_empty() => {
                        self.drop_link(at, "the server hung up", &mut answers, effects)
                    }
                    Done::Read(bytes) => match self.received(at, &bytes, &mut answers) {
                        Ok(true) => self.pump_all(&mut answers, effects),
                        Ok(false) => self.drop_link(at, "the server closed the connection", &mut answers, effects),
                        Err(why) => self.drop_link(at, &why, &mut answers, effects),
                    },
                    Done::Failed(error) => {
                        self.drop_link(at, &format!("receiving failed: {error}"), &mut answers, effects)
                    }
                    other => self.drop_link(at, &format!("a receive completed as {other:?}"), &mut answers, effects),
                }
            }
            Role::Stale => {}
        }
        answers
    }

    /// Hangs up; everything not yet answered fails, the store's fetches returned here.
    pub fn close(&mut self, effects: &mut Vec<Effect>) -> Vec<Completion> {
        let mut answers = std::mem::take(&mut self.undelivered);
        self.fail_queued("the client closed", &mut answers);
        while !self.links.is_empty() {
            self.drop_link(0, "the client closed", &mut answers, effects);
        }
        answers
    }

    fn role(&self, tag: Tag) -> Role {
        if self.abandoned.contains(&tag) {
            return Role::Abandoned;
        }
        for (at, link) in self.links.iter().enumerate() {
            match link {
                Link::Connecting(it) if *it == tag => return Role::Connect(at),
                Link::Up(up) if up.sending == Some(tag) => return Role::Send(at),
                Link::Up(up) if up.recv == Some(tag) => return Role::Recv(at),
                _ => {}
            }
        }
        Role::Stale
    }

    fn connect_failed(&mut self, at: usize, why: &str, answers: &mut Vec<Completion>) {
        self.links.remove(at);
        if self.links.is_empty() {
            self.fail_queued(why, answers);
        }
    }

    /// Fails everything not yet asked.
    fn fail_queued(&mut self, why: &str, answers: &mut Vec<Completion>) {
        let mut outcomes =
            Outcomes { host: &self.endpoint.host, store: answers, published: &mut self.published, got: &mut self.got };
        let fetches: Vec<(Tag, Key)> = self.fetches.drain(..).collect();
        if !fetches.is_empty() {
            outcomes.fail(Asked::Fetch(fetches), why);
        }
        let publishes: Vec<(Tag, Key)> = self.publishes.drain(..).map(|(tag, signed, _)| (tag, signed.record.key)).collect();
        if !publishes.is_empty() {
            outcomes.fail(Asked::Publish(publishes), why);
        }
        for work in self.bundles.drain(..) {
            match work {
                BundleWork::Get { tag, id } => outcomes.fail(Asked::Bundle(tag, id), why),
                BundleWork::Publish { tag, manifest } => outcomes.fail(Asked::BundlePublish(tag, manifest.id), why),
            }
        }
    }

    /// Gives up link `at`, failing what it owes. What is queued goes on another link.
    fn drop_link(&mut self, at: usize, why: &str, answers: &mut Vec<Completion>, effects: &mut Vec<Effect>) {
        match self.links.remove(at) {
            Link::Connecting(tag) => self.abandoned.push(tag),
            Link::Up(up) => {
                effects.push(Effect::Hangup { conn: up.conn, tag: self.tags.next() });
                let why = format!("{why} with {} requests unanswered", up.asked.len());
                let mut outcomes = Outcomes {
                    host: &self.endpoint.host,
                    store: answers,
                    published: &mut self.published,
                    got: &mut self.got,
                };
                for asked in up.asked {
                    outcomes.fail(asked, &why);
                }
            }
        }
        if self.links.is_empty() && self.queued() {
            self.pump_all(answers, effects);
        }
    }

    fn queued(&self) -> bool {
        !self.fetches.is_empty() || !self.publishes.is_empty() || !self.bundles.is_empty()
    }

    /// Takes bytes received on link `at` and answers each whole response. `Ok(false)` when
    /// the server is done with the connection.
    fn received(&mut self, at: usize, bytes: &[u8], answers: &mut Vec<Completion>) -> Result<bool, String> {
        let Client { links, trust, listed, fetches, published, got, endpoint, .. } = self;
        let Link::Up(up) = &mut links[at] else { return Ok(true) };
        let up = &mut **up;
        up.channel.received(bytes, &mut up.plain)?;
        let mut outcomes = Outcomes { host: &endpoint.host, store: answers, published, got };
        let mut open = !up.channel.peer_closed();
        let mut start = 0;
        let result = loop {
            let rest = &up.plain[start..];
            let head = match http::response_head(rest, RESPONSE_MOST) {
                Ok(Some(head)) => head,
                Ok(None) => break Ok(()),
                Err(why) => break Err(format!("a malformed response: {why}")),
            };
            if rest.len() < head.head + head.body {
                break Ok(());
            }
            let Some(asked) = up.asked.pop_front() else {
                break Err("a response to nothing asked".to_owned());
            };
            let body = &rest[head.head..head.head + head.body];
            start += head.head + head.body;
            if head.status != 200 {
                let said = String::from_utf8_lossy(&body[..body.len().min(wire::REASON_MOST)]).into_owned();
                outcomes.fail(asked, &format!("it answered {}: {said}", head.status));
            } else if let Err(why) = answer(asked, body, trust, listed, &mut up.recv_window, fetches, &mut outcomes) {
                break Err(why);
            }
            if head.close {
                open = false;
                break Ok(());
            }
        };
        up.plain.drain(..start);
        result?;
        up.channel.outgoing(&mut up.staged)?;
        Ok(open)
    }

    /// [`Client::pump_all`] from a call that returns no completions: what fails waits for
    /// the next [`Client::complete`].
    fn pump_later(&mut self, effects: &mut Vec<Effect>) {
        let mut answers = Vec::new();
        self.pump_all(&mut answers, effects);
        self.undelivered.extend(answers);
    }

    /// Has each link ask what it can, and opens another while what is queued is more than
    /// the open ones take.
    fn pump_all(&mut self, answers: &mut Vec<Completion>, effects: &mut Vec<Effect>) {
        let mut at = 0;
        while at < self.links.len() {
            match self.pump(at, effects) {
                Ok(()) => at += 1,
                Err(why) => self.drop_link(at, &why, answers, effects),
            }
        }
        let full = self.links.iter().all(|link| matches!(link, Link::Up(up) if up.asked.len() >= PIPELINE));
        if self.queued() && full && self.links.len() < CONNECTIONS {
            let tag = self.tags.next();
            effects.push(Effect::Connect { at: self.endpoint.at.clone(), tag });
            self.links.push(Link::Connecting(tag));
        }
    }

    /// Asks on link `at` what it can take, sends what is staged, and keeps a receive in
    /// flight, so a hangup is seen when nothing is owed.
    fn pump(&mut self, at: usize, effects: &mut Vec<Effect>) -> Result<(), String> {
        let Client { links, fetches, publishes, bundles, listed, tags, endpoint, .. } = self;
        let Link::Up(up) = &mut links[at] else { return Ok(()) };
        let up = &mut **up;
        let mut request = Vec::new();
        while up.asked.len() < PIPELINE {
            let mut body = Vec::new();
            let (path, asked) = if let Some(work) = bundles.pop_front() {
                match work {
                    BundleWork::Get { tag, id } => {
                        wire::header(wire::BUNDLE_ASK, &mut body);
                        body.extend_from_slice(&id.0.0);
                        (BUNDLE_PATH, Asked::Bundle(tag, id))
                    }
                    BundleWork::Publish { tag, manifest } => {
                        wire::header(wire::BUNDLE_PUBLISH, &mut body);
                        manifest.encode(&mut body);
                        (BUNDLE_PUBLISH_PATH, Asked::BundlePublish(tag, manifest.id))
                    }
                }
            } else if !publishes.is_empty() {
                let mut bytes = 0;
                let mut items = Vec::new();
                while let Some((_, _, value)) = publishes.front() {
                    let len = value.len() as u64;
                    if !items.is_empty() && (bytes + len > BATCH_BYTES || items.len() == BATCH_MOST) {
                        break;
                    }
                    bytes += len;
                    items.push(publishes.pop_front().expect("a front"));
                }
                wire::header(wire::PUBLISH_ASK, &mut body);
                body.extend_from_slice(&(items.len() as u32).to_le_bytes());
                for (_, signed, value) in &items {
                    body.extend_from_slice(&signed.encode());
                    up.send_window.write(value, &mut body);
                }
                let keys = items.into_iter().map(|(tag, signed, _)| (tag, signed.record.key)).collect();
                (PUBLISH_PATH, Asked::Publish(keys))
            } else if !fetches.is_empty() {
                let mut bytes = 0;
                let mut keys = Vec::new();
                while let Some((_, key)) = fetches.front() {
                    let len = listed.get(key).map(|it| it.len);
                    let most = if len.is_some() { BATCH_MOST } else { BATCH_KEYS };
                    if !keys.is_empty() && (keys.len() >= most || bytes + len.unwrap_or(0) > BATCH_BYTES) {
                        break;
                    }
                    bytes += len.unwrap_or(0);
                    keys.push(fetches.pop_front().expect("a front"));
                }
                let names: Vec<Key> = keys.iter().map(|(_, key)| *key).collect();
                wire::fetch_ask(&names, &mut body);
                (FETCH_PATH, Asked::Fetch(keys))
            } else {
                break;
            };
            http::write_request(&endpoint.host, path, &body, &mut request);
            up.asked.push_back(asked);
        }
        if !request.is_empty() {
            up.channel.send(&request)?;
        }
        up.channel.outgoing(&mut up.staged)?;
        if up.sending.is_none() && !up.staged.is_empty() {
            let tag = tags.next();
            up.sending = Some(tag);
            effects.push(Effect::Send { conn: up.conn, bytes: std::mem::take(&mut up.staged), tag });
        }
        if up.recv.is_none() {
            let tag = tags.next();
            up.recv = Some(tag);
            let owed = match http::response_head(&up.plain, RESPONSE_MOST) {
                Ok(Some(head)) => (head.head + head.body).saturating_sub(up.plain.len()),
                _ => 0,
            };
            effects.push(Effect::Recv { conn: up.conn, most: owed.clamp(RECV_LEAST, RECV_MOST), tag });
        }
        Ok(())
    }
}

/// Answers what `asked` asked from the response `body`, failing what it can't. An `Err` is
/// a response that is not one to what was asked: the connection can't be trusted after it.
fn answer(
    asked: Asked,
    body: &[u8],
    trust: &Trust,
    listed: &mut ByHash<Key, Record>,
    window: &mut RecvWindow,
    fetches: &mut VecDeque<(Tag, Key)>,
    outcomes: &mut Outcomes<'_>,
) -> Result<(), String> {
    match asked {
        Asked::Fetch(keys) => {
            let mut answered = 0;
            let mut reader = Reader::new(body);
            let result = fetched(&keys, &mut answered, &mut reader, trust, listed, window, fetches, outcomes);
            if let Err(why) = &result {
                outcomes.fail(Asked::Fetch(keys[answered..].to_vec()), &format!("a malformed answer: {why}"));
            }
            result
        }
        Asked::Publish(keys) => {
            let checked = wire::parse_publish_answer(body, keys.len()).and_then(|statuses| {
                if statuses.len() != keys.len() {
                    return Err(format!("{} statuses for {} values", statuses.len(), keys.len()));
                }
                match keys.iter().zip(&statuses).find(|((_, key), (said, _))| said != key) {
                    Some(((_, key), (said, _))) => {
                        Err(format!("a status for {:x} in place of one for {:x}", said.0, key.0))
                    }
                    None => Ok(statuses),
                }
            });
            match checked {
                Ok(statuses) => {
                    for ((tag, key), (_, result)) in keys.into_iter().zip(statuses) {
                        outcomes.published.push(Published { tag, key, result });
                    }
                    Ok(())
                }
                Err(why) => {
                    outcomes.fail(Asked::Publish(keys), &format!("a malformed answer: {why}"));
                    Err(why)
                }
            }
        }
        Asked::Bundle(tag, id) => {
            let result = bundle_answer(&mut Reader::new(body), id, trust);
            if let Ok(Some(manifest)) = &result {
                listed.reserve(manifest.entries.len());
                for entry in &manifest.entries {
                    listed.insert(entry.key, *entry);
                }
            }
            let failed = result.as_ref().err().cloned();
            outcomes.got.push(GotBundle { tag, id, result });
            failed.map_or(Ok(()), Err)
        }
        Asked::BundlePublish(tag, id) => {
            let key = Key(id.0);
            let checked = wire::parse_publish_answer(body, 1).and_then(|statuses| match <[_; 1]>::try_from(statuses) {
                Ok([(said, result)]) if said == key => Ok(result),
                _ => Err("an answer for another bundle".to_owned()),
            });
            match checked {
                Ok(result) => {
                    outcomes.published.push(Published { tag, key, result });
                    Ok(())
                }
                Err(why) => {
                    outcomes.fail(Asked::BundlePublish(tag, id), &format!("a malformed answer: {why}"));
                    Err(why)
                }
            }
        }
    }
}

fn bundle_answer(reader: &mut Reader<'_>, id: BundleId, trust: &Trust) -> Result<Option<Manifest>, String> {
    reader.header(wire::BUNDLE_ANSWER, "a bundle's answer")?;
    let manifest = match reader.u8("which answer")? {
        wire::MISSING => None,
        wire::FOUND => {
            let manifest = Manifest::decode(reader)?;
            if manifest.id != id {
                return Err(format!("the manifest of {:x} in place of {:x}", manifest.id.0, id.0));
            }
            manifest.verify(trust).map_err(|why| format!("the manifest of {:x} is refused: {why}", id.0))?;
            Some(manifest)
        }
        other => return Err(format!("an unknown answer {other}")),
    };
    reader.end("a bundle's answer")?;
    Ok(manifest)
}

/// The answers of one fetch, in the order asked; `answered` counts the keys answered, so on
/// an `Err` the caller fails the rest.
#[allow(clippy::too_many_arguments)]
fn fetched(
    keys: &[(Tag, Key)],
    answered: &mut usize,
    reader: &mut Reader<'_>,
    trust: &Trust,
    listed: &ByHash<Key, Record>,
    window: &mut RecvWindow,
    fetches: &mut VecDeque<(Tag, Key)>,
    outcomes: &mut Outcomes<'_>,
) -> Result<(), String> {
    reader.header(wire::FETCH_ANSWER, "a fetch's answer")?;
    let count = reader.count(33, keys.len(), "answers")?;
    if count != keys.len() {
        return Err(format!("{count} answers for {} keys", keys.len()));
    }
    let mut deferred = Vec::new();
    for &(tag, key) in keys {
        let which = reader.u8("which answer")?;
        let said = reader.key("a key")?;
        if said != key {
            return Err(format!("an answer for {:x} in place of one for {:x}", said.0, key.0));
        }
        match which {
            wire::MISSING => {
                outcomes.store.push(Completion { tag, done: Done::Read(Answer::Missing { key }.encode()) });
            }
            wire::DEFERRED => deferred.push((tag, key)),
            wire::FOUND => {
                let signed = reader.signed()?;
                let record = signed.record;
                if record.key != key {
                    return Err(format!("a record of {:x} in an answer for {:x}", record.key.0, key.0));
                }
                let checked = match listed.get(&key) {
                    Some(listed) if *listed == record => Ok(()),
                    Some(_) => Err(Refused { key, what: "its record is not the one its bundle's manifest lists".into() }),
                    None => trust.verify(&signed),
                };
                let value = window.read(reader, record.len, checked.is_ok())?;
                let done = match checked {
                    Err(refused) => Done::Failed(IoError(refused.to_string())),
                    Ok(()) => {
                        let bytes = value.expect("a value whose record is checked is kept");
                        let found = blaze_cas::content_of(&bytes);
                        if found == record.content {
                            Done::Read(Answer::Value { key, kind: record.kind, content: record.content, bytes }.encode())
                        } else {
                            let what = format!("its bytes hash to {found:x}, not the {:x} its record names", record.content);
                            Done::Failed(IoError(Refused { key, what }.to_string()))
                        }
                    }
                };
                outcomes.store.push(Completion { tag, done });
            }
            other => return Err(format!("an unknown answer {other} (0 missing, 1 deferred, 2 found)")),
        }
        *answered += 1;
    }
    reader.end("a fetch's answer")?;
    for it in deferred.into_iter().rev() {
        fetches.push_front(it);
    }
    Ok(())
}

impl<C: Channels> Remote for Client<C> {
    /// Only for a key [`Client::fetch`] is asking for, or one a verified manifest lists: the
    /// cache is asked for nothing that was not shown public.
    fn fetch(&mut self, key: &Key, tag: Tag, effects: &mut Vec<Effect>) {
        let allowed = self.allowed == Some(*key) || self.listed.contains_key(key);
        assert!(allowed, "the global cache is asked for {:x}, which was not shown public", key.0);
        self.fetches.push_back((tag, *key));
        if !self.batching {
            self.pump_later(effects);
        }
    }
}

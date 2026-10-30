//! The CLI's checkouts as heads (docs/v2/plan/lanes/cargo/notes.md section 6): a request that names
//! `ViewRef::Head(<absolute path>)` builds the checkout as saved.
//!
//! - The first request on a checkout imports it: a [`Watcher`] of its directory scans it into
//!   the head named by its path and keeps the head current. What a build writes and what is no
//!   source is not taken in: the root's `target/` and every `.git`.
//! - A request on a checkout waits until the watcher has seen every change made before it, as
//!   watchman's cookies do: the daemon writes a cookie file under the checkout's `target/`
//!   (outside the view) and holds the request until the watcher reports the cookie and has
//!   committed everything it was told before it. Only then does the session see it.
//! - A checkout whose directory can't be read, or whose watch ended, fails the requests waiting
//!   on it, naming it; the next request watches it again.

use std::collections::BTreeMap;

use blaze_api::{ApiError, Arrived, Reply, Request, RequestRef, Response, ViewRef};
use blaze_core::{Completion, Conn, Done, Effect, FileName, Tag};
use blaze_ids::ContentId;
use blaze_vfs::{Vfs, Watched, Watcher};

/// How many tags one watcher uses ([`Watcher::new`]).
const WATCHER_TAGS: u64 = 1 << 32;

/// What [`Heads`] makes of an arrival or a completion.
#[derive(Debug, PartialEq)]
pub enum Out {
    /// For the session.
    Pass(Arrived),
    /// An answer the daemon sends itself, the session never having seen the request.
    Answer(Reply),
}

/// A request whose checkouts' watchers have not caught up with it.
struct Held {
    from: RequestRef,
    request: Request,
    /// Each checkout it names, and the cookie it waits for there.
    waits: Vec<(String, u64)>,
}

struct Checkout {
    watcher: Watcher,
    /// The checkout's directory under the driver's root.
    dir: FileName,
    /// Cookies written or being written and not seen yet, by number.
    cookies: BTreeMap<u64, Cookie>,
}

struct Cookie {
    file: FileName,
    written: bool,
    seen: bool,
}

pub struct Heads {
    /// The cookies' writes and removes: `tags..tags + 2^32`.
    tags: u64,
    /// The first tag of the next watcher's range.
    next_watcher: u64,
    /// The last tag of every watcher's range.
    watcher_end: u64,
    next_cookie: u64,
    checkouts: BTreeMap<String, Checkout>,
    /// By cookie tag: the checkout and the cookie.
    writing: BTreeMap<Tag, (String, u64)>,
    held: Vec<Held>,
}

/// Whether a head is a checkout's: one the CLI names by the checkout's absolute path.
pub fn is_checkout(head: &str) -> bool {
    head.starts_with('/')
}

/// The checkout heads `view` names.
fn view_heads(view: &ViewRef, out: &mut Vec<String>) {
    match view {
        ViewRef::Head(head) if is_checkout(head) => out.push(head.clone()),
        ViewRef::Head(_) | ViewRef::Root(_) => {}
        ViewRef::WhatIf { base, .. } => view_heads(base, out),
    }
}

/// The checkout heads `request` reads. A commit names a head to move, not to read: it waits for
/// nothing.
pub fn named(request: &Request) -> Vec<String> {
    let mut heads = Vec::new();
    match request {
        Request::Check { view, .. }
        | Request::Build { view, .. }
        | Request::Test { view, .. }
        | Request::Run { view, .. }
        | Request::Why { view, .. }
        | Request::Open { view }
        | Request::Verify { view, .. }
        | Request::CodegenInputs { view, .. }
        | Request::Trace { view, .. }
        | Request::Tables { view } => view_heads(view, &mut heads),
        Request::Diff { from, to } | Request::Reach { from, to, .. } => {
            view_heads(from, &mut heads);
            view_heads(to, &mut heads);
        }
        Request::BestOf { base, .. } => view_heads(base, &mut heads),
        _ => {}
    }
    heads.sort_unstable();
    heads.dedup();
    heads
}

/// Whether the file at `rel` of a checkout is out of its view: the build's `target/` at the
/// root and every `.git`.
pub fn excluded(rel: &str) -> bool {
    rel == "target" || rel.starts_with("target/") || rel.split('/').any(|part| part == ".git")
}

impl Heads {
    /// Cookies take the tags `tags..tags + 2^32`; watchers take ranges of `2^32` from
    /// `watchers` on, up to `watchers_end`.
    pub fn new(tags: u64, watchers: u64, watchers_end: u64) -> Heads {
        Heads {
            tags,
            next_watcher: watchers,
            watcher_end: watchers_end,
            next_cookie: 0,
            checkouts: BTreeMap::new(),
            writing: BTreeMap::new(),
            held: Vec::new(),
        }
    }

    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.tags) < 1 << 32 || self.checkouts.values().any(|it| it.watcher.owns(tag))
    }

    /// The checkout whose head is at `root`, if one is: where an agent's build of it is placed.
    pub fn path_of(&self, vfs: &Vfs, root: ContentId) -> Option<String> {
        self.checkouts.keys().find(|head| vfs.head(head) == Some(root)).cloned()
    }

    /// How many requests wait for a watcher.
    pub fn waiting(&self) -> usize {
        self.held.len()
    }

    /// Takes what arrived: passed on, or held until the watchers of the checkouts it names
    /// have caught up with it.
    pub fn arrive(&mut self, arrived: Arrived, effects: &mut Vec<Effect>, out: &mut Vec<Out>) {
        let (from, request) = match arrived {
            Arrived::Gone { conn } => {
                self.held.retain(|held| held.from.conn != conn);
                out.push(Out::Pass(Arrived::Gone { conn }));
                return;
            }
            Arrived::Request { from, request } => (from, request),
        };
        if let Request::Cancel { id } = request
            && let Some(at) = self.held.iter().position(|it| it.from == RequestRef { conn: from.conn, id })
        {
            let held = self.held.remove(at);
            answer(out, held.from, None, false);
            answer(out, from, None, true);
            return;
        }
        if self.held.iter().any(|it| it.from == from) {
            let error = ApiError::new(
                format!("request {} is already under way on this connection", from.id),
                Some(format!("request {}", from.id)),
            );
            answer(out, from, Some(error), false);
            return;
        }
        let heads = named(&request);
        if heads.is_empty() {
            out.push(Out::Pass(Arrived::Request { from, request }));
            return;
        }
        let mut waits = Vec::with_capacity(heads.len());
        for head in heads {
            match self.cookie(&head, effects) {
                Ok(cookie) => waits.push((head, cookie)),
                Err(error) => {
                    answer(out, from, Some(error), false);
                    return;
                }
            }
        }
        self.held.push(Held { from, request, waits });
    }

    /// A cookie written into `head`'s checkout, its watcher started first if it has none.
    fn cookie(&mut self, head: &str, effects: &mut Vec<Effect>) -> Result<u64, ApiError> {
        if !self.checkouts.contains_key(head) {
            let path = head.trim_end_matches('/');
            let Some(dir) = FileName::new(path.trim_start_matches('/')) else {
                return Err(ApiError::new(
                    format!("the head `{head}` names no directory a checkout can be in"),
                    Some(head.to_owned()),
                ));
            };
            assert!(
                self.next_watcher < self.watcher_end,
                "the daemon has watched 2^32 checkouts and has no tags for another"
            );
            let mut watcher = Watcher::new(head, dir.clone(), self.next_watcher);
            self.next_watcher += WATCHER_TAGS;
            watcher.start(effects);
            self.checkouts.insert(head.to_owned(), Checkout { watcher, dir, cookies: BTreeMap::new() });
        }
        let checkout = self.checkouts.get_mut(head).expect("a checkout just watched");
        let number = self.next_cookie;
        self.next_cookie += 1;
        let file = FileName::new(&format!("{}/target/.blaze-cookie-{}-{number}", checkout.dir, std::process::id()))
            .expect("a cookie's name is a name");
        let tag = Tag(self.tags + (number % (1 << 31)) * 2);
        self.writing.insert(tag, (head.to_owned(), number));
        effects.push(Effect::Write { file: file.clone(), bytes: format!("{number}\n").into_bytes(), tag });
        checkout.cookies.insert(number, Cookie { file, written: false, seen: false });
        Ok(number)
    }

    /// Takes a completion of its own; what was held and is now caught up goes on.
    pub fn complete(&mut self, vfs: &mut Vfs, completion: Completion, effects: &mut Vec<Effect>, out: &mut Vec<Out>) {
        if completion.tag.0.wrapping_sub(self.tags) < 1 << 32 {
            self.cookie_done(completion, out);
        } else {
            self.watched(vfs, completion, effects, out);
        }
        self.release(out);
    }

    fn cookie_done(&mut self, completion: Completion, out: &mut Vec<Out>) {
        // Odd tags are the removes of cookies seen: nothing waits for them.
        let Some((head, number)) = self.writing.remove(&completion.tag) else { return };
        let Some(checkout) = self.checkouts.get_mut(&head) else { return };
        match completion.done {
            Done::Written => {
                if let Some(cookie) = checkout.cookies.get_mut(&number) {
                    cookie.written = true;
                }
            }
            done => {
                let file = checkout.cookies.remove(&number).map(|it| it.file);
                let why = match done {
                    Done::Failed(error) => error.to_string(),
                    other => format!("the driver answered {other:?}"),
                };
                let what = format!(
                    "writing the cookie {} that shows the watcher of {head} has caught up: {why}",
                    file.map_or_else(String::new, |it| format!("/{it}"))
                );
                self.fail_waiting(|it| it.0 == head && it.1 == number, &what, &head, out);
            }
        }
    }

    fn watched(&mut self, vfs: &mut Vfs, completion: Completion, effects: &mut Vec<Effect>, out: &mut Vec<Out>) {
        let Some((head, checkout)) =
            self.checkouts.iter_mut().find(|(_, it)| it.watcher.owns(completion.tag))
        else {
            return;
        };
        let head = head.clone();
        let below = format!("{}/", checkout.dir);
        let rel = |file: &FileName| file.as_str().strip_prefix(&below).map(str::to_owned);
        let done = match completion.done {
            Done::Changed { files, lost } => {
                for cookie in checkout.cookies.values_mut() {
                    let told = files.iter().any(|it| {
                        cookie.file == *it
                            || cookie.file.as_str().strip_prefix(it.as_str()).is_some_and(|rest| rest.starts_with('/'))
                    });
                    if told || (lost && cookie.written) {
                        cookie.seen = true;
                    }
                }
                let files = files.into_iter().filter(|it| rel(it).is_none_or(|rel| !excluded(&rel))).collect();
                Done::Changed { files, lost }
            }
            Done::Listed(listed) => {
                Done::Listed(listed.into_iter().filter(|(it, _)| rel(it).is_none_or(|rel| !excluded(&rel))).collect())
            }
            done => done,
        };
        let watched = checkout.watcher.complete(vfs, Completion { tag: completion.tag, done }, effects);
        let mut ended = None;
        for it in watched {
            match it {
                Watched::Committed(_) | Watched::Lost | Watched::Scanned => {}
                Watched::Conflict { path } => {
                    eprintln!("blaze serve: {head}/{path} changed on disk and by a commit since; the head keeps the commit's");
                }
                Watched::Unreadable { path, error } if path.is_empty() => {
                    ended = Some(format!("the checkout {head} can't be read: {error}"));
                }
                Watched::Unreadable { path, error } => {
                    eprintln!("blaze serve: {head}/{path} changed and can't be read ({error}); the head keeps what it had");
                }
                Watched::Ended(error) => {
                    ended.get_or_insert_with(|| match error {
                        Some(error) => format!("the watch of the checkout {head} ended: {error}"),
                        None => format!("the watch of the checkout {head} was stopped"),
                    });
                }
            }
        }
        if let Some(what) = ended {
            if let Some(mut gone) = self.checkouts.remove(&head) {
                gone.watcher.stop(effects);
            }
            self.writing.retain(|_, it| it.0 != head);
            self.fail_waiting(|it| it.0 == head, &what, &head, out);
        }
    }

    /// Fails every held request with a wait `matches` says, naming `what`.
    fn fail_waiting(&mut self, matches: impl Fn(&(String, u64)) -> bool, what: &str, head: &str, out: &mut Vec<Out>) {
        let (failed, kept): (Vec<Held>, Vec<Held>) =
            std::mem::take(&mut self.held).into_iter().partition(|it| it.waits.iter().any(&matches));
        self.held = kept;
        for held in failed {
            answer(out, held.from, Some(ApiError::new(what, Some(head.to_owned()))), false);
        }
    }

    /// Hands on every held request whose every checkout has seen its cookie and committed what
    /// it was told before, oldest first, and removes the cookies no request waits for.
    fn release(&mut self, out: &mut Vec<Out>) {
        let checkouts = &self.checkouts;
        let caught_up = |(head, number): &(String, u64)| {
            checkouts.get(head).is_some_and(|it| {
                it.watcher.idle() && it.cookies.get(number).is_some_and(|cookie| cookie.seen)
            })
        };
        let (ready, waiting): (Vec<Held>, Vec<Held>) =
            std::mem::take(&mut self.held).into_iter().partition(|it| it.waits.iter().all(caught_up));
        self.held = waiting;
        for held in ready {
            out.push(Out::Pass(Arrived::Request { from: held.from, request: held.request }));
        }
    }

    /// Removes the cookies seen that no held request waits for.
    pub fn sweep(&mut self, effects: &mut Vec<Effect>) {
        let held = &self.held;
        for (head, checkout) in &mut self.checkouts {
            let done: Vec<u64> = checkout
                .cookies
                .iter()
                .filter(|(number, cookie)| {
                    cookie.seen && !held.iter().any(|it| it.waits.iter().any(|(h, n)| h == head && n == *number))
                })
                .map(|(number, _)| *number)
                .collect();
            for number in done {
                let cookie = checkout.cookies.remove(&number).expect("a cookie just found");
                let tag = Tag(self.tags + (number % (1 << 31)) * 2 + 1);
                effects.push(Effect::Remove { file: cookie.file, tag });
            }
        }
    }

    /// Stops every watch.
    pub fn stop(&mut self, effects: &mut Vec<Effect>) {
        for checkout in self.checkouts.values_mut() {
            checkout.watcher.stop(effects);
        }
    }

    /// The requests of `conn` that wait.
    pub fn of(&self, conn: Conn) -> impl Iterator<Item = RequestRef> + '_ {
        self.held.iter().filter(move |it| it.from.conn == conn).map(|it| it.from)
    }
}

fn answer(out: &mut Vec<Out>, to: RequestRef, error: Option<ApiError>, ok: bool) {
    if let Some(error) = error {
        out.push(Out::Answer(Reply { to, response: Response::Error(error) }));
    }
    out.push(Out::Answer(Reply { to, response: Response::Done { ok } }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_dir_and_git_are_out_of_the_view() {
        assert!(excluded("target"));
        assert!(excluded("target/debug/foo"));
        assert!(excluded(".git/HEAD"));
        assert!(excluded("vendor/x/.git/config"));
        assert!(!excluded("src/target.rs"));
        assert!(!excluded("crates/target/src/lib.rs"));
        assert!(!excluded(".cargo/config.toml"));
    }

    #[test]
    fn what_waits_is_what_reads_a_checkout() {
        let head = |it: &str| ViewRef::Head(it.to_owned());
        let targets = blaze_api::Targets {
            packages: Vec::new(),
            profile: String::new(),
            target: None,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            cargo: None,
        };
        let build = Request::Build { view: head("/w"), targets: targets.clone() };
        assert_eq!(named(&build), vec!["/w".to_owned()]);
        let what_if = Request::Check {
            view: ViewRef::WhatIf { base: Box::new(head("/w")), ops: Vec::new() },
            targets: targets.clone(),
        };
        assert_eq!(named(&what_if), vec!["/w".to_owned()]);
        assert!(named(&Request::Build { view: head("ci/main"), targets }).is_empty());
        assert!(named(&Request::Status).is_empty());
        let diff = Request::Diff { from: head("/w"), to: head("/w") };
        assert_eq!(named(&diff), vec!["/w".to_owned()]);
    }
}

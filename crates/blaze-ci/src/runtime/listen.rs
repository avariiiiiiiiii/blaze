//! The webhook's listener, behind the VM's proxy: one request per connection, read on a kept
//! handler thread and checked in place ([`webhook::receive_ref`]), answered 202 or with the
//! refusal's status, deduplicated by delivery id (GitHub redelivers), and its event handed to
//! the service.

use std::{
    collections::{HashSet, VecDeque},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use blaze_api::http::{self, RequestParser};

use super::log;
use crate::{
    config::Secret,
    service::Input,
    webhook::{self, Delivery},
};

/// How many delivery ids are remembered.
pub(crate) const REMEMBERED: usize = 10_000;

/// How many connections are read at once; more are answered 503.
pub(crate) const MOST_CONNECTIONS: usize = 64;

/// How long a peer may take to send or read.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The last [`REMEMBERED`] delivery ids.
#[derive(Default)]
pub(crate) struct Seen {
    order: VecDeque<String>,
    ids: HashSet<String>,
}

impl Seen {
    /// Whether `id` is new; it is remembered either way.
    pub(crate) fn first(&mut self, id: &str) -> bool {
        if self.ids.contains(id) {
            return false;
        }
        if self.order.len() == REMEMBERED
            && let Some(oldest) = self.order.pop_front()
        {
            self.ids.remove(&oldest);
        }
        self.order.push_back(id.to_owned());
        self.ids.insert(id.to_owned());
        true
    }
}

pub(crate) struct Listener {
    pub(crate) secret: Arc<Secret>,
    pub(crate) repo: String,
    pub(crate) service: mpsc::Sender<Input>,
    pub(crate) seen: Mutex<Seen>,
    open: AtomicUsize,
}

impl Listener {
    pub(crate) fn new(secret: Arc<Secret>, repo: String, service: mpsc::Sender<Input>) -> Listener {
        Listener { secret, repo, service, seen: Mutex::new(Seen::default()), open: AtomicUsize::new(0) }
    }
}

pub(crate) fn run(listener: TcpListener, secret: Arc<Secret>, repo: String, service: mpsc::Sender<Input>) {
    serve(listener, Arc::new(Listener::new(secret, repo, service)));
}

/// Accepts connections and hands each to a handler thread. Handlers are started only when
/// every one is busy, up to [`MOST_CONNECTIONS`], and then kept: a delivery costs no thread
/// start (a stack mapped, a clone, an exit), only a channel hand-off to a parked handler.
/// `open` counts connections taken and not yet answered, and there are never fewer handlers
/// than that, so a connection handed off always has a handler free for it.
pub(crate) fn serve(listener: TcpListener, shared: Arc<Listener>) {
    let (to_handlers, streams) = crossbeam_channel::unbounded::<TcpStream>();
    let mut handlers = 0;
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                log(&format!("the webhook's accept failed: {error}"));
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let open = shared.open.fetch_add(1, Ordering::AcqRel) + 1;
        if open > MOST_CONNECTIONS {
            refuse(&shared, &mut stream, "too many deliveries at once; GitHub redelivers");
            continue;
        }
        if open > handlers {
            let (its_shared, its_streams) = (shared.clone(), streams.clone()); // clone: once per handler started
            let started = thread::Builder::new()
                .name(format!("blaze-ci webhook {handlers}")) // alloc: once per handler started
                .spawn(move || handler(&its_shared, &its_streams));
            if let Err(error) = started {
                log(&format!("a webhook handler did not start: {error}")); // alloc: a failed start
                refuse(&shared, &mut stream, "no thread to read the delivery on; GitHub redelivers");
                continue;
            }
            handlers += 1;
        }
        to_handlers.send(stream).expect("the handlers' receiver is held here");
    }
}

/// Answers 503 on the accepting thread and lets go of the connection's place in `open`.
fn refuse(shared: &Listener, stream: &mut TcpStream, said: &str) {
    shared.open.fetch_sub(1, Ordering::AcqRel);
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    let _ = stream.write_all(&answer(503, said));
}

/// One handler: each connection in turn, with one read buffer for all of them.
fn handler(shared: &Listener, streams: &crossbeam_channel::Receiver<TcpStream>) {
    let mut buffer = vec![0; 64 << 10]; // alloc: once per handler thread
    for mut stream in streams {
        handle(&mut stream, shared, &mut buffer);
        drop(stream);
        shared.open.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle(stream: &mut TcpStream, shared: &Listener, buffer: &mut [u8]) {
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    let mut parser = RequestParser::new(webhook::MOST_PAYLOAD);
    loop {
        match parser.next_ref() {
            Ok(Some(request)) => {
                let (status, said) = match webhook::receive_ref(shared.secret.expose(), &request) {
                    Ok(delivery) => accept(shared, delivery),
                    Err(error) => (error.status, error.what),
                };
                let _ = stream.write_all(&answer(status, &said));
                return;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = stream.write_all(&answer(error.status, &error.what));
                return;
            }
        }
        match stream.read(buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => parser.push(&buffer[..read]),
        }
    }
}

/// What a checked delivery is answered with; its event goes to the service once.
pub(crate) fn accept(shared: &Listener, delivery: Delivery) -> (u16, String) {
    let Some(event) = delivery.event else {
        return (202, "nothing for CI to run".to_owned());
    };
    if delivery.repo != shared.repo {
        return (
            403,
            format!("a delivery for {}, and this service is for {}", delivery.repo, shared.repo),
        );
    }
    if !shared.seen.lock().unwrap_or_else(PoisonError::into_inner).first(&delivery.id) {
        return (202, format!("delivery {} was already received", delivery.id));
    }
    if shared.service.send(Input::Event(event)).is_err() {
        return (503, "the service has stopped".to_owned());
    }
    (202, "accepted".to_owned())
}

fn answer(status: u16, said: &str) -> Vec<u8> {
    let headers = [("Content-Type", "text/plain; charset=utf-8"), ("Connection", "close")];
    http::response(status, &headers, format!("{said}\n").as_bytes())
}

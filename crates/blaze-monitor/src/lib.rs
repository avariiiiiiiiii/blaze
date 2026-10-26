//! The monitoring mirror of blaze's daemon (`docs/monitor.md`): what the client, the daemon
//! and the workers tell it, as entities, components and relations of a Flecs world that one
//! thread owns and serves over Flecs REST on localhost.
//!
//! Salsa stays the source of truth. Nothing here is read back to decide a build: the world
//! is written from [`Event`]s sent after the fact, and lost events only make it less
//! complete.

pub mod event;
mod world;

use std::{net::TcpListener, time::Duration};

use crossbeam_channel::Sender;

pub use event::Event;

/// The first port the monitor tries, and how many after it: the first that is free is used.
pub const PORT: u16 = 27870;
const PORTS: u16 = 16;

/// Where the REST server listens.
#[derive(Clone, Copy, Debug)]
pub enum Listen {
    /// The first free port from [`PORT`] on.
    Scan,
    Port(u16),
    /// No REST server: the world is kept for the observers only.
    Nowhere,
}

/// The running mirror: send it events.
pub struct Monitor {
    events: Sender<Event>,
    /// The port REST serves on, when it does.
    pub port: Option<u16>,
}

impl Monitor {
    /// Starts the thread that owns the world, and its REST server as `listen` says.
    pub fn start(listen: Listen) -> std::io::Result<Monitor> {
        let port = match listen {
            Listen::Scan => (PORT..PORT + PORTS).find(|&it| free(it)),
            Listen::Port(port) => free(port).then_some(port),
            Listen::Nowhere => None,
        };
        let (events, received) = crossbeam_channel::unbounded();
        let (ready, readied) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("blaze-monitor".to_owned())
            .spawn(move || world::run(&received, port, &ready))?;
        let port = readied
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| std::io::Error::other("the monitor's world did not start"))?;
        Ok(Monitor { events, port })
    }

    /// Hands `event` to the world's thread; it never waits for it.
    pub fn send(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

/// Whether nothing listens on `port` of the loopback address. Flecs binds without saying
/// whether it could, so the port is tried first.
fn free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

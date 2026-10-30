use blaze_core::Conn;
use blaze_ids::{ContentId, Idx};
use blaze_vfs::View;

use super::Intents;
use crate::build::{Builder, JobId, Opened, Work};

/// The jobs cancelled and promoted, in order.
#[derive(Default)]
struct Calls {
    cancelled: Vec<u64>,
    promoted: Vec<u64>,
}

impl Builder for Calls {
    fn start(&mut self, _: JobId, _: Option<Opened>, _: Work) {
        unreachable!("intents only cancel and promote");
    }

    fn cancel(&mut self, job: JobId) {
        self.cancelled.push(job.0);
    }

    fn speculate(&mut self, _: JobId, _: Opened, _: Work) {
        unreachable!("intents only cancel and promote");
    }

    fn promote(&mut self, job: JobId) {
        self.promoted.push(job.0);
    }
}

fn opened(root: u8) -> Opened {
    Opened { view: View { root: ContentId([root; 32]) }, files: Idx::from_raw(0), queries: Idx::from_raw(0) }
}

#[test]
fn intents_go_in_job_order_and_a_warm_tick_allocates_nothing() {
    let (mut intents, mut calls) = (Intents::default(), Calls::default());
    intents.tick(&mut calls, 1_000);
    for job in [5, 3, 9] {
        intents.add(&mut calls, Conn(job), JobId(job), opened(1), None, 1);
    }
    intents.add(&mut calls, Conn(7), JobId(4), opened(2), None, 1);
    intents.take_over(&mut calls, ContentId([1; 32]));
    assert_eq!(calls.promoted, [3, 5, 9]);
    intents.tick(&mut calls, 1_001_000);
    assert_eq!(calls.cancelled, [4]);
    assert_eq!(intents.deadline(), None);

    for job in [12, 10, 11] {
        intents.add(&mut calls, Conn(job), JobId(job), opened(3), None, 1);
    }
    calls.cancelled.reserve(8);
    blaze_alloc_count::assert_at_most(0, "a warm tick that cancels three intents", || {
        intents.tick(&mut calls, 3_000_000);
    });
    assert_eq!(calls.cancelled, [4, 10, 11, 12]);
    assert_eq!(intents.views(), 7);
}

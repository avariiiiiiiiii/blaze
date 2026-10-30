//! How often each query ran, and the first to start, while `RUSTD_TRACE` is set: a database
//! made then reports every execution here, and the build prints what [`take`] and [`order`]
//! say after each request.

use std::sync::Mutex;

use rustc_hash::FxHashMap;
use salsa::{Database as _, Event, EventKind, IngredientIndex};

use super::Db;

/// How many of the first queries to start [`order`] names.
const ORDER_KEPT: usize = 300;

static RAN: Mutex<Option<FxHashMap<IngredientIndex, u32>>> = Mutex::new(None);
static ORDER: Mutex<Vec<IngredientIndex>> = Mutex::new(Vec::new()); // alloc: empty until traced

/// Whether a database made now counts its executions.
pub(super) fn traced() -> bool {
    std::env::var_os("RUSTD_TRACE").is_some()
}

pub(super) fn record(event: &Event) {
    let EventKind::WillExecute { database_key } = event.kind else { return };
    let query = database_key.ingredient_index();
    let mut ran = RAN.lock().unwrap_or_else(|it| it.into_inner());
    *ran.get_or_insert_default().entry(query).or_default() += 1;
    let mut order = ORDER.lock().unwrap_or_else(|it| it.into_inner());
    if order.len() < ORDER_KEPT {
        order.push(query);
    }
}

/// The first queries that started since the last call, in the order they started.
pub fn order(db: &Db) -> Vec<String> {
    let order = std::mem::take(&mut *ORDER.lock().unwrap_or_else(|it| it.into_inner()));
    // alloc: RUSTD_TRACE's report, once per request and only when traced
    order.into_iter().map(|query| db.ingredient_debug_name(query).into_owned()).collect()
}

/// The queries that ran since the last call, most often first.
pub fn take(db: &Db) -> Vec<(String, u32)> {
    let ran = RAN.lock().unwrap_or_else(|it| it.into_inner()).take().unwrap_or_default();
    let mut ran: Vec<_> = ran
        .into_iter()
        .map(|(query, count)| (db.ingredient_debug_name(query).into_owned(), count))
        .collect(); // alloc: RUSTD_TRACE's report, once per request and only when traced
    ran.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ran
}

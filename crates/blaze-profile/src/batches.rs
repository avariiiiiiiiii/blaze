//! The flight recorder's rows ([`flight::Read`]) as Arrow record batches of the schema's tables:
//! what the Parquet history writes, the live feed sends as IPC, and the explorer reads. Names
//! the ring keeps as numbers (sites and [`flight::Sym`]s) are resolved here, once per distinct
//! number a batch, into each dictionary column's values.

use std::sync::{Arc, OnceLock};

use arrow_array::{
    Array, ArrayRef, DictionaryArray, Float64Array, Int64Array, RecordBatch, StringArray, UInt16Array,
    UInt32Array, UInt64Array, types::UInt32Type,
};
use arrow_schema::{ArrowError, SchemaRef};
use rustc_hash::FxHashMap;

use crate::{
    flight::{self, CounterRow, Read, SpanRow, ThreadNow},
    schema::{self, Table},
};

/// What every row of a batch from one process shares.
#[derive(Clone, Copy, Debug)]
pub struct Process<'a> {
    pub pid: u32,
    /// What the process is: `blaze serve`, `blaze build`, a build script's crate.
    pub name: &'a str,
    /// The workspace's commit, when the process builds one checkout.
    pub commit: Option<&'a str>,
}

impl Process<'static> {
    /// This process, with no commit.
    pub fn this() -> Process<'static> {
        static NAME: OnceLock<String> = OnceLock::new();
        Process { pid: std::process::id(), name: NAME.get_or_init(crate::command), commit: None }
    }
}

/// The Arrow schema of `table`, made once a process.
pub fn schema_of(table: &'static Table) -> SchemaRef {
    const ALL: usize = schema::TABLES.len() + 2;
    static SCHEMAS: [OnceLock<SchemaRef>; ALL] = [const { OnceLock::new() }; ALL];
    let results = [&schema::TILES, &schema::OPEN];
    let at = schema::TABLES
        .iter()
        .chain(&results)
        .position(|it| it.name == table.name)
        .unwrap_or_else(|| unreachable!("{} is a table of the schema", table.name));
    let it = &SCHEMAS[at];
    // clone: an `Arc` of a schema made once
    it.get_or_init(|| Arc::new(crate::arrow::schema(table))).clone()
}

/// A column of a batch.
fn column(array: impl Array + 'static) -> ArrayRef {
    // alloc: a column, once a batch, never per row
    Arc::new(array)
}

/// A dictionary column being built: each distinct number given a key the first time, its
/// string looked up then.
struct Dict {
    keys_of: FxHashMap<u64, u32>,
    values: Vec<&'static str>,
}

impl Dict {
    fn new() -> Dict {
        // alloc: a dictionary's values, once a column a batch
        Dict { keys_of: FxHashMap::default(), values: Vec::new() }
    }

    fn key(&mut self, number: u64, text: impl FnOnce() -> &'static str) -> u32 {
        let next = self.values.len() as u32;
        let values = &mut self.values;
        *self.keys_of.entry(number).or_insert_with(|| {
            values.push(text());
            next
        })
    }

    fn finish(self, keys: UInt32Array) -> Result<ArrayRef, ArrowError> {
        let values = column(StringArray::from(self.values));
        Ok(column(DictionaryArray::<UInt32Type>::try_new(keys, values)?))
    }
}

/// A dictionary column of `values`, keyed as they come.
fn dict<'a>(values: impl Iterator<Item = &'a str>) -> ArrayRef {
    column(values.collect::<DictionaryArray<UInt32Type>>())
}

/// A dictionary column of one value, `rows` times; null throughout when `value` is `None`.
fn constant(value: Option<&str>, rows: usize) -> Result<ArrayRef, ArrowError> {
    let values = column(StringArray::from_iter_values(value));
    let keys = match value {
        Some(_) => UInt32Array::from_value(0, rows),
        None => UInt32Array::new_null(rows),
    };
    Ok(column(DictionaryArray::<UInt32Type>::try_new(keys, values)?))
}

fn build_id(build: u32) -> Option<u64> {
    (build != 0).then_some(u64::from(build))
}

/// The `spans` rows of `spans`, all from `process`.
pub fn spans(spans: &[SpanRow], process: Process<'_>) -> Result<RecordBatch, ArrowError> {
    let rows = spans.len();
    let (mut threads, mut names, mut targets, mut kinds, mut crates) =
        (Dict::new(), Dict::new(), Dict::new(), Dict::new(), Dict::new());
    // alloc: a batch's key columns, once a read, never per span written
    let (mut thread_keys, mut name_keys, mut target_keys) =
        (Vec::with_capacity(rows), Vec::with_capacity(rows), Vec::with_capacity(rows));
    // alloc: a batch's key columns, once a read, never per span written
    let (mut kind_keys, mut crate_keys, mut functions, mut units) = (
        Vec::with_capacity(rows),
        Vec::with_capacity(rows),
        Vec::with_capacity(rows),
        Vec::with_capacity(rows),
    );
    for row in spans {
        let meta = flight::meta(row.site);
        let name = meta.map_or("?", |it| it.name());
        thread_keys.push(threads.key(u64::from(row.thread_name), || {
            flight::name(row.thread_name).unwrap_or("?")
        }));
        name_keys.push(names.key(u64::from(row.site), || name));
        target_keys.push(targets.key(u64::from(row.site), || meta.map_or("?", |it| it.target())));
        // A rust-analyzer query's span is named for its query.
        kind_keys.push(name.ends_with("_query").then(|| kinds.key(u64::from(row.site), || name)));
        crate_keys.push(
            (row.krate != 0)
                .then(|| crates.key(u64::from(row.krate), || flight::name(row.krate).unwrap_or("?"))),
        );
        functions.push(flight::name(row.function));
        units.push(flight::name(row.unit));
    }
    // alloc: a batch's list of columns, once a read
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter(spans.iter().map(|it| build_id(it.build)))),
        constant(process.commit, rows)?,
        column(UInt32Array::from_value(process.pid, rows)),
        constant(Some(process.name), rows)?,
        column(UInt32Array::from_iter_values(spans.iter().map(|it| it.thread))),
        threads.finish(UInt32Array::from(thread_keys))?,
        column(UInt64Array::from_iter_values(spans.iter().map(|it| it.span_id))),
        column(UInt64Array::from_iter(spans.iter().map(|it| (it.parent != 0).then_some(it.parent)))),
        column(UInt16Array::from_iter_values(spans.iter().map(|it| it.depth))),
        names.finish(UInt32Array::from(name_keys))?,
        targets.finish(UInt32Array::from(target_keys))?,
        column(Int64Array::from_iter_values(spans.iter().map(|it| it.start_ns))),
        column(Int64Array::from_iter_values(spans.iter().map(|it| it.dur_ns))),
        // The ring keeps no CPU time; BLAZE_PROFILE's tables have it.
        column(Int64Array::new_null(rows)),
        column(UInt64Array::from_iter(spans.iter().map(|it| it.alloc.map(|it| it.bytes)))),
        column(UInt64Array::from_iter(spans.iter().map(|it| it.alloc.map(|it| it.count)))),
        kinds.finish(UInt32Array::from(kind_keys))?,
        crates.finish(UInt32Array::from(crate_keys))?,
        column(StringArray::from(functions)),
        column(StringArray::from(units)),
    ];
    RecordBatch::try_new(schema_of(&schema::SPANS), columns)
}

/// The `counters` rows of `counters`, all from `process`.
pub fn counters(counters: &[CounterRow], process: Process<'_>) -> Result<RecordBatch, ArrowError> {
    let rows = counters.len();
    let mut names = Dict::new();
    let name_keys =
        UInt32Array::from_iter_values(counters.iter().map(|it| names.key(it.plot as u64, || it.plot.name())));
    // alloc: a batch's list of columns, once a read
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter(counters.iter().map(|it| build_id(it.build)))),
        column(UInt32Array::from_value(process.pid, rows)),
        names.finish(name_keys)?,
        column(Int64Array::from_iter_values(counters.iter().map(|it| it.t_ns))),
        column(Float64Array::from_iter_values(counters.iter().map(|it| it.value as f64))),
    ];
    RecordBatch::try_new(schema_of(&schema::COUNTERS), columns)
}

/// The `open` rows of `threads`, read at `at_ns`, all from `process`.
pub fn open(threads: &[ThreadNow], at_ns: i64, process: Process<'_>) -> Result<RecordBatch, ArrowError> {
    let rows = threads.iter().map(|it| it.open.len()).sum();
    let spans = || threads.iter().flat_map(|thread| thread.open.iter().enumerate().map(move |it| (thread, it)));
    // alloc: a batch's list of columns, once a read of the threads
    let columns: Vec<ArrayRef> = vec![
        column(UInt32Array::from_value(process.pid, rows)),
        column(UInt32Array::from_iter_values(spans().map(|(thread, _)| thread.tid as u32))),
        dict(spans().map(|(thread, _)| thread.name.as_str())),
        column(UInt16Array::from_iter_values(spans().map(|(_, (depth, _))| depth as u16))),
        dict(spans().map(|(_, (_, span))| span.name)),
        dict(spans().map(|(_, (_, span))| span.target)),
        column(Int64Array::from_iter_values(spans().map(|(_, (_, span))| at_ns - span.for_ns as i64))),
    ];
    RecordBatch::try_new(schema_of(&schema::OPEN), columns)
}

/// Both tables of a read: `spans` then `counters`.
pub fn of(read: &Read, process: Process<'_>) -> Result<[RecordBatch; 2], ArrowError> {
    Ok([spans(&read.spans, process)?, counters(&read.counters, process)?])
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use arrow_array::{Array, cast::AsArray};

    use super::*;

    #[test]
    fn a_read_is_a_batch_of_each_table_in_the_schema() {
        crate::install();
        std::thread::spawn(|| {
            let _build = flight::building(9);
            let _span = tracing::info_span!(target: "blaze", "batched", krate = "app").entered();
            flight::plot(flight::Plot::Queued, 4);
        })
        .join()
        .unwrap();
        let mut read = Read::default();
        flight::window(Duration::from_secs(60), &mut read);
        let process = Process { pid: 7, name: "blaze test", commit: Some("abc") };
        let [spans, counters] = of(&read, process).unwrap();
        assert_eq!(spans.schema().metadata()["blaze.table"], "spans");
        assert_eq!(spans.num_rows(), read.spans.len());
        assert_eq!(counters.num_rows(), read.counters.len());
        let at = read
            .spans
            .iter()
            .position(|it| flight::meta(it.site).map(|it| it.name()) == Some("batched"))
            .expect("the span was read");
        let column = |name: &str| spans.column(schema::SPANS.column(name).unwrap());
        let dict = |name: &str, row: usize| {
            let it = column(name).as_dictionary::<UInt32Type>();
            let values = it.values().as_string::<i32>();
            it.keys().is_valid(row).then(|| values.value(it.keys().value(row) as usize).to_owned())
        };
        assert_eq!(dict("name", at).as_deref(), Some("batched"));
        assert_eq!(dict("crate", at).as_deref(), Some("app"));
        assert_eq!(dict("commit", at).as_deref(), Some("abc"));
        assert_eq!(dict("query_kind", at), None);
        assert_eq!(column("build_id").as_primitive::<arrow_array::types::UInt64Type>().value(at), 9);
        let plotted = counters.column(schema::COUNTERS.column("value").unwrap());
        assert!(plotted.as_primitive::<arrow_array::types::Float64Type>().values().contains(&4.0));
    }

    #[test]
    fn an_empty_read_is_empty_batches() {
        let [spans, counters] = of(&Read::default(), Process::this()).unwrap();
        assert_eq!((spans.num_rows(), counters.num_rows()), (0, 0));
    }
}

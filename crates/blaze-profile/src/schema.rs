//! The one schema of what blaze records about itself: every reader (the Parquet history,
//! `blaze debug sql`, `blaze debug live`, the explorer at ix.dev/explore, the stall reports)
//! reads these tables, and nothing else defines them. The Arrow schema of each table is made
//! from it (`arrow.rs`), and so are the DuckDB views and the table of columns in
//! docs/profiling.md.
//!
//! Time: every `*_ns` instant is nanoseconds on the machine's monotonic clock ([`CLOCK`]): the
//! same clock in every process of the machine, so the spans of the client, the daemon and the
//! build scripts line up without adjusting. Durations are nanoseconds.

/// Bumped when a column changes meaning or is removed; adding a nullable column at the end of
/// a table does not bump it.
pub const VERSION: u32 = 1;

/// The name of the clock the `*_ns` instants count on, as `hello` says it.
#[cfg(target_os = "linux")]
pub const CLOCK: &str = "linux CLOCK_MONOTONIC";
#[cfg(target_vendor = "apple")]
pub const CLOCK: &str = "darwin CLOCK_UPTIME_RAW";
#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
pub const CLOCK: &str = "std Instant";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    U16,
    U32,
    U64,
    I64,
    F64,
    Utf8,
    /// A string from a small set (names, kinds): dictionary-encoded, `u32` keys.
    Dict,
    /// A call stack, outermost frame first, each frame a [`Type::Dict`] string.
    Stack,
}

#[derive(Clone, Copy, Debug)]
pub struct Column {
    pub name: &'static str,
    pub ty: Type,
    pub nullable: bool,
    pub doc: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct Table {
    pub name: &'static str,
    pub doc: &'static str,
    pub columns: &'static [Column],
}

impl Table {
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|it| it.name == name)
    }
}

const fn col(name: &'static str, ty: Type, doc: &'static str) -> Column {
    Column { name, ty, nullable: false, doc }
}

const fn opt(name: &'static str, ty: Type, doc: &'static str) -> Column {
    Column { name, ty, nullable: true, doc }
}

const BUILD_ID: Column =
    opt("build_id", Type::U64, "the build it was part of; null outside any build");
const PROCESS: Column = col("process", Type::U32, "the process id");
const PROCESS_NAME: Column = col(
    "process_name",
    Type::Dict,
    "what the process is: blaze serve, blaze build, a build script's crate",
);
const THREAD: Column = col("thread", Type::U32, "the operating system's thread id");
const THREAD_NAME: Column = col("thread_name", Type::Dict, "the thread's name");

/// One row a span: from when it was made to when it closed, on the thread that made it.
pub const SPANS: Table = Table {
    name: "spans",
    doc: "one row a span, from its start to its close, on the thread that made it",
    columns: &[
        BUILD_ID,
        opt("commit", Type::Dict, "the commit of the workspace the build was of"),
        PROCESS,
        PROCESS_NAME,
        THREAD,
        THREAD_NAME,
        col("span_id", Type::U64, "unique in its process while the process lives"),
        opt("parent", Type::U64, "the span it is inside; null at a root"),
        col("depth", Type::U16, "how many spans its thread was inside when it began"),
        col("name", Type::Dict, "the span's name"),
        col(
            "target",
            Type::Dict,
            "its tracing target: the module path it was made in, unless it names one (`blaze` for blaze's steps)",
        ),
        col("start_ns", Type::I64, "when it began"),
        col("dur_ns", Type::I64, "how long it was open"),
        opt(
            "self_cpu_ns",
            Type::I64,
            "its thread's CPU time less that of the spans under it; only while BLAZE_PROFILE is on",
        ),
        opt("alloc_bytes", Type::U64, "bytes its thread allocated while it was open"),
        opt("allocs", Type::U64, "allocations its thread made while it was open"),
        opt("query_kind", Type::Dict, "the query it ran, for a span of a query"),
        opt("crate", Type::Dict, "the crate it was of, from its field `crate`"),
        opt(
            "function",
            Type::Utf8,
            "the function it was of, from its field `function` or `subject`",
        ),
        opt("unit", Type::Utf8, "the codegen unit it was of, from its field `unit`"),
        // Beside `alloc_bytes` in meaning; at the end so adding them keeps `VERSION`.
        opt(
            "frame_grow_copies",
            Type::U64,
            "blocks its thread's task frames moved to grow them while it was open (a block grown while not its frame's last is copied whole); null when not counted or when it closed on another thread",
        ),
        opt("frame_copied_bytes", Type::U64, "bytes those moves copied"),
    ],
};

/// A value over time: queue depths, memo counts, bytes.
pub const COUNTERS: Table = Table {
    name: "counters",
    doc: "one row a value of a counter at an instant",
    columns: &[
        BUILD_ID,
        PROCESS,
        col("name", Type::Dict, "what is counted"),
        col("t_ns", Type::I64, "when"),
        col("value", Type::F64, "the value then"),
        opt("fiber", Type::U32, "the fiber a fiber stack counter is of; null for any other counter"),
        opt("job", Type::U64, "the job that ran on that fiber; null for any other counter"),
    ],
};

/// One row a switch of a job on a fiber: started, parked (waiting, or moving to another core),
/// resumed and ended, on the thread it happened on. A wait is a job's `parked` row and its next
/// `resumed` one; when their threads differ, the job moved.
pub const SWITCHES: Table = Table {
    name: "switches",
    doc: "one row a switch of a job on a fiber, on the thread it happened on",
    columns: &[
        BUILD_ID,
        PROCESS,
        THREAD,
        THREAD_NAME,
        col("t_ns", Type::I64, "when"),
        col("event", Type::Dict, "started, parked, resumed or ended"),
        col(
            "job",
            Type::U64,
            "the job: its slot's generation << 32 | its slot, one job of its driver's life",
        ),
        col("fiber", Type::U32, "the fiber it runs on, numbered in its process"),
        col("core", Type::U32, "the driver's core of the thread"),
        opt(
            "wait_kind",
            Type::Dict,
            "for parked: what it waits for (job, key, counter, until) or moved; null otherwise",
        ),
        opt(
            "wait_value",
            Type::U64,
            "the job's slot, the key's hash, the counter or the driver-clock deadline; null for moved",
        ),
    ],
};

/// One row a build request.
pub const BUILDS: Table = Table {
    name: "builds",
    doc: "one row a build request the daemon ran",
    columns: &[
        col("build_id", Type::U64, "the build's number, unique in the daemon's store"),
        opt("commit", Type::Utf8, "the workspace's commit, when it is a git checkout"),
        col("workspace", Type::Utf8, "the workspace's root"),
        col("command", Type::Utf8, "what was asked: build, test, run, check, with the arguments"),
        col("started_ns", Type::I64, "when the daemon took it"),
        opt("dur_ns", Type::I64, "how long it ran; null while it runs"),
        opt("started_unix_ns", Type::I64, "when it was taken, on the wall clock"),
        col("outcome", Type::Dict, "ok, failed, cancelled, stalled or running"),
        opt("hits", Type::U64, "nodes the memo settled without running them"),
        opt("misses", Type::U64, "nodes that ran their task"),
        opt("joined", Type::U64, "nodes that took a task another node ran"),
    ],
};

/// The CPU sampler's samples: where each running thread was, 49 times a second.
pub const SAMPLES: Table = Table {
    name: "samples",
    doc: "one row a CPU sample of a running thread",
    columns: &[
        BUILD_ID,
        PROCESS,
        THREAD,
        col("t_ns", Type::I64, "when it was taken"),
        col("stack", Type::Stack, "the call stack, outermost first, each frame demangled"),
        opt("span_id", Type::U64, "the innermost span its thread was inside"),
    ],
};

/// Each query of a build that was not a plain memo hit, and why.
pub const PROVENANCE: Table = Table {
    name: "provenance",
    doc: "one row a query of a build that was verified or rerun, with the cause",
    columns: &[
        col("build_id", Type::U64, "the build"),
        col("query_key", Type::Utf8, "the query and its key"),
        col("kind", Type::Dict, "the query's kind"),
        col("outcome", Type::Dict, "verified, reran, or reran-same-output (an early cutoff)"),
        opt("cause", Type::Utf8, "the input or query key whose change made it rerun"),
        col("dur_ns", Type::I64, "how long verifying or running it took"),
        opt("unit", Type::Utf8, "the codegen unit it was for"),
        opt("crate", Type::Dict, "the crate it was of"),
    ],
};

/// What changed for a build, and how blaze saw it.
pub const INPUTS: Table = Table {
    name: "inputs",
    doc: "one row an input that changed for a build",
    columns: &[
        col("build_id", Type::U64, "the build"),
        col("key", Type::Utf8, "the input: a path, a variable's name, a flag, a dependency"),
        col("kind", Type::Dict, "file, env, flag or dependency"),
        opt("detail", Type::Utf8, "what changed about it"),
        col("seen_by", Type::Dict, "watcher, rescan or request"),
    ],
};

/// One allocation in about every 512 KiB, with its stack.
pub const ALLOC_SAMPLES: Table = Table {
    name: "alloc_samples",
    doc: "one row a sampled allocation: about one every 512 KiB allocated, on each thread",
    columns: &[
        BUILD_ID,
        PROCESS,
        THREAD,
        col("t_ns", Type::I64, "when"),
        col("bytes", Type::U64, "the bytes the sample stands for (the sampling interval)"),
        col("stack", Type::Stack, "the call stack, outermost first"),
        opt("span_id", Type::U64, "the innermost span its thread was inside"),
    ],
};

/// The heap at the end of a build, by what holds it.
pub const HEAP_BY_OWNER: Table = Table {
    name: "heap_by_owner",
    doc: "one row an owner of live heap at the end of a build: a memo table per query kind, an arena",
    columns: &[
        col("build_id", Type::U64, "the build"),
        col("owner", Type::Dict, "what holds the bytes"),
        col("bytes", Type::U64, "live bytes"),
    ],
};

/// What a `tiles` request answers: the spans of a range, aggregated per pixel-wide bucket.
pub const TILES: Table = Table {
    name: "tiles",
    doc: "spans of a time range per (process, thread, depth, bucket): what the explorer draws zoomed out",
    columns: &[
        PROCESS,
        THREAD,
        col("depth", Type::U16, "the spans' depth"),
        col("t0_ns", Type::I64, "the bucket's start"),
        col("t1_ns", Type::I64, "the bucket's end"),
        col("count", Type::U32, "spans overlapping it"),
        col("busy_ns", Type::I64, "of the bucket, the time inside any of them"),
        col("name", Type::Dict, "the longest one's name"),
    ],
};

/// The tiles of the jobs' lanes: `tiles` with a lane per fiber job in place of a lane per
/// thread, so a job that waits shows its wait as a gap and a job that moved between threads
/// is one lane. A result, not stored: the explorer's `tiles` op with `"lane": "job"` makes it
/// from the `job_spans` view.
pub const JOB_TILES: Table = Table {
    name: "job_tiles",
    doc: "spans of a time range per (process, job, depth, bucket): the explorer's lane per fiber job",
    columns: &[
        PROCESS,
        col("job", Type::U64, "the job, as `switches` numbers it"),
        col("depth", Type::U16, "the spans' depth"),
        col("t0_ns", Type::I64, "the bucket's start"),
        col("t1_ns", Type::I64, "the bucket's end"),
        col("count", Type::U32, "spans overlapping it"),
        col("busy_ns", Type::I64, "of the bucket, the time inside any of them"),
        col("name", Type::Dict, "the longest one's name"),
    ],
};

/// The spans each thread is inside now, not yet rows of `spans`: what the live feed sends
/// beside the rows that closed.
pub const OPEN: Table = Table {
    name: "open",
    doc: "one row a span a live thread is inside now, outermost first",
    columns: &[
        PROCESS,
        THREAD,
        THREAD_NAME,
        col("depth", Type::U16, "its place on its thread's stack, 0 outermost"),
        col("name", Type::Dict, "the span's name"),
        col("target", Type::Dict, "its tracing target, as in `spans`"),
        col("start_ns", Type::I64, "when it began"),
    ],
};

/// Every table the history and the live feed hold.
pub const TABLES: [&Table; 9] =
    [&SPANS, &COUNTERS, &BUILDS, &SAMPLES, &PROVENANCE, &INPUTS, &ALLOC_SAMPLES, &HEAP_BY_OWNER, &SWITCHES];

/// The tables that are results, made on request and not kept.
pub const RESULTS: [&Table; 3] = [&TILES, &JOB_TILES, &OPEN];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_names_each_column_once() {
        for table in TABLES.iter().chain(&RESULTS).copied() {
            for (i, it) in table.columns.iter().enumerate() {
                assert_eq!(
                    table.column(it.name),
                    Some(i),
                    "{}.{} is named twice",
                    table.name,
                    it.name
                );
                assert!(!it.doc.is_empty(), "{}.{} says what it is", table.name, it.name);
            }
        }
    }

    #[test]
    fn the_explorers_columns_are_there() {
        let spans = [
            "build_id", "process", "thread", "span_id", "parent", "depth", "name", "start_ns",
            "dur_ns",
        ];
        assert!(spans.iter().all(|it| SPANS.column(it).is_some()));
        let counters = ["build_id", "process", "name", "t_ns", "value"];
        assert!(counters.iter().all(|it| COUNTERS.column(it).is_some()));
    }
}

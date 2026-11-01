-- Named queries over the views of views.sql: what the explorer's `views` op lists and
-- `blaze explain <view> <args>` runs. Each starts at a `-- name:` line, then a `-- says:` line,
-- then its SQL. `$<arg>` is an argument, declared with its type and default in
-- crates/blaze-profile/src/views.rs and bound there as an SQL literal. `$build` is the latest
-- build when not given.

-- name: slowest_queries
-- says: the slowest query runs of a build, with what the memo did and why
SELECT query_kind, crate, function, dur_ns / 1e6 AS ms, alloc_bytes, memo, cause
FROM queries
WHERE build_id = $build
ORDER BY dur_ns DESC
LIMIT $limit

-- name: slower_between_commits
-- says: the spans whose median time grew most from the commit `earlier` to the commit `later` (prefixes of either)
WITH runs AS (
    SELECT builds.commit AS commit, spans.name AS name, spans.dur_ns AS dur_ns
    FROM spans
    JOIN builds ON builds.build_id = spans.build_id
), earlier AS (
    SELECT name, median(dur_ns) AS dur_ns, count(*) AS runs
    FROM runs
    WHERE starts_with(commit, $earlier)
    GROUP BY name
), later AS (
    SELECT name, median(dur_ns) AS dur_ns, count(*) AS runs
    FROM runs
    WHERE starts_with(commit, $later)
    GROUP BY name
)
SELECT
    later.name AS name,
    earlier.dur_ns / 1e6 AS earlier_ms,
    later.dur_ns / 1e6 AS later_ms,
    (later.dur_ns - earlier.dur_ns) / 1e6 AS slower_ms,
    earlier.runs AS earlier_runs,
    later.runs AS later_runs
FROM later
JOIN earlier ON earlier.name = later.name
ORDER BY slower_ms DESC
LIMIT $limit

-- name: memo_hit_rate_per_crate
-- says: of the queries a build verified or reran, per crate, how many the memo kept (verified, or reran to the same output)
SELECT
    crate,
    count(*) FILTER (WHERE outcome = 'verified') AS verified,
    count(*) FILTER (WHERE outcome = 'reran-same-output') AS same_output,
    count(*) FILTER (WHERE outcome = 'reran') AS reran,
    round(count(*) FILTER (WHERE outcome <> 'reran') / count(*), 3) AS hit_rate
FROM provenance
WHERE build_id = $build
GROUP BY crate
ORDER BY reran DESC

-- name: critical_path
-- says: the longest spans on a build's critical path: from the root span that ended last, down through the child each span waited for last
WITH RECURSIVE build_spans AS (
    SELECT process, span_id, parent, depth, name, crate, function, start_ns, dur_ns,
        start_ns + dur_ns AS end_ns
    FROM spans
    WHERE build_id = $build
), last_child AS (
    SELECT process, parent, arg_max(span_id, end_ns) AS span_id
    FROM build_spans
    WHERE parent IS NOT NULL
    GROUP BY process, parent
), root AS (
    SELECT process, span_id
    FROM build_spans
    WHERE parent IS NULL
    ORDER BY end_ns DESC
    LIMIT 1
), chain AS (
    SELECT process, span_id FROM root
    UNION ALL
    SELECT last_child.process AS process, last_child.span_id AS span_id
    FROM chain
    JOIN last_child ON last_child.process = chain.process AND last_child.parent = chain.span_id
)
SELECT build_spans.name AS name, build_spans.depth AS depth, build_spans.crate AS crate,
    build_spans.function AS function, build_spans.dur_ns / 1e6 AS ms, build_spans.start_ns AS start_ns
FROM chain
JOIN build_spans ON build_spans.process = chain.process AND build_spans.span_id = chain.span_id
ORDER BY build_spans.dur_ns DESC
LIMIT $limit

-- name: allocations_per_query_kind
-- says: a build's allocations per query kind: bytes and allocations made while its spans were open (the queries under them too)
SELECT
    query_kind,
    count(*) AS runs,
    sum(alloc_bytes) AS alloc_bytes,
    sum(allocs) AS allocs,
    round(sum(alloc_bytes) / count(*)) AS bytes_per_run
FROM spans
WHERE build_id = $build AND query_kind IS NOT NULL
GROUP BY query_kind
ORDER BY alloc_bytes DESC NULLS LAST

-- name: what_reran
-- says: the queries a build reran, slowest first, with the input or query whose change made each rerun
SELECT kind, query_key, outcome, cause, dur_ns / 1e6 AS ms, crate, unit
FROM provenance
WHERE build_id = $build AND outcome <> 'verified'
ORDER BY dur_ns DESC
LIMIT $limit

-- name: busiest_threads
-- says: the threads that spent most of a build inside spans, by the time of their outermost spans
SELECT
    process,
    thread,
    any_value(thread_name) AS thread_name,
    sum(dur_ns) / 1e6 AS busy_ms,
    count(*) AS spans
FROM spans
WHERE build_id = $build AND depth = 0
GROUP BY process, thread
ORDER BY busy_ms DESC
LIMIT $limit

-- name: longest_gaps
-- says: the longest stretches of a build in which no span closed: stalls too short for the watchdog
WITH ends AS (
    SELECT start_ns + dur_ns AS end_ns
    FROM spans
    WHERE build_id = $build
), gaps AS (
    SELECT lag(end_ns) OVER (ORDER BY end_ns) AS from_ns, end_ns AS to_ns
    FROM ends
)
SELECT from_ns, to_ns, (to_ns - from_ns) / 1e6 AS gap_ms
FROM gaps
WHERE from_ns IS NOT NULL
ORDER BY gap_ms DESC
LIMIT $limit

-- name: top_cpu_stacks
-- says: the call stacks a build's CPU samples found most often, with the CPU seconds each stands for at 49 samples a second
SELECT
    array_to_string(stack, ' > ') AS frames,
    count(*) AS taken,
    round(count(*) / 49.0, 2) AS cpu_s
FROM samples
WHERE build_id = $build
GROUP BY stack
ORDER BY taken DESC
LIMIT $limit

-- name: time_by_span_name
-- says: a build's time per span name: how many, their whole time, their own CPU time and the longest
SELECT
    name,
    count(*) AS spans,
    sum(dur_ns) / 1e6 AS total_ms,
    sum(self_cpu_ns) / 1e6 AS self_cpu_ms,
    max(dur_ns) / 1e6 AS longest_ms
FROM spans
WHERE build_id = $build
GROUP BY name
ORDER BY total_ms DESC

-- name: jobs_by_stack_high_water
-- says: a build's jobs ranked by how deep they went on their fiber's stack and past it, in KiB, beside what the fiber reserved
SELECT
    rank() OVER (ORDER BY high_water DESC) AS rank,
    process,
    fiber,
    job,
    ended_ns,
    high_water / 1024 AS high_water_kib,
    deepest / 1024 AS deepest_kib,
    grown / 1024 AS grown_kib,
    touched / 1024 AS touched_kib,
    reserved / 1024 AS reserved_kib,
    round(100 * deepest / reserved, 1) AS reserved_used_pct
FROM fiber_stacks
WHERE build_id = $build
ORDER BY high_water DESC
LIMIT $limit

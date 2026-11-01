-- The DuckDB views over blaze's Parquet history (crates/blaze-profile/src/history.rs): one a
-- table of crates/blaze-profile/src/schema.rs, and `queries` and `passes` over `spans`.
-- `blaze debug sql` runs this before each query, with every `@<table>` replaced by the list of
-- globs of that table's files, `['<dir>/*.<table>.parquet', ...]`: the history's directory and,
-- with `--live`, the recorder's last seconds beside it. Each directory holds an empty
-- `schema.<table>.parquet` of every table, so each glob matches a file with every column.

CREATE OR REPLACE VIEW spans AS
SELECT * FROM read_parquet(@spans, union_by_name = true);

CREATE OR REPLACE VIEW counters AS
SELECT * FROM read_parquet(@counters, union_by_name = true);

CREATE OR REPLACE VIEW builds AS
SELECT * FROM read_parquet(@builds, union_by_name = true);

CREATE OR REPLACE VIEW samples AS
SELECT * FROM read_parquet(@samples, union_by_name = true);

CREATE OR REPLACE VIEW provenance AS
SELECT * FROM read_parquet(@provenance, union_by_name = true);

CREATE OR REPLACE VIEW inputs AS
SELECT * FROM read_parquet(@inputs, union_by_name = true);

CREATE OR REPLACE VIEW alloc_samples AS
SELECT * FROM read_parquet(@alloc_samples, union_by_name = true);

CREATE OR REPLACE VIEW heap_by_owner AS
SELECT * FROM read_parquet(@heap_by_owner, union_by_name = true);

-- One row a span of a query, with what the memo did with that query when provenance has it:
-- `memo` is verified, reran or reran-same-output, matched on the build, the query's kind and
-- its key (the span's `function` field); null where provenance has no row for it.
CREATE OR REPLACE VIEW queries AS
SELECT spans.*, memo.outcome AS memo, memo.cause AS cause
FROM spans
LEFT JOIN (
    SELECT build_id, kind, query_key, any_value(outcome) AS outcome, any_value(cause) AS cause
    FROM provenance
    GROUP BY build_id, kind, query_key
) AS memo
    ON memo.build_id = spans.build_id
    AND memo.kind = spans.query_kind
    AND memo.query_key = spans.function
WHERE spans.query_kind IS NOT NULL;

-- One row a step of the code generator on one function: a span that names its function and is
-- not a query.
CREATE OR REPLACE VIEW passes AS
SELECT
    build_id, commit, process, thread, crate, function, unit, name AS pass, start_ns, dur_ns,
    self_cpu_ns, alloc_bytes, allocs
FROM spans
WHERE function IS NOT NULL AND query_kind IS NULL;

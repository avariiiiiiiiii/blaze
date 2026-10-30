

## Reverse index for `name_of`/`names_of` (speed-hir-nameres-scope, for edge)

In a scope of more than 8 names, `ItemScope::by_def` (`OnceLock<Box<[u32]>>`, 4 bytes an entry)
is binary-searched instead of scanning the column. Like `sorted`, it is derived from the columns,
made once by the first read after a def changed, and cleared only through `&mut` where a def is
added or replaced. That is the write-once pattern `concurrency.md` allows, not a lock held on
reads. The answers and their order are unchanged; the model test checks `names_of`'s whole list.

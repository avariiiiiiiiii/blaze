//! blaze's proc-macro server, `blaze-proc-macro-srv` (docs/no-rustc.md, step J1): the protocol
//! of `proc-macro-api` over `proc-macro-srv`, which loads the libraries blaze links. blaze
//! starts it on stdin and stdout, or warm in a sandbox on the Unix socket its pool names
//! ([`main_loop::run_warm`]).

pub mod main_loop;

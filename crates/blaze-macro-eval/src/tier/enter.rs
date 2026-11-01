//! Expansions on a compiled image (macros-tier.md, "Running"), as `run.rs`'s `evaluate`
//! runs them on the evaluator: for each, the macro's client made by its own client function,
//! its input put in a buffer made by the bridge's own `Buffer::new` and `reserve`, and the
//! client's `run` entered with a `BridgeConfig` whose dispatch is the claimed
//! `Closure::call`, answered here by the expansion's server ([`Answer`]). The macro's code
//! runs on the calling thread's stack, its memory the region's.
//!
//! A value the host passes is in 16-byte slots: a word as itself, a value of more words at a
//! copy in a frame taken from the shadow stack for the call, given by its guest address (a
//! pointer) or by the confined host address of the copy (a struct argument, which the call
//! copies on), and a result of more words written by the callee to a place in that frame.

use base_db::{Db, Env};
use cranelift_codegen::ir;
use hir_def::StaticId;

use super::{
    build::{BridgeAbi, Compiled},
    confine::{BAD_FN_PTR, BAD_SIGNATURE},
    machine::{self, Escape, Escaped, Running},
    memory::Memory,
    region::{Bounds, Layout, cell},
    shims::{Bridge, Shims},
    table::Sig,
};
use crate::{
    guest::{Guest, Stop},
    os::Os,
    run::{Expansion, Ran},
    server::Server,
};

/// The bridge's `Buffer`: `data`, `len`, `capacity`, `reserve`, `drop`, a word each.
const BUFFER: usize = 40;
/// The bridge's `BridgeConfig`: the input `Buffer`, the dispatch `Closure` (`call`, `env`)
/// and `force_show_panics`, padded to a word.
const CONFIG: usize = 64;
/// The shadow stack's alignment, the frames' the host takes from it.
const ALIGN: u64 = 16;

/// A value the host passes.
#[derive(Clone, Copy, Debug)]
enum Arg<'a> {
    Word(u64),
    /// A value of more words, copied into the call's frame.
    Value(&'a [u8]),
}

fn word(bytes: &[u8], n: usize) -> u64 {
    u64::from_le_bytes(bytes[n * 8..n * 8 + 8].try_into().expect("a word is 8 bytes"))
}

fn set_word(bytes: &mut [u8], n: usize, value: u64) {
    bytes[n * 8..n * 8 + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_word(memory: &Memory, at: u64) -> Result<u64, Stop> {
    Ok(word(memory.read(at, 8)?, 0))
}

/// Why the entry that returned `false` escaped.
fn escaped(running: &Running<'_>) -> Stop {
    if let Some(fault) = running.take_fault() {
        let name = running.image().function_at(fault.pc).map_or("its code", |(_, name)| name);
        let what = match fault.trap {
            Some(BAD_FN_PTR) => "a call through a function pointer that is no function".to_owned(), // alloc: error path
            Some(BAD_SIGNATURE) => "a call through a function pointer of another signature".to_owned(), // alloc: error path
            Some(ir::TrapCode::HEAP_OUT_OF_BOUNDS) => "an access outside its memory".to_owned(), // alloc: error path
            Some(ir::TrapCode::STACK_OVERFLOW) => "an overflow of its stack".to_owned(), // alloc: error path
            Some(code) => format!("the trap {code}"), // alloc: error path
            None => format!("signal {}", fault.signal), // alloc: error path
        };
        // alloc: error path
        return Stop(format!("stopped at {what} in `{name}` (compiled tier)"));
    }
    match running.take_escaped() {
        Some(Escaped::Stop(stop)) => stop,
        // alloc: error path
        Some(Escaped::Uncaught(at)) => Stop(format!("panicked outside the bridge's catch (its exception at {at:#x})")),
        None => Stop("left its code by no escape blaze knows".to_owned()), // alloc: error path
    }
}

/// Calls `code`, of the table signature `sig`, with `args`, writing its result's bytes to
/// `out`: through its entry thunk, its values in a frame of the shadow stack, which is given
/// back when it returns.
fn call(
    compiled: &Compiled,
    running: &Running<'_>,
    shims: &mut Shims<'_, '_>,
    code: u64,
    sig: Sig,
    args: &[Arg<'_>],
    out: &mut [u8],
) -> Result<(), Stop> {
    let Some(signature) = compiled.signature(sig) else {
        return Err(Stop(format!("entered code of signature {}, which its image has none of", sig.0))); // alloc: error path
    };
    let Some(thunk) = compiled.thunk(sig) else {
        return Err(Stop(format!("entered code of signature {signature}, which no entry thunk was made for"))); // alloc: error path
    };
    let params = &signature.params[..signature.params.len().saturating_sub(1)];
    let sret = params.iter().any(|it| it.purpose == ir::ArgumentPurpose::StructReturn);
    let given = params.iter().filter(|it| it.purpose != ir::ArgumentPurpose::StructReturn).count();
    if given != args.len() || params.len() > 8 || signature.returns.len() > 2 {
        // alloc: error path
        return Err(Stop(format!("entered code of signature {signature} with {} values", args.len())));
    }

    // The frame: the result's place, then each value's copy.
    let round = |len: usize| (len as u64).next_multiple_of(ALIGN);
    let copies: u64 = args
        .iter()
        .map(|it| match it {
            Arg::Word(_) => 0,
            Arg::Value(bytes) => round(bytes.len()),
        })
        .sum();
    let size = (if sret { round(out.len()) } else { 0 }) + copies;
    let sp_cell = u64::from(Layout::HEADER + cell::SHADOW_SP);
    let memory = shims.memory();
    let sp = read_word(memory, sp_cell)?;
    let limit = read_word(memory, u64::from(Layout::HEADER + cell::SHADOW_LIMIT))?;
    let frame = sp.checked_sub(size).map(|it| it & !(ALIGN - 1)).filter(|&it| it >= limit);
    let Some(frame) = frame else {
        return Err(Stop("overflowed its stack (the compiled tier's shadow stack)".to_owned())); // alloc: error path
    };
    memory.write(sp_cell, &frame.to_le_bytes())?;
    let base = memory.region().base();

    let mut slots = [[0u8; 16]; 8];
    let mut place = frame;
    let ret_at = place;
    if sret {
        place += round(out.len());
    }
    let mut next = args.iter();
    for (param, slot) in params.iter().zip(&mut slots) {
        let value = match param.purpose {
            ir::ArgumentPurpose::StructReturn => ret_at,
            purpose => match next.next() {
                Some(Arg::Word(word)) => *word,
                Some(Arg::Value(bytes)) => {
                    let at = place;
                    memory.write(at, bytes)?;
                    place += round(bytes.len());
                    match purpose {
                        ir::ArgumentPurpose::StructArgument(_) => base + at,
                        _ => at,
                    }
                }
                None => unreachable!("the values were counted against the parameters"),
            },
        };
        slot[..8].copy_from_slice(&value.to_le_bytes());
    }
    let mut rets = [[0u8; 16]; 2];
    let returned = running.enter(shims, thunk, code, base, &slots[..params.len()], &mut rets[..signature.returns.len().max(1)]);
    let memory = shims.memory();
    let result = if !returned {
        Err(escaped(running))
    } else if sret {
        memory.read(ret_at, out.len()).map(|bytes| out.copy_from_slice(bytes))
    } else if out.len() <= 16 {
        out.copy_from_slice(&rets[0][..out.len()]);
        Ok(())
    } else {
        // alloc: error path
        Err(Stop(format!("returned {} bytes in registers from code of signature {signature}", out.len())))
    };
    memory.write(sp_cell, &sp.to_le_bytes())?;
    result
}

/// Calls the function at the table pointer `pointer` as [`call`] does.
fn call_pointer(
    compiled: &Compiled,
    running: &Running<'_>,
    shims: &mut Shims<'_, '_>,
    pointer: u64,
    args: &[Arg<'_>],
    out: &mut [u8],
) -> Result<(), Stop> {
    let Some(entry) = compiled.image.table().entry(pointer) else {
        return Err(Stop(format!("gave {pointer:#x} as a function pointer, which is no function of its image"))); // alloc: error path
    };
    let (code, sig) = (entry.code, entry.sig);
    call(compiled, running, shims, code, sig, args, out)
}

/// The buffer `buffer` made to hold `bytes` and nothing else: grown by its own `reserve`
/// when it is too small, as the client's `extend_from_slice` grows it.
fn fill(
    compiled: &Compiled,
    running: &Running<'_>,
    shims: &mut Shims<'_, '_>,
    buffer: &mut [u8; BUFFER],
    bytes: &[u8],
) -> Result<(), Stop> {
    set_word(buffer, 1, 0);
    if (word(buffer, 2) as usize) < bytes.len() {
        let reserve = word(buffer, 3);
        let given = *buffer;
        call_pointer(compiled, running, shims, reserve, &[Arg::Value(&given), Arg::Word(bytes.len() as u64)], buffer)?;
    }
    shims.memory().write(word(buffer, 0), bytes)?;
    set_word(buffer, 1, bytes.len() as u64);
    Ok(())
}

/// The run's half of the bridge: the claimed `Closure::call`, each request the client makes
/// answered in its buffer by the expansion's server.
struct Answer<'r> {
    db: &'r Db,
    compiled: &'r Compiled,
    abi: Option<BridgeAbi>,
    server: Server,
    /// The replies' buffer, reused.
    reply: Vec<u8>,
}

impl Bridge for Answer<'_> {
    fn call(&mut self, shims: &mut Shims<'_, '_>, running: &Running<'_>, slots: &mut [[u8; 16]]) -> Result<(), Escape> {
        let Some(abi) = self.abi else {
            return Err(Stop("called the bridge's dispatch, which its build never bound".to_owned()).into()); // alloc: error path
        };
        let at = |slot: usize| slots.get(slot).map(|it| word(&it[..8], 0));
        let (Some(ret), Some(buffer_at)) = (at(abi.ret), at(abi.buffer)) else {
            return Err(Stop("called the bridge's dispatch with too few values".to_owned()).into()); // alloc: error path
        };
        let mut buffer = [0u8; BUFFER];
        buffer.copy_from_slice(shims.memory().read(buffer_at, BUFFER)?);
        let (data, len) = (word(&buffer, 0), word(&buffer, 1));
        let len = usize::try_from(len).map_err(|_| Stop("sent a request longer than memory".to_owned()))?; // alloc: error path
        let mut reply = std::mem::take(&mut self.reply);
        let answered = {
            let request = shims.memory().read(data, len)?;
            self.server.dispatch(self.db, request, &mut reply)
        };
        let filled = answered.and_then(|()| fill(self.compiled, running, shims, &mut buffer, &reply));
        reply.clear();
        self.reply = reply;
        filled?;
        shims.memory().write(ret, &buffer)?;
        slots[0] = [0; 16];
        slots[0][..8].copy_from_slice(&ret.to_le_bytes());
        Ok(())
    }
}

/// Whether the static's bytes may hold an address of the region: any word that is one of its
/// image's, stack's or heap's offsets. A session whose statics hold one runs its calls again
/// rather than carrying the bytes over.
fn holds_address(bytes: &[u8], layout: &Layout) -> bool {
    bytes.chunks_exact(8).any(|it| {
        let at = word(it, 0);
        (u64::from(Layout::HEADER)..u64::from(layout.heap.end)).contains(&at)
    })
}

/// Runs `expansions` in order on `compiled`, each with its own server, starting from the
/// statics `preset`: what the last gave, and what all of them did.
pub(crate) fn run(
    db: &Db,
    compiled: &Compiled,
    env: &Env,
    preset: &[(StaticId, Vec<u8>)],
    expansions: &[Expansion<'_>],
) -> Result<Ran, Stop> {
    let Some(last) = expansions.last() else {
        return Err(Stop("was given nothing to expand".to_owned())); // alloc: error path
    };
    let image = &compiled.image;
    let mut memory = Memory::new(&image.writable, &image.readonly, Bounds::DEFAULT)?;
    if memory.region().layout() != &image.layout {
        return Err(Stop("laid out its region apart from its image".to_owned())); // alloc: error path
    }
    for (id, bytes) in preset {
        let Some(&(_, blob, len)) = compiled.statics.iter().find(|it| it.0 == *id) else {
            // alloc: error path
            return Err(Stop(format!("was given the bytes of a static ({id:?}) its build did not reach")));
        };
        if bytes.len() != len as usize {
            // alloc: error path
            return Err(Stop(format!("was given {} bytes of a static of {len}", bytes.len())));
        }
        let at = image.blob(blob).expect("every static is a blob of its image");
        memory.write(u64::from(at), bytes)?;
    }
    let running = Running::new(image, machine::current_stack()?, compiled.resume)?;
    let mut os = Os::new(env);
    let mut answer = Answer {
        db,
        compiled,
        abi: compiled.bridge,
        server: Server::new(last.sites, last.edition, last.forced_keywords),
        reply: Vec::new(), // alloc: none, empty until the first reply
    };
    // alloc: once per run, the input's and the output's bytes
    let mut input = Vec::new();
    let mut output_bytes = Vec::new(); // alloc: as above
    let mut output = Ok(crate::stream::Stream::default());
    for expansion in expansions {
        answer.server = Server::new(expansion.sites, expansion.edition, expansion.forced_keywords);
        input.clear();
        answer.server.input(expansion.inputs, &mut input);
        let Some(&(_, client_fn)) = compiled.clients.iter().find(|it| it.0 == expansion.function) else {
            return Err(Stop("expanded a function that is no macro of its crate".to_owned())); // alloc: error path
        };
        let stubs = &compiled.stubs;
        let mut shims = Shims::new(stubs, &mut memory, &mut os, &mut answer);
        let entered = |n: u32| {
            let code = image.code(n).expect("every function is placed");
            let (sig, _) = compiled.signature_of(n).expect("every compiled function has a signature");
            (code, sig)
        };
        let mut client = [0u8; 8];
        let (code, sig) = entered(client_fn);
        call(compiled, &running, &mut shims, code, sig, &[], &mut client)?;
        let mut buffer = [0u8; BUFFER];
        let (code, sig) = entered(compiled.buffer_new);
        call(compiled, &running, &mut shims, code, sig, &[], &mut buffer)?;
        fill(compiled, &running, &mut shims, &mut buffer, &input)?;
        let mut config = [0u8; CONFIG];
        config[..BUFFER].copy_from_slice(&buffer);
        let mut returned = [0u8; BUFFER];
        call_pointer(compiled, &running, &mut shims, word(&client, 0), &[Arg::Value(&config)], &mut returned)?;
        let (data, len) = (word(&returned, 0), word(&returned, 1));
        let len = usize::try_from(len).map_err(|_| Stop("gave an output longer than memory".to_owned()))?; // alloc: error path
        output_bytes.clear();
        output_bytes.extend_from_slice(shims.memory().read(data, len)?);
        drop(shims);
        output = answer.server.output(&output_bytes)?;
    }

    // The statics outside the standard library that the runs changed.
    let mut statics = Vec::new(); // alloc: once per run, the statics it changed
    let layout = &image.layout;
    for &(id, blob, len) in &compiled.statics {
        let at = image.blob(blob).expect("every static is a blob of its image");
        if !layout.writable.contains(&at) || crate::run::of_sysroot(db, id) {
            continue;
        }
        let start = (at - layout.writable.start) as usize;
        let before = &image.writable[start..start + len as usize];
        let now = memory.read(u64::from(at), len as usize)?;
        if now != before {
            statics.push((id, now.to_vec(), holds_address(now, layout))); // alloc: once per static changed
        }
    }
    let server = answer.server;
    // alloc: once per run, what it read, given to the key
    let seen = os.seen().map(|(seen, what)| (seen, what.to_owned())).collect();
    Ok(Ran {
        output,
        asked: server.asked,
        tracked: server.tracked,
        emitted: server.emitted,
        seen,
        unkeyable: os.unkeyable().map(str::to_owned),
        statics,
    })
}

#[cfg(test)]
mod tests;

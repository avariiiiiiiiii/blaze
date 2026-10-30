//! The tables of `__LINKEDIT` dyld reads (`<mach-o/loader.h>`): the opcodes that say which
//! addresses it slides (rebase) and which it binds to a symbol of a library, the trie of what
//! the image exports, and the starts of its functions.

use object::macho;

/// A segment, by its index among the image's `LC_SEGMENT_64`s.
pub(super) struct Span {
    pub name: &'static str,
    pub start: u64,
    pub end: u64,
    pub writable: bool,
}

/// An address dyld binds to `name` of the library `ordinal` (or a `BIND_SPECIAL_DYLIB_*`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Bind {
    pub addr: u64,
    pub ordinal: i64,
    /// As Mach-O names it, with its underscore.
    pub name: Vec<u8>,
    pub weak: bool,
    pub addend: i64,
}

pub(super) fn uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

pub(super) fn sleb(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
        if done {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn uleb_len(value: u64) -> u64 {
    u64::from((64 - value.leading_zeros()).max(1).div_ceil(7))
}

/// The segment `addr` is in, by index; an error when it is in none or dyld can't write it.
fn segment_of(spans: &[Span], addr: u64, what: &str) -> Result<usize, String> {
    let nth = (spans.iter())
        .position(|it| (it.start..it.end).contains(&addr))
        .ok_or_else(|| format!("{what} at {addr:#x} is in no segment"))?;
    if !spans[nth].writable {
        return Err(format!(
            "{what} at {addr:#x} is in {}, which the loader can't write: a pointer there would \
             need a text relocation",
            spans[nth].name
        ));
    }
    if nth > 15 {
        return Err(format!("{what} at {addr:#x} is in segment {nth}, past the 16 opcodes name"));
    }
    Ok(nth)
}

/// The rebase opcodes of `addrs`, each a pointer dyld adds the slide to.
pub(super) fn rebases(addrs: &[u64], spans: &[Span]) -> Result<Vec<u8>, String> {
    let mut addrs = addrs.to_vec();
    addrs.sort_unstable();
    if let Some(twice) = addrs.windows(2).find(|it| it[0] == it[1]) {
        return Err(format!("the address {:#x} is to be slid twice", twice[0]));
    }
    if addrs.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = vec![macho::REBASE_OPCODE_SET_TYPE_IMM | macho::REBASE_TYPE_POINTER];
    // Where dyld's cursor is: its segment and address.
    let mut cursor: Option<(usize, u64)> = None;
    let mut rest = &addrs[..];
    while let Some(&first) = rest.first() {
        let segment = segment_of(spans, first, "a pointer to slide")?;
        let end = spans[segment].end;
        let run = (rest.iter().zip(0u64..))
            .take_while(|(it, nth)| **it == first + nth * 8 && **it + 8 <= end)
            .count()
            .max(1);
        match cursor {
            Some((at, address)) if at == segment && first >= address => {
                if first > address {
                    out.push(macho::REBASE_OPCODE_ADD_ADDR_ULEB);
                    uleb(&mut out, first - address);
                }
            }
            _ => {
                out.push(macho::REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB | segment as u8);
                uleb(&mut out, first - spans[segment].start);
            }
        }
        if run < 16 {
            out.push(macho::REBASE_OPCODE_DO_REBASE_IMM_TIMES | run as u8);
        } else {
            out.push(macho::REBASE_OPCODE_DO_REBASE_ULEB_TIMES);
            uleb(&mut out, run as u64);
        }
        cursor = Some((segment, first + run as u64 * 8));
        rest = &rest[run..];
    }
    out.push(macho::REBASE_OPCODE_DONE);
    out.resize(out.len().next_multiple_of(8), 0);
    Ok(out)
}

/// The bind opcodes of `binds`, sorted here by symbol so each symbol is named once.
pub(super) fn binds(binds: &mut [Bind], spans: &[Span]) -> Result<Vec<u8>, String> {
    if binds.is_empty() {
        return Ok(Vec::new());
    }
    let mut addrs: Vec<u64> = binds.iter().map(|it| it.addr).collect();
    addrs.sort_unstable();
    if let Some(twice) = addrs.windows(2).find(|it| it[0] == it[1]) {
        return Err(format!("the address {:#x} is to be bound twice", twice[0]));
    }
    binds.sort_unstable_by(|a, b| {
        (a.ordinal, &a.name, a.weak, a.addend, a.addr).cmp(&(
            b.ordinal, &b.name, b.weak, b.addend, b.addr,
        ))
    });
    let mut out = vec![macho::BIND_OPCODE_SET_TYPE_IMM | macho::BIND_TYPE_POINTER];
    let (mut ordinal, mut symbol, mut addend) = (None, None::<(&[u8], bool)>, 0i64);
    let mut cursor: Option<(usize, u64)> = None;
    for bind in binds.iter() {
        if ordinal != Some(bind.ordinal) {
            match bind.ordinal {
                ..=0 if bind.ordinal >= -15 => out.push(
                    macho::BIND_OPCODE_SET_DYLIB_SPECIAL_IMM
                        | (bind.ordinal as u8 & macho::BIND_IMMEDIATE_MASK),
                ),
                1..=15 => out.push(macho::BIND_OPCODE_SET_DYLIB_ORDINAL_IMM | bind.ordinal as u8),
                16.. => {
                    out.push(macho::BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB);
                    uleb(&mut out, bind.ordinal as u64);
                }
                other => return Err(format!("a bind to the library ordinal {other}")),
            }
            ordinal = Some(bind.ordinal);
        }
        if symbol != Some((&bind.name, bind.weak)) {
            if bind.name.is_empty() || bind.name.contains(&0) {
                return Err(format!("a bind at {:#x} to a symbol of no name", bind.addr));
            }
            let flags = if bind.weak { macho::BIND_SYMBOL_FLAGS_WEAK_IMPORT } else { 0 };
            out.push(macho::BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM | flags);
            out.extend_from_slice(&bind.name);
            out.push(0);
            symbol = Some((&bind.name, bind.weak));
        }
        if addend != bind.addend {
            out.push(macho::BIND_OPCODE_SET_ADDEND_SLEB);
            sleb(&mut out, bind.addend);
            addend = bind.addend;
        }
        let segment = segment_of(spans, bind.addr, "a pointer to bind")?;
        match cursor {
            Some((at, address)) if at == segment && bind.addr >= address => {
                if bind.addr > address {
                    out.push(macho::BIND_OPCODE_ADD_ADDR_ULEB);
                    uleb(&mut out, bind.addr - address);
                }
            }
            _ => {
                out.push(macho::BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB | segment as u8);
                uleb(&mut out, bind.addr - spans[segment].start);
            }
        }
        out.push(macho::BIND_OPCODE_DO_BIND);
        cursor = Some((segment, bind.addr + 8));
    }
    out.push(macho::BIND_OPCODE_DONE);
    out.resize(out.len().next_multiple_of(8), 0);
    Ok(out)
}

#[derive(Default)]
struct Node {
    edges: Vec<(Vec<u8>, usize)>,
    /// Its export's flags and address (from the image's start).
    terminal: Option<(u64, u64)>,
    offset: u64,
}

impl Node {
    fn info_len(&self) -> u64 {
        self.terminal.map_or(0, |(flags, addr)| uleb_len(flags) + uleb_len(addr))
    }
}

/// The export trie of `exports` (Mach-O name, `EXPORT_SYMBOL_FLAGS_*`, address from the
/// image's start), as dyld and `dlsym` walk it.
pub(super) fn trie(exports: &mut [(Vec<u8>, u64, u64)]) -> Result<Vec<u8>, String> {
    if exports.is_empty() {
        return Ok(Vec::new());
    }
    exports.sort_unstable();
    if let Some(twice) = exports.windows(2).find(|it| it[0].0 == it[1].0) {
        return Err(format!("`{}` is exported twice", String::from_utf8_lossy(&twice[0].0)));
    }
    let mut nodes = vec![Node::default()];
    for (name, flags, addr) in exports.iter() {
        if name.is_empty() || name.contains(&0) {
            return Err("an export of no name".to_owned());
        }
        let (mut node, mut name) = (0, &name[..]);
        loop {
            if name.is_empty() {
                nodes[node].terminal = Some((*flags, *addr));
                break;
            }
            let edge = nodes[node].edges.iter().position(|it| it.0[0] == name[0]);
            let Some(edge) = edge else {
                nodes.push(Node { terminal: Some((*flags, *addr)), ..Node::default() });
                let child = nodes.len() - 1;
                nodes[node].edges.push((name.to_vec(), child));
                break;
            };
            let (label, child) = nodes[node].edges[edge].clone();
            let common = label.iter().zip(name).take_while(|(a, b)| a == b).count();
            if common < label.len() {
                nodes.push(Node { edges: vec![(label[common..].to_vec(), child)], ..Node::default() });
                let middle = nodes.len() - 1;
                nodes[node].edges[edge] = (label[..common].to_vec(), middle);
                node = middle;
            } else {
                node = child;
            }
            name = &name[common..];
        }
    }
    for node in &mut nodes {
        node.edges.sort_unstable();
    }
    // Each node before its children, depth first; offsets until no ULEB changes its length.
    let mut order = Vec::new();
    let mut stack = vec![0];
    while let Some(node) = stack.pop() {
        order.push(node);
        stack.extend(nodes[node].edges.iter().rev().map(|it| it.1));
    }
    loop {
        let mut offset = 0;
        let mut moved = false;
        for &node in &order {
            moved |= nodes[node].offset != offset;
            nodes[node].offset = offset;
            let info = nodes[node].info_len();
            let mut len = uleb_len(info) + info + 1;
            for (label, child) in &nodes[node].edges {
                len += label.len() as u64 + 1 + uleb_len(nodes[*child].offset);
            }
            offset += len;
        }
        if !moved {
            break;
        }
    }
    let mut out = Vec::new();
    for &node in &order {
        let node = &nodes[node];
        debug_assert_eq!(out.len() as u64, node.offset);
        uleb(&mut out, node.info_len());
        if let Some((flags, addr)) = node.terminal {
            uleb(&mut out, flags);
            uleb(&mut out, addr);
        }
        if node.edges.len() > 255 {
            return Err("an export trie node of more than 255 edges".to_owned());
        }
        out.push(node.edges.len() as u8);
        for (label, child) in &node.edges {
            out.extend_from_slice(label);
            out.push(0);
            uleb(&mut out, nodes[*child].offset);
        }
    }
    out.resize(out.len().next_multiple_of(8), 0);
    Ok(out)
}

/// `LC_FUNCTION_STARTS`: the functions' addresses as ULEB distances, the first from `text`.
pub(super) fn function_starts(text: u64, addrs: &[u64]) -> Vec<u8> {
    let mut addrs = addrs.to_vec();
    addrs.sort_unstable();
    addrs.dedup();
    let mut out = Vec::new();
    let mut last = text;
    // A distance of 0 ends the list: the headers are at `text`, never a function.
    for addr in addrs.into_iter().filter(|it| *it > text) {
        uleb(&mut out, addr - last);
        last = addr;
    }
    if out.is_empty() {
        return out;
    }
    out.push(0);
    out.resize(out.len().next_multiple_of(8), 0);
    out
}

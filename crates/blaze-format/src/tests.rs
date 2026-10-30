use std::mem::offset_of;

use blaze_ids::ContentId;

use crate::{
    Aligned, FormatError, HEADER_LEN, Persist, Record, Rel, Slice, SpanRef, Spans, Str, Writer,
    header, open, open_as, record, schema, to_bytes,
};

record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Leaf: 1 {
        value: u64,
        flag: bool,
        pad: [u8; 7],
    }
}

record! {
    #[derive(Clone, Copy, Debug)]
    struct Root: 1 {
        leaf: Rel<Leaf>,
        numbers: Slice<u32>,
        name: Str,
        none: Rel<Leaf>,
        leaves: Slice<Leaf>,
    }
}

fn written(flag: u8) -> Vec<u8> {
    let mut out = Writer::new(Root::SCHEMA);
    let leaf = out.push(&Leaf { value: 7, flag: true, pad: [0; 7] });
    let numbers = out.push_slice(&[1u32, 2, 3]);
    let name = out.push_str("blaze");
    let leaves = out.push_slice(&[
        Leaf { value: 1, flag: false, pad: [0; 7] },
        Leaf { value: 2, flag: true, pad: [0; 7] },
    ]);
    if flag != 1 {
        let at = leaf as usize + offset_of!(Leaf, flag);
        out.bytes[at] = flag;
    }
    let at = out.next::<Root>();
    let field = |offset: usize| at + offset as u64;
    let root = Root {
        leaf: Rel::between(field(offset_of!(Root, leaf)), leaf),
        numbers: Slice::between(field(offset_of!(Root, numbers)), numbers, 3),
        name: Str::between(field(offset_of!(Root, name)), name, 5),
        none: Rel::NONE,
        leaves: Slice::between(field(offset_of!(Root, leaves)), leaves, 2),
    };
    assert_eq!(out.push(&root), at);
    out.finish(at)
}

#[test]
fn a_buffer_reads_in_place() {
    let buffer = Aligned::copy_from(&written(1));
    let root = open::<Root>(&buffer).unwrap();
    assert_eq!(root.leaf.get(&buffer), Some(&Leaf { value: 7, flag: true, pad: [0; 7] }));
    assert_eq!(root.numbers.get(&buffer), &[1, 2, 3]);
    assert_eq!(root.name.as_str(&buffer), Ok("blaze"));
    assert_eq!(root.none.get(&buffer), None);
    assert_eq!(root.leaves.get(&buffer).iter().map(|it| it.value).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(header(&buffer).unwrap().len, buffer.len() as u64);
    assert_eq!(buffer.len() % crate::ALIGN, 0);
}

#[test]
fn equal_values_are_equal_bytes() {
    assert_eq!(written(1), written(1));
}

#[test]
fn a_buffer_reads_the_same_wherever_it_is() {
    let bytes = written(1);
    let (a, b) = (Aligned::copy_from(&bytes), Aligned::copy_from(&bytes));
    let (ra, rb) = (open::<Root>(&a).unwrap(), open::<Root>(&b).unwrap());
    assert_eq!(ra.numbers.get(&a), rb.numbers.get(&b));
    assert_eq!(ra.leaf.get(&a), rb.leaf.get(&b));
}

#[test]
fn a_reader_of_another_schema_refuses() {
    let buffer = Aligned::copy_from(&written(1));
    assert_eq!(
        open_as::<Root>(&buffer, schema("root 2")).unwrap_err(),
        FormatError::Schema { found: Root::SCHEMA, wanted: schema("root 2") }
    );
    assert!(matches!(open::<Leaf>(&buffer), Err(FormatError::Schema { .. })));
}

#[test]
fn not_a_buffer_is_refused() {
    let bytes = written(1);
    assert!(matches!(open::<Root>(&Aligned::copy_from(&bytes[..20])), Err(FormatError::Header(_))));
    let mut magic = bytes.clone();
    magic[0] = b'X';
    assert!(matches!(open::<Root>(&Aligned::copy_from(&magic)), Err(FormatError::Header(_))));
    let mut longer = bytes.clone();
    longer.extend([0; 16]);
    assert!(matches!(open::<Root>(&Aligned::copy_from(&longer)), Err(FormatError::Header(_))));
    let mut shifted = vec![0];
    shifted.extend(&bytes);
    let shifted = Aligned::copy_from(&shifted);
    assert_eq!(
        open::<Root>(&shifted[1..]).unwrap_err(),
        FormatError::Header("not at an address aligned to 16 bytes")
    );
}

#[test]
fn an_invalid_value_is_refused() {
    let buffer = Aligned::copy_from(&written(2));
    assert!(matches!(open::<Root>(&buffer), Err(FormatError::Invalid { .. })));
}

fn with_root(edit: impl FnOnce(&mut Vec<u8>, usize)) -> Aligned {
    let mut bytes = written(1);
    let root = header(&Aligned::copy_from(&bytes)).unwrap().root as usize;
    edit(&mut bytes, root);
    Aligned::copy_from(&bytes)
}

fn set_i32(bytes: &mut [u8], at: usize, value: i32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[test]
fn a_reference_that_leaves_is_refused() {
    let forward = with_root(|bytes, root| set_i32(bytes, root + offset_of!(Root, leaf), 8));
    assert!(matches!(open::<Root>(&forward), Err(FormatError::OutOfBounds { .. })));
    let into_header = with_root(|bytes, root| {
        set_i32(bytes, root + offset_of!(Root, leaf), -(root as i32) + 8)
    });
    assert!(matches!(open::<Root>(&into_header), Err(FormatError::OutOfBounds { .. })));
    let misaligned = with_root(|bytes, root| {
        let at = root + offset_of!(Root, numbers);
        let offset = i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        set_i32(bytes, at, offset + 1)
    });
    assert!(matches!(open::<Root>(&misaligned), Err(FormatError::OutOfBounds { .. })));
    let too_long = with_root(|bytes, root| {
        let at = root + offset_of!(Root, numbers) + 4;
        bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    });
    assert!(matches!(open::<Root>(&too_long), Err(FormatError::OutOfBounds { .. })));
    let root_out = with_root(|bytes, _| {
        let at = offset_of!(crate::Header, root);
        bytes[at..at + 8].copy_from_slice(&(HEADER_LEN as u64 - 8).to_le_bytes());
    });
    assert!(matches!(open::<Root>(&root_out), Err(FormatError::OutOfBounds { .. })));
}

#[test]
fn text_that_is_not_utf8_is_refused() {
    let buffer = with_root(|bytes, root| {
        let at = root + offset_of!(Root, name);
        let offset = i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        bytes[(at as i64 + offset as i64) as usize] = 0xff;
    });
    let root = open::<Root>(&buffer).unwrap();
    assert!(matches!(root.name.as_str(&buffer), Err(FormatError::Invalid { .. })));
}

#[test]
#[should_panic(expected = "leaves the buffer")]
fn following_into_another_buffer_panics() {
    let (a, b) = (Aligned::copy_from(&written(1)), Aligned::copy_from(&written(1)));
    let root = open::<Root>(&a).unwrap();
    root.numbers.get(&b);
}

#[test]
fn spans_round_trip() {
    let anchor = ContentId::of("blaze v2 test 1", b"fn main");
    let mut spans = Spans::new(anchor);
    let a = spans.push(0, 7);
    let b = spans.push(3, 2);
    assert_eq!(b, SpanRef { anchor, index: 1 });
    let bytes = Aligned::copy_from(&to_bytes(&spans));
    let read = Spans::read(&bytes).unwrap();
    assert_eq!(read.anchor, anchor);
    assert_eq!(read.get(a), spans.get(a));
    assert_eq!(read.get(b).map(|it| (it.start, it.len)), Some((3, 2)));
    let other = SpanRef { anchor: ContentId::of("blaze v2 test 1", b"fn other"), index: 0 };
    assert_eq!(read.get(other), None);
    assert_eq!(read.to_spans(), spans);
    let empty = Spans::new(anchor);
    let bytes = Aligned::copy_from(&to_bytes(&empty));
    assert_eq!(Spans::read(&bytes).unwrap().spans, &[]);
}

#[test]
fn schemas_name_layouts() {
    assert_ne!(Root::SCHEMA, Leaf::SCHEMA);
    assert_ne!(<Rel<u32> as Record>::SCHEMA, <Rel<u64> as Record>::SCHEMA);
    assert_ne!(<Rel<u32> as Record>::SCHEMA, <Slice<u32> as Record>::SCHEMA);
    assert_ne!(<[u8; 4] as Record>::SCHEMA, <[u8; 5] as Record>::SCHEMA);
    assert_ne!(<Spans as Persist>::SCHEMA, <SpanRef as Record>::SCHEMA);
}

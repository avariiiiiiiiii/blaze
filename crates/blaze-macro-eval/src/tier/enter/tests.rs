use super::*;

fn layout() -> Layout {
    Layout::new(64, 64, Bounds { stack: 1 << 16, heap: 1 << 20 }).unwrap()
}

#[test]
fn a_word_inside_the_region_is_an_address_and_others_are_not() {
    let layout = layout();
    let mut bytes = [0u8; 24];
    assert!(!holds_address(&bytes, &layout), "zeros");
    set_word(&mut bytes, 1, 42);
    assert!(!holds_address(&bytes, &layout), "a small integer, below the header");
    set_word(&mut bytes, 2, u64::from(layout.heap.start) + 16);
    assert!(holds_address(&bytes, &layout), "a heap offset");
    set_word(&mut bytes, 2, u64::from(layout.heap.end));
    assert!(!holds_address(&bytes, &layout), "past the heap");
    set_word(&mut bytes, 0, u64::from(layout.writable.start));
    assert!(holds_address(&bytes, &layout), "a static's offset");
}

#[test]
fn the_bridges_layouts_are_its_words() {
    let mut buffer = [0u8; BUFFER];
    for n in 0..5 {
        set_word(&mut buffer, n, 10 + n as u64);
    }
    assert_eq!((word(&buffer, 0), word(&buffer, 4)), (10, 14));
    assert_eq!(CONFIG, BUFFER + 2 * 8 + 8, "a buffer, a closure of two words, and a padded bool");
}

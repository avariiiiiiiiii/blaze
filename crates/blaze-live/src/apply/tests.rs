use super::*;

const PAGE: u64 = 0x1000;

fn process() -> Process {
    Process { pid: 1, uuid: [0; 16], started: 0, next: 0x10_0000, end: 0x20_0000 }
}

fn code(base: u64, text: usize) -> Code {
    Code { base, text: vec![0; text], data: vec![1; 10], ..Code::default() }
}

fn write(address: u64, bytes: &[u8]) -> Write {
    Write { address, bytes: bytes.to_vec(), pointers: Vec::new() }
}

/// A patch that fails after its room is taken leaves `next` past it: the next patch laid
/// out there is refused, one laid out past it is taken.
#[test]
fn a_failed_apply_keeps_its_room_from_the_next_patch() {
    let mut process = process();
    let end = claim(&mut process, &code(0x10_0000, 0x2000), PAGE).expect("room");
    assert_eq!((end, process.next), (0x10_3000, 0x10_3000));
    // Whatever became of that patch, a second laid out over it is refused.
    let refused = claim(&mut process, &code(0x10_0000, 0x1000), PAGE).unwrap_err();
    assert!(refused.contains("no room"), "{refused}");
    assert_eq!(process.next, 0x10_3000);
    assert_eq!(claim(&mut process, &code(0x10_3000, 0x1000), PAGE), Ok(0x10_5000));
}

#[test]
fn a_patch_past_the_room_or_memory_is_refused() {
    let mut process = process();
    assert!(claim(&mut process, &code(0x1f_f000, 0x1000), PAGE).unwrap_err().contains("no room"));
    let wraps = claim(&mut process, &code(u64::MAX & !(PAGE - 1), 0x1000), PAGE).unwrap_err();
    assert!(wraps.contains("past the end of memory"), "{wraps}");
    assert!(claim(&mut process, &code(0x10_0800, 0x1000), PAGE).unwrap_err().contains("pages"));
    assert!(claim(&mut process, &code(0x10_0000, 0x800), PAGE).unwrap_err().contains("pages"));
    assert_eq!(process.next, 0x10_0000);
}

#[test]
fn a_write_outside_the_image_or_wrapping_is_refused() {
    let image = 0x5000..0x9000;
    assert_eq!(in_image(&[write(0x1000, &[0; 8]), write(0x4ff8, &[0; 8])], 0x4000, &image), Ok(()));
    let before = in_image(&[write(0x0fff, &[0; 8])], 0x4000, &image).unwrap_err();
    assert!(before.contains("0x4fff"), "{before}");
    let after = in_image(&[write(0x4ff9, &[0; 8])], 0x4000, &image).unwrap_err();
    assert!(after.contains("0x8ff9"), "{after}");
    // An address whose slide or end wraps past the end of memory.
    assert!(in_image(&[write(u64::MAX - 2, &[0; 8])], 0, &(0..u64::MAX)).is_err());
    assert!(in_image(&[write(u64::MAX - 0x1000, &[0; 8])], 0x2000, &(0..u64::MAX)).is_err());
}

#[test]
fn addresses_slid_in_place_stay_in_their_bytes() {
    let mut bytes = 0x1000u64.to_le_bytes().to_vec();
    slide_at(&mut bytes, 0, 0x20).expect("in place");
    assert_eq!(bytes, 0x1020u64.to_le_bytes());
    assert!(slide_at(&mut bytes, 1, 0x20).is_err());
    assert!(slide_at(&mut bytes, u64::MAX - 3, 0x20).is_err());
}

fn le(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|it| it.to_le_bytes()).collect()
}

/// A CIE (length 8, pointer 0), an FDE pointing back at it (length 8), then a zero length.
fn eh_frame() -> Vec<u8> {
    le(&[8, 0, 0x0101, 8, 16, 0x0202, 0])
}

#[test]
fn a_run_of_entries_ends_in_a_zero_length_inside_the_patch() {
    let mut text = vec![0xaa; 16];
    text.extend(eh_frame());
    assert_eq!(frame_run(&text, (16, 28)), Ok(Some(16)));
    // The zero length just after the section is the run's end too.
    assert_eq!(frame_run(&text, (16, 24)), Ok(Some(16)));
    assert_eq!(frame_run(&text, (0, 0)), Ok(None));
    // An entry past its section, a section past the text, a length that wraps.
    assert!(frame_run(&text, (16, 20)).unwrap_err().contains("past its section"));
    assert!(frame_run(&text, (16, 40)).unwrap_err().contains("past the end of the patch"));
    assert!(frame_run(&text, (u64::MAX, 2)).is_err());
    // A run with no zero length in the text.
    let unended = [vec![0; 16], le(&[8, 0, 0x0101])].concat();
    assert!(frame_run(&unended, (16, 12)).is_err());
}

#[test]
fn each_fde_is_inside_its_section_and_names_a_cie_before_it() {
    let text = [vec![0xaa; 16], eh_frame()].concat();
    assert_eq!(fdes(&text, (16, 28)), Ok(vec![28]));
    // An FDE past its section.
    assert!(fdes(&text, (16, 20)).unwrap_err().contains("past its section"));
    // An FDE whose CIE pointer goes before the section, or at no CIE.
    let before = [vec![0xaa; 16], le(&[8, 0, 0x0101, 8, 40, 0x0202, 0])].concat();
    assert!(fdes(&before, (16, 28)).unwrap_err().contains("names no CIE"));
    let between = [vec![0xaa; 16], le(&[8, 0, 0x0101, 8, 12, 0x0202, 0])].concat();
    assert!(fdes(&between, (16, 28)).unwrap_err().contains("names no CIE"));
}

#[test]
fn runtime_functions_are_whole_and_inside_the_patch() {
    let patch = 0x10_0000;
    let mut memory = vec![0u8; 0x100];
    memory[0x40..0x4c].copy_from_slice(&le(&[0x10_0000, 0x10_0020, 0x10_0080]));
    memory[0x4c..0x58].copy_from_slice(&le(&[0x10_0020, 0x10_0030, 0x10_0090]));
    assert_eq!(functions(&memory, (0x40, 24), patch), Ok(2));
    assert_eq!(functions(&memory, (0, 0), patch), Ok(0));
    assert!(functions(&memory, (0x40, 20), patch).unwrap_err().contains("whole entries"));
    assert!(functions(&memory, (0x42, 12), patch).unwrap_err().contains("whole entries"));
    assert!(functions(&memory, (0xf8, 12), patch).unwrap_err().contains("past the end"));
    // A function or unwind information outside the patch.
    memory[0x4c..0x58].copy_from_slice(&le(&[0x10_0020, 0x10_0030, 0x10_00fe]));
    assert!(functions(&memory, (0x40, 24), patch).unwrap_err().contains("outside the patch"));
    memory[0x4c..0x58].copy_from_slice(&le(&[0x0f_0000, 0x10_0030, 0x10_0090]));
    assert!(functions(&memory, (0x40, 24), patch).unwrap_err().contains("outside the patch"));
}

/// What the image has in the tests: each byte its address's low byte.
fn image_byte(address: u64) -> u8 {
    address as u8 ^ 0x5a
}

fn read_image(address: u64, into: &mut [u8]) {
    for (nth, byte) in into.iter_mut().enumerate() {
        *byte = image_byte(address + nth as u64);
    }
}

fn patch_of(writes: Vec<Write>, redirected: u64) -> Code {
    Code { writes, redirected, ..Code::default() }
}

#[test]
fn a_plan_puts_back_what_the_next_patch_drops_and_what_a_longer_write_left() {
    let mut overwrites = Overwrites::default();
    let slide = 0x1_0000;
    let first = patch_of(vec![write(0x100, &[1; 12]), write(0x200, &[2; 8])], 1);
    let plan = overwrites.plan(&first, slide).expect("a plan");
    assert_eq!(plan.steps.iter().map(|it| it.address).collect::<Vec<_>>(), [0x1_0100, 0x1_0200]);
    assert!(plan.steps[0].jump && !plan.steps[1].jump);
    overwrites.read(&plan, read_image);
    overwrites.record(&plan, plan.steps.len());

    // Slid addresses are what is kept: the jump at 0x100, shorter now, is not put back first.
    let second = patch_of(vec![write(0x100, &[3; 5])], 1);
    let plan = overwrites.plan(&second, slide).expect("a plan");
    let steps: Vec<(u64, Vec<u8>)> =
        plan.steps.iter().map(|it| (it.address, it.bytes.clone())).collect();
    let original =
        |address: u64, len: u64| (address..address + len).map(image_byte).collect::<Vec<_>>();
    let shorter = [vec![3; 5], original(0x1_0105, 7)].concat();
    assert_eq!(steps, [(0x1_0200, original(0x1_0200, 8)), (0x1_0100, shorter)]);
    overwrites.read(&plan, |_, _| panic!("nothing new to read"));
    overwrites.record(&plan, plan.steps.len());
    assert_eq!(overwrites.written, HashMap::from([(0x1_0100, 5)]));
}

/// The originals are read into the room `plan` reserved: none of them grows while the
/// threads are stopped.
#[test]
fn reading_the_originals_allocates_nothing() {
    let mut overwrites = Overwrites::default();
    let plan = overwrites
        .plan(&patch_of(vec![write(0x100, &[1; 40]), write(0x300, &[1; 3])], 2), 0)
        .unwrap();
    let before: HashMap<u64, usize> =
        overwrites.originals.iter().map(|(at, it)| (*at, it.capacity())).collect();
    overwrites.read(&plan, read_image);
    for (at, original) in &overwrites.originals {
        assert_eq!(original.capacity(), before[at], "{at:#x}");
        assert_eq!(
            *original,
            (*at..*at + original.len() as u64).map(image_byte).collect::<Vec<_>>()
        );
    }
}

#[test]
fn a_pc_strictly_inside_a_write_is_found() {
    let mut overwrites = Overwrites::default();
    let writes = vec![write(0x100, &[0; 5]), write(0x200, &[0; 12]), write(0x204, &[0; 2])];
    let plan = overwrites.plan(&patch_of(writes, 3), 0).unwrap();
    assert_eq!(plan.inside(&[0x100, 0x105, 0x1ff, 0x20c]), None);
    assert_eq!(plan.inside(&[0x50, 0x104]), Some((0x104, 0x100)));
    assert_eq!(plan.inside(&[0x101]), Some((0x101, 0x100)));
    // Inside the longer write behind a shorter one that starts later.
    assert_eq!(plan.inside(&[0x208]), Some((0x208, 0x200)));
    assert_eq!(plan.inside(&[]), None);
}

/// Reads `words` as the image from address 0.
fn words(words: &[u32]) -> impl FnMut(u64, &mut [u8]) {
    let bytes = le(words);
    move |address, into| {
        let at = address as usize;
        into.copy_from_slice(&bytes[at..at + into.len()]);
    }
}

#[test]
fn a_call_among_the_words_a_jump_covers_is_found() {
    let calls = |code: &[u32]| {
        let mut overwrites = Overwrites::default();
        let plan = overwrites.plan(&patch_of(vec![write(0, &[0; 12])], 1), 0).unwrap();
        overwrites.read(&plan, words(code));
        overwrites.calls(&plan)
    };
    // stp, mov, then a call as the last word the jump covers: it returns past the jump.
    assert_eq!(calls(&[0xa9bf_7bfd, 0x9100_03fd, 0x9400_0010]), None);
    // bl, blr x8, blraaz x1, blraa x2, x3 before the last word.
    for call in [0x9400_0010, 0xd63f_0100, 0xd63f_083f, 0xd73f_0843] {
        assert_eq!(calls(&[0xa9bf_7bfd, call, 0xd503_201f]), Some(0), "{call:#x}");
    }
    // br and ret are not calls.
    assert_eq!(calls(&[0xd61f_0100, 0xd65f_03c0, 0xd503_201f]), None);
}

/// Stops the fake threads at the pcs it is given, one list each time.
struct Fake {
    rounds: Vec<Vec<u64>>,
    stopped: usize,
}

impl World for Fake {
    fn stopped<T>(&mut self, f: impl FnOnce(&[u64]) -> T) -> Result<T, String> {
        let pcs = self.rounds.get(self.stopped).or(self.rounds.last()).cloned().unwrap_or_default();
        self.stopped += 1;
        Ok(f(&pcs))
    }
}

#[test]
fn writes_wait_for_a_thread_inside_them_and_then_refuse() {
    let mut overwrites = Overwrites::default();
    let plan = overwrites.plan(&patch_of(vec![write(0x100, &[0xe9; 5])], 1), 0).unwrap();
    let mut world = Fake { rounds: vec![vec![0x102], vec![0x102], vec![0x100, 0x200]], stopped: 0 };
    let mut written = Vec::new();
    let done = write_stopped(&mut world, &mut overwrites, &plan, false, read_image, |n, step| {
        written.push((n, step.address));
        Ok(())
    });
    assert_eq!(done, Ok(()));
    assert_eq!((world.stopped, written), (3, vec![(0, 0x100)]));
    assert_eq!(overwrites.written, HashMap::from([(0x100, 5)]));

    let plan = overwrites.plan(&patch_of(vec![write(0x100, &[0xe9; 5])], 1), 0).unwrap();
    let mut world = Fake { rounds: vec![vec![0x101]], stopped: 0 };
    let refused = write_stopped(&mut world, &mut overwrites, &plan, false, read_image, |_, _| {
        panic!("written with a thread inside")
    });
    let why = match refused {
        Err(Failed::Declined(why)) => why,
        other => panic!("{other:?}"),
    };
    assert!(why.contains("0x101") && why.contains("0x100"), "{why}");
    assert_eq!(world.stopped, TRIES as usize);
}

#[test]
fn a_write_that_breaks_after_others_says_restart() {
    let mut overwrites = Overwrites::default();
    let writes = vec![write(0x100, &[0xe9; 5]), write(0x200, &[0xe9; 5])];
    let plan = overwrites.plan(&patch_of(writes, 2), 0).unwrap();
    let mut world = Fake { rounds: vec![vec![]], stopped: 0 };
    let broke = |n: usize, step: &Step| match n {
        0 => Ok(()),
        _ => Err(Broke { what: "making it writable", at: step.address, code: 12, written: false }),
    };
    let failed = write_stopped(&mut world, &mut overwrites, &plan, false, read_image, broke);
    let why = match failed {
        Err(Failed::Restart(why)) => why,
        other => panic!("{other:?}"),
    };
    assert!(why.contains("0x200") && why.contains("after 1 of 2"), "{why}");
    assert_eq!(overwrites.written, HashMap::from([(0x100, 5)]));

    let mut fresh = Overwrites::default();
    let plan = fresh.plan(&patch_of(vec![write(0x100, &[0xe9; 5])], 1), 0).unwrap();
    let first = |_: usize, step: &Step| {
        Err(Broke { what: "making it writable", at: step.address, code: 12, written: false })
    };
    let failed = write_stopped(&mut world, &mut fresh, &plan, false, read_image, first);
    assert!(matches!(failed, Err(Failed::Declined(_))), "{failed:?}");
    assert!(fresh.written.is_empty());
}

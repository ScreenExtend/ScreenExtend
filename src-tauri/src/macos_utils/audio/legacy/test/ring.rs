use crate::macos_utils::audio::legacy::shm_reader::{clamp_read, layout};

#[test]
fn no_lap_when_reader_keeps_up() {
    // writer 100 ahead of a 1024 ring: nothing lost
    let (new_read, avail, lapped) = clamp_read(500, 600, 1024);
    assert_eq!(new_read, 500);
    assert_eq!(avail, 100);
    assert!(!lapped);
}

#[test]
fn exactly_full_is_not_a_lap() {
    let (new_read, avail, lapped) = clamp_read(0, 1024, 1024);
    assert_eq!((new_read, avail, lapped), (0, 1024, false));
}

#[test]
fn lap_skips_to_one_ring_behind_head() {
    // writer 1500 ahead of a 1024 ring: 476 samples unrecoverable
    let (new_read, avail, lapped) = clamp_read(0, 1500, 1024);
    assert!(lapped);
    assert_eq!(avail, 1024);
    assert_eq!(new_read, 1500 - 1024); // == 476
}

#[test]
fn overwrite_wraparound_stays_consistent_over_many_writes() {
    let cap = 8u64;
    let mut read_pos = 0u64;
    let mut write_pos = 0u64;
    for step in 0..1000u64 {
        write_pos = write_pos.wrapping_add(1 + (step % 5));
        let (new_read, avail, _lapped) = clamp_read(read_pos, write_pos, cap);
        read_pos = new_read;
        assert!(avail <= cap);
        assert!(write_pos.wrapping_sub(read_pos) <= cap);
        let n = avail.min(3);
        read_pos = read_pos.wrapping_add(n);
    }
}

#[test]
fn generation_change_means_resync_to_head() {
    // on a generation bump the reader sets read_pos = write_pos (drops the backlog)
    let write_pos = 4242u64;
    let read_pos_after_gen_change = write_pos;
    assert_eq!(
        clamp_read(
            read_pos_after_gen_change,
            write_pos,
            layout::CAPACITY as u64
        ),
        (write_pos, 0, false)
    );
}

/// The header the driver is compiled against, read at build time so this test fails when
/// `shm_ring.hpp` changes and the Rust reader does not. The two are built by different toolchains
/// and neither runs in CI, so a comment saying "change both together" is not enough.
const CPP_HEADER: &str = include_str!("../../../../../macos/ScreenExtendAudio/src/shm_ring.hpp");

/// Value of `inline constexpr <type> <name> = <literal>;` in the C++ header.
fn cpp_constant(name: &str) -> u64 {
    let line = CPP_HEADER
        .lines()
        .find(|l| l.contains("inline constexpr") && l.contains(&format!(" {name} =")))
        .unwrap_or_else(|| panic!("shm_ring.hpp has no `inline constexpr … {name} = …`"));
    let rhs = line.split('=').nth(1).expect("constant has a value");
    let digits: String = rhs
        .trim()
        .trim_start_matches("0x")
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    let radix = if rhs.trim().starts_with("0x") { 16 } else { 10 };
    u64::from_str_radix(&digits, radix)
        .unwrap_or_else(|e| panic!("{name} = {rhs:?} is not a number: {e}"))
}

/// Byte offset asserted for `field` by a `static_assert(offsetof(ShmHeader, field) == N, …)`.
fn cpp_offset(field: &str) -> usize {
    let needle = format!("offsetof(ShmHeader, {field}) ==");
    let idx = CPP_HEADER
        .find(&needle)
        .unwrap_or_else(|| panic!("shm_ring.hpp does not static_assert the offset of `{field}`"));
    let rhs = &CPP_HEADER[idx + needle.len()..];
    let digits: String = rhs
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits
        .parse()
        .unwrap_or_else(|e| panic!("offset of {field} is not a number: {e}"))
}

#[test]
fn layout_matches_the_drivers_own_header() {
    assert_eq!(layout::MAGIC as u64, cpp_constant("kShmMagic"));
    assert_eq!(layout::VERSION as u64, cpp_constant("kLayoutVersion"));
    assert_eq!(layout::CAPACITY as u64, cpp_constant("kRingCapacity"));
    assert_eq!(
        layout::SAMPLE_FORMAT as u64,
        cpp_constant("kFormatF32Interleaved")
    );
    assert!(layout::CAPACITY.is_power_of_two());

    for (field, offset) in layout::OFFSETS {
        assert_eq!(
            cpp_offset(field),
            *offset,
            "`{field}` sits at a different offset in shm_ring.hpp"
        );
    }

    // sizeof(ShmHeader) is asserted in C++ as a literal, so read that assert's literal too
    let sizeof_line = CPP_HEADER
        .lines()
        .find(|l| l.contains("sizeof(ShmHeader) =="))
        .expect("shm_ring.hpp static_asserts sizeof(ShmHeader)");
    let header_bytes: usize = sizeof_line
        .split("sizeof(ShmHeader) ==")
        .nth(1)
        .unwrap()
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .expect("sizeof assert carries a literal");
    assert_eq!(layout::HEADER_BYTES, header_bytes);
    assert_eq!(
        layout::TOTAL_BYTES,
        header_bytes + layout::CAPACITY * 4,
        "total bytes must be the header plus one f32 per ring slot"
    );
}

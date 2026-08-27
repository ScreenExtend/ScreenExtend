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

#[test]
fn layout_matches_cpp_abi_contract() {
    // MUST match macos/ScreenExtendAudio/src/shm_ring.hpp's static_asserts
    assert_eq!(layout::HEADER_BYTES, 64);
    assert_eq!(layout::OFF_WRITE_POS, 32);
    assert_eq!(layout::OFF_GENERATION, 40);
    assert_eq!(layout::MAGIC, 0x3141_4553); // 'SEA1' little-endian
    assert_eq!(layout::VERSION, 1);
    assert_eq!(layout::CAPACITY, 131_072);
    assert!(layout::CAPACITY.is_power_of_two());
    assert_eq!(layout::TOTAL_BYTES, 64 + 131_072 * 4);
}

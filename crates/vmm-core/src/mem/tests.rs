// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;

/// Create a test SubMapping backed by anonymous mmap (no VmmHdl needed).
fn make_test_mapping(size: usize) -> Arc<Mapping> {
    Arc::new(Mapping::anon(size).unwrap())
}

#[test]
fn segid_alloc_stops_at_kernel_limit() {
    let alloc = SegidAlloc::new(3);
    assert_eq!(alloc.alloc(), Some(3));
    assert_eq!(alloc.alloc(), Some(4));
    assert_eq!(alloc.alloc(), None);

    let exhausted = SegidAlloc::new(VM_MAX_MEMSEGS);
    assert_eq!(exhausted.alloc(), None);
}

#[test]
fn copy_out_roundtrip_and_bounds() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let pattern: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    sub.write_bytes(&pattern).unwrap();

    let mut copied = vec![0; pattern.len()];
    sub.copy_out(&mut copied).unwrap();
    assert_eq!(copied, pattern);

    let mut oversized = vec![0; 4097];
    let err = sub.copy_out(&mut oversized).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn copy_in_matches_write_bytes() {
    // copy_in is the fast path the boot loaders take. It must land the
    // same bytes as the volatile loop it replaces, or a kernel image
    // loads subtly corrupt.
    let pattern: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();

    let volatile_mapping = make_test_mapping(4096);
    let volatile_sub = SubMapping::new(&volatile_mapping);
    volatile_sub.write_bytes(&pattern).unwrap();
    let mut via_write_bytes = vec![0; pattern.len()];
    volatile_sub.copy_out(&mut via_write_bytes).unwrap();

    let bulk_mapping = make_test_mapping(4096);
    let bulk_sub = SubMapping::new(&bulk_mapping);
    bulk_sub.copy_in(&pattern).unwrap();
    let mut via_copy_in = vec![0; pattern.len()];
    bulk_sub.copy_out(&mut via_copy_in).unwrap();

    assert_eq!(via_copy_in, via_write_bytes);
    assert_eq!(via_copy_in, pattern);
}

#[test]
fn copy_in_rejects_oversized_source() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let err = sub.copy_in(&vec![0u8; 4097]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn copy_in_checks_write_permission() {
    let mapping = make_test_mapping(4096);
    let mut write_only = SubMapping::new(&mapping);
    write_only.prot = Prot::WRITE;
    assert!(write_only.copy_in(&[0; 16]).is_ok());

    let mut read_only = SubMapping::new(&mapping);
    read_only.prot = Prot::READ;
    let err = read_only.copy_in(&[0; 16]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
}

/// Write `payload` to a scratch file and hand back the open handle.
fn scratch_file(name: &str, payload: &[u8]) -> std::fs::File {
    use std::io::Write;
    let dir = std::env::temp_dir().join("vmm-core-mem-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(payload).unwrap();
    f.sync_all().unwrap();
    drop(f);
    std::fs::File::open(&path).unwrap()
}

#[test]
fn read_exact_from_fills_the_mapping() {
    let payload: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    let file = scratch_file("full.bin", &payload);

    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    sub.read_exact_from(&file, 0, payload.len()).unwrap();

    let mut got = vec![0; payload.len()];
    sub.copy_out(&mut got).unwrap();
    assert_eq!(got, payload);
}

#[test]
fn read_exact_from_honors_the_file_offset() {
    let payload: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
    let file = scratch_file("offset.bin", &payload);

    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    sub.read_exact_from(&file, 512, 512).unwrap();

    let mut got = vec![0; 512];
    sub.copy_out(&mut got).unwrap();
    assert_eq!(got, &payload[512..1024]);
}

#[test]
fn read_exact_from_rejects_a_segment_past_end_of_file() {
    // A PT_LOAD claiming more bytes than its file holds must fail, not
    // fill part of the range and leave the tail as it was. Silently
    // short-filling would leak whatever the guest page held before.
    let file = scratch_file("short.bin", &[0xAB; 100]);

    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let err = sub.read_exact_from(&file, 0, 200).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
}

#[test]
fn read_exact_from_rejects_a_length_past_the_mapping() {
    let file = scratch_file("big.bin", &[0xCD; 8192]);

    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let err = sub.read_exact_from(&file, 0, 4097).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn read_exact_from_checks_write_permission() {
    let file = scratch_file("perm.bin", &[0xEF; 64]);

    let mapping = make_test_mapping(4096);
    let mut read_only = SubMapping::new(&mapping);
    read_only.prot = Prot::READ;
    let err = read_only.read_exact_from(&file, 0, 64).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
}

#[test]
fn copy_out_checks_read_permission() {
    let mapping = make_test_mapping(4096);
    let mut read_only = SubMapping::new(&mapping);
    read_only.prot = Prot::READ;
    assert!(read_only.copy_out(&mut [0; 16]).is_ok());

    let mut write_only = SubMapping::new(&mapping);
    write_only.prot = Prot::WRITE;
    let err = write_only.copy_out(&mut [0; 16]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
}

#[test]
fn anonymous_devmem_segment_is_guestless() {
    let segment = DevMemSeg::new_anon(0x1000).unwrap();
    assert_eq!(segment.len(), 0x1000);
    assert_eq!(segment.mapped_gpa(), None);
    segment.map_at(0x8000_0000).unwrap();
    assert_eq!(segment.mapped_gpa(), None);
    segment.unmap().unwrap();
    assert_eq!(segment.mapped_gpa(), None);

    let data = [0x12, 0x34, 0x56, 0x78];
    let view = segment.view();
    view.write_bytes(&data).unwrap();
    let mut copied = [0; 4];
    view.copy_out(&mut copied).unwrap();
    assert_eq!(copied, data);
}

#[test]
fn read_aligned_u32() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Page-aligned pointer can read u32
    let val = sub.read::<u32>();
    assert!(val.is_ok());
}

#[test]
fn read_unaligned_u32_fails() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Offset by 1 byte to create misalignment for u32 (align=4)
    let unaligned = sub.subregion(1, 4).unwrap();
    let result = unaligned.read::<u32>();
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn write_unaligned_u32_fails() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let unaligned = sub.subregion(1, 4).unwrap();
    let result = unaligned.write::<u32>(&42);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn read_aligned_u16_at_offset() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Offset by 2 is still aligned for u16 (align=2)
    let aligned = sub.subregion(2, 2).unwrap();
    assert!(aligned.read::<u16>().is_ok());
}

#[test]
fn read_unaligned_u16_fails() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Offset by 1 is misaligned for u16 (align=2)
    let unaligned = sub.subregion(1, 2).unwrap();
    assert!(unaligned.read::<u16>().is_err());
}

#[test]
fn read_u8_always_aligned() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // u8 has align=1, so any offset works
    let at_offset_1 = sub.subregion(1, 1).unwrap();
    assert!(at_offset_1.read::<u8>().is_ok());
}

#[test]
fn read_write_bytes_ignores_alignment() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let unaligned = sub.subregion(1, 4).unwrap();
    // read_bytes and write_bytes work at any alignment
    let mut buf = [0u8; 4];
    assert!(unaligned.read_bytes(&mut buf).is_ok());
    assert!(unaligned.write_bytes(&[1, 2, 3, 4]).is_ok());
}

#[test]
fn read_write_roundtrip() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    sub.write::<u32>(&0xDEAD_BEEF).unwrap();
    let val = sub.read::<u32>().unwrap();
    assert_eq!(val, 0xDEAD_BEEF);
}

#[test]
fn subregion_bounds_check() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Exactly at end: should succeed
    assert!(sub.subregion(4092, 4).is_some());
    // Past end: should fail
    assert!(sub.subregion(4093, 4).is_none());
    // Overflow: should fail
    assert!(sub.subregion(usize::MAX, 1).is_none());
}

#[test]
fn read_too_small_fails() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let tiny = sub.subregion(0, 2).unwrap();
    assert!(tiny.read::<u32>().is_err());
}

#[test]
fn write_u32_too_small_fails() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let tiny = sub.subregion(0, 2).unwrap();
    let result = tiny.write::<u32>(&0xDEAD);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn read_u16_aligned_roundtrip() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Write at aligned offset, read back
    let region = sub.subregion(0, 2).unwrap();
    region.write::<u16>(&0xBEEF).unwrap();
    let val = region.read::<u16>().unwrap();
    assert_eq!(val, 0xBEEF);
}

#[test]
fn read_u16_at_aligned_offset_roundtrip() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Offset 4 is aligned for u16
    let region = sub.subregion(4, 2).unwrap();
    region.write::<u16>(&0xCAFE).unwrap();
    let val = region.read::<u16>().unwrap();
    assert_eq!(val, 0xCAFE);
}

#[test]
fn write_bytes_read_bytes_roundtrip() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let data = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
    sub.write_bytes(&data).unwrap();
    let mut buf = [0u8; 8];
    sub.read_bytes(&mut buf).unwrap();
    assert_eq!(buf, data);
}

#[test]
fn write_bytes_read_bytes_at_offset() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Write at an unaligned offset via subregion
    let region = sub.subregion(3, 5).unwrap();
    let data = [1, 2, 3, 4, 5];
    region.write_bytes(&data).unwrap();
    let mut buf = [0u8; 5];
    region.read_bytes(&mut buf).unwrap();
    assert_eq!(buf, data);
}

#[test]
fn read_bytes_exceeds_bounds() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let tiny = sub.subregion(0, 4).unwrap();
    let mut buf = [0u8; 8];
    let result = tiny.read_bytes(&mut buf);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn write_bytes_exceeds_bounds() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    let tiny = sub.subregion(0, 4).unwrap();
    let result = tiny.write_bytes(&[0u8; 8]);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[test]
fn subregion_exact_end() {
    let mapping = make_test_mapping(256);
    let sub = SubMapping::new(&mapping);
    // Exactly the full mapping
    assert!(sub.subregion(0, 256).is_some());
    // One byte past the end
    assert!(sub.subregion(0, 257).is_none());
    // Last byte
    assert!(sub.subregion(255, 1).is_some());
    // Zero-length at end
    assert!(sub.subregion(256, 0).is_some());
}

#[test]
fn subregion_nested() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    // Create a subregion, then a subregion of that
    let outer = sub.subregion(100, 200).unwrap();
    let inner = outer.subregion(50, 100).unwrap();
    assert_eq!(inner.len(), 100);
    // Inner subregion cannot exceed outer
    assert!(outer.subregion(150, 100).is_none());
}

#[test]
fn read_from_write_only_mapping_fails() {
    // Create a write-only mapping
    let raw = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    assert_ne!(raw, libc::MAP_FAILED);
    let ptr = NonNull::new(raw as *mut u8).unwrap();
    let mapping = Arc::new(Mapping {
        ptr,
        len: 4096,
        prot: Prot::WRITE,
    });
    let sub = SubMapping::new(&mapping);
    let result = sub.read::<u32>();
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::PermissionDenied);
}

#[test]
fn write_to_read_only_mapping_fails() {
    let raw = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    assert_ne!(raw, libc::MAP_FAILED);
    let ptr = NonNull::new(raw as *mut u8).unwrap();
    let mapping = Arc::new(Mapping {
        ptr,
        len: 4096,
        prot: Prot::READ,
    });
    let sub = SubMapping::new(&mapping);
    let result = sub.write::<u32>(&42);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::PermissionDenied);
}

#[test]
fn len_and_is_empty() {
    let mapping = make_test_mapping(4096);
    let sub = SubMapping::new(&mapping);
    assert_eq!(sub.len(), 4096);
    assert!(!sub.is_empty());

    let zero = sub.subregion(0, 0).unwrap();
    assert_eq!(zero.len(), 0);
    assert!(zero.is_empty());
}

#[test]
fn phys_map_empty() {
    let map = PhysMap::new();
    assert_eq!(map.num_regions(), 0);
    assert_eq!(map.total_memory(), 0);
    assert!(map.regions().is_empty());
    // Lookup in empty map returns None
    assert!(map.lookup(0, 1).is_none());
    assert!(map.lookup(0x1000, 4096).is_none());
}

// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Dataset Management and Write Zeroes, and the 512e namespace fields a
//! guest reads to align its writes.

use super::*;

const E512: NvmeConfig = NvmeConfig {
    lba_size: LbaSize::B512,
    volatile_write_cache: false,
    msi_policy: MsiPolicy::PerBatch,
    deallocate: true,
    hints: Some(NsHints {
        physical_bytes: 4096,
        optimal_write_bytes: 32 * 4096,
    }),
};

const LIST: u64 = 0x60000;

fn setup(config: NvmeConfig) -> (Harness, Arc<Manual>) {
    let manual = Manual::durable(IO_QDEPTH);
    let mut h = Harness::manual_with(&manual, config);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    (h, manual)
}

/// A Dataset Management range list of (slba, nlb).
fn put_ranges(h: &Harness, ranges: &[(u64, u32)]) {
    let mut b = Vec::new();
    for &(slba, nlb) in ranges {
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&nlb.to_le_bytes());
        b.extend_from_slice(&slba.to_le_bytes());
    }
    h.mem().write(LIST, &b).expect("range list");
}

fn dsm(ranges: usize, ad: bool) -> Sqe {
    Sqe {
        opcode: NVM_OPC_DSM,
        nsid: 1,
        prp1: LIST,
        cdw10: ranges as u32 - 1,
        cdw11: if ad { DSM_AD } else { 0 },
        ..Sqe::default()
    }
}

fn write_zeroes(slba: u64, nlb: u16, deac: bool) -> Sqe {
    Sqe {
        opcode: NVM_OPC_WRITE_ZEROES,
        nsid: 1,
        cdw10: slba as u32,
        cdw11: (slba >> 32) as u32,
        cdw12: u32::from(nlb - 1) | if deac { WZ_DEAC } else { 0 },
        ..Sqe::default()
    }
}

#[test]
fn deallocate_reaches_the_backend_as_byte_ranges() {
    let (h, manual) = setup(E512);
    put_ranges(&h, &[(1, 7), (100, 0), (64, 1024)]);
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, dsm(3, true), 1);
    let req = manual.take().pop().expect("one request");
    assert_eq!(req.op(), BlockOp::Deallocate);
    // The empty range is dropped.
    assert_eq!(req.zero_ranges(), &[(512, 7 * 512), (64 * 512, 1024 * 512)]);
    assert_eq!(req.data().len(), 0);
    assert_eq!((req.lba(), req.blocks()), (0, 0));
    let status = manual.run(&req);
    manual.sink().complete(&[(req.id(), status)]);
    assert_eq!(h.wait_cqe(CQ1, 0, true).status(), Status::SUCCESS);
}

#[test]
fn dataset_management_without_deallocate_does_nothing() {
    let (h, manual) = setup(E512);
    put_ranges(&h, &[(1, 7)]);
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, dsm(1, false), 1);
    assert_eq!(h.wait_cqe(CQ1, 0, true).status(), Status::SUCCESS);
    assert!(manual.submitted_queues().is_empty());
}

#[test]
fn write_zeroes_names_its_range_and_deallocate_bit() {
    let (h, manual) = setup(E512);
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, write_zeroes(9, 3, true), 1);
    submit_on(&h, 1, SQ1, &mut tail, write_zeroes(16, 8, false), 2);
    let reqs = manual.take();
    assert_eq!(reqs.len(), 2);
    assert_eq!(
        reqs[0].op(),
        BlockOp::WriteZeroes {
            deallocate: true,
            fua: false
        }
    );
    assert_eq!(reqs[0].zero_ranges(), &[(9 * 512, 3 * 512)]);
    assert_eq!(
        reqs[1].op(),
        BlockOp::WriteZeroes {
            deallocate: false,
            fua: false
        }
    );
    assert_eq!(reqs[1].zero_ranges(), &[(16 * 512, 8 * 512)]);
}

#[test]
fn ranges_past_the_end_and_unoffered_commands_are_refused() {
    let (h, manual) = setup(E512);
    put_ranges(&h, &[(0, 1), (BACKING_BLOCKS - 1, 2)]);
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, dsm(2, true), 1);
    assert_eq!(
        h.wait_cqe(CQ1, 0, true).status(),
        Status::of(SC_LBA_OUT_OF_RANGE | SC_DNR)
    );
    submit_on(
        &h,
        1,
        SQ1,
        &mut tail,
        write_zeroes(BACKING_BLOCKS - 1, 2, false),
        2,
    );
    assert_eq!(
        h.wait_cqe(CQ1, 1, true).status(),
        Status::of(SC_INVALID_FIELD | SC_DNR)
    );
    assert!(manual.submitted_queues().is_empty());

    // A backend that does not take them is not offered them.
    let plain = NvmeConfig {
        deallocate: false,
        ..E512
    };
    let (h, manual) = setup(plain);
    put_ranges(&h, &[(0, 1)]);
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, dsm(1, true), 1);
    submit_on(&h, 1, SQ1, &mut tail, write_zeroes(0, 1, false), 2);
    for i in 0..2 {
        assert_eq!(
            h.wait_cqe(CQ1, i, true).status(),
            Status::of(SC_INVALID_OPCODE | SC_DNR)
        );
    }
    assert!(manual.submitted_queues().is_empty());
}

#[test]
fn a_512e_namespace_reports_its_physical_block_and_oncs() {
    let (mut h, _manual) = setup(E512);
    let ns = {
        let cqe = h.admin(Sqe {
            opcode: ADMIN_OPC_IDENTIFY,
            nsid: 1,
            prp1: 0x20000,
            cdw10: u32::from(IDENTIFY_CNS_NAMESPACE),
            ..Sqe::default()
        });
        assert_eq!(cqe.status(), Status::SUCCESS);
        h.read_guest(0x20000, PAGE_SIZE)
    };
    assert_eq!((ns[24], ns[33]), (0x12, 0x09));
    assert_eq!(u16::from_le_bytes([ns[36], ns[37]]), 7, "NAWUPF");
    assert_eq!(u16::from_le_bytes([ns[64], ns[65]]), 7, "NPWG");
    let cqe = h.admin(Sqe {
        opcode: ADMIN_OPC_IDENTIFY,
        nsid: 0,
        prp1: 0x20000,
        cdw10: u32::from(IDENTIFY_CNS_CONTROLLER),
        ..Sqe::default()
    });
    assert_eq!(cqe.status(), Status::SUCCESS);
    let ctrl = h.read_guest(0x20000, PAGE_SIZE);
    assert_eq!(u16::from_le_bytes([ctrl[520], ctrl[521]]), 0x0c, "ONCS");
    assert_eq!(ctrl[525], 0, "VWC");
}

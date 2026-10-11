// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Crate-boundary surface for vmm_storage.

use std::path::Path;
use std::sync::Arc;

use vmm_devices::pci::device::PciDevice;
use vmm_devices::{FlushError, FlushIntent, Lifecycle};
use vmm_storage::ahci::media::IsoMedia;
use vmm_storage::ahci::AhciCtrl;
use vmm_storage::nvme::backend::{
    AsyncBackend, BackendFactory, BlockRequest, BlockStatus, CompletionSink,
    RequestId, Segments,
};
use vmm_storage::nvme::{LbaSize, MsiPolicy, NvmeConfig, NvmeController};

fn _coerce_ahci(
    dev: Arc<AhciCtrl>,
) -> (Arc<dyn PciDevice>, Arc<dyn Lifecycle>) {
    (dev.clone(), dev)
}

fn _coerce_nvme(
    dev: Arc<NvmeController>,
) -> (Arc<dyn PciDevice>, Arc<dyn Lifecycle>) {
    (dev.clone(), dev)
}

/// Uses only public items, as a ring backend outside this crate must.
struct OutsideBackend {
    sink: Arc<dyn CompletionSink>,
}

impl AsyncBackend for OutsideBackend {
    fn capacity(&self) -> u64 {
        1 << 20
    }

    fn queue_depth(&self) -> u16 {
        64
    }

    fn durable_writes(&self) -> bool {
        true
    }

    fn submit(&self, req: BlockRequest) {
        let _ = (req.op(), req.lba(), req.blocks(), req.byte_offset());
        let _ = (req.id().queue(), req.id().tag(), req.data().is_live());
        let segs: Segments = req.data().segments().unwrap_or_default();
        let mut head = [0u8; 8];
        let _ = req.data().copy_from_guest_at(&segs, 0, &mut head);
        let _ = req.data().copy_to_guest_at(&segs, segs.len(), &[]);
        self.sink.complete(&[(req.id(), BlockStatus::Success)]);
    }

    fn flush(&self, _intent: FlushIntent) -> Result<(), FlushError> {
        Ok(())
    }

    fn halt(&self) {}
}

struct NoSink;

impl CompletionSink for NoSink {
    fn complete(&self, _done: &[(RequestId, BlockStatus)]) {}
}

#[test]
fn a_backend_from_outside_the_crate_fits_the_interface() {
    let factory: BackendFactory = Box::new(|sink| {
        Arc::new(OutsideBackend { sink }) as Arc<dyn AsyncBackend>
    });
    let backend = factory(Arc::new(NoSink));
    let config = NvmeConfig {
        lba_size: LbaSize::B4096,
        volatile_write_cache: false,
        msi_policy: MsiPolicy::PerBatch,
        deallocate: false,
        hints: None,
    };

    assert_eq!(backend.max_queues(), 15);
    assert!(backend.durable_writes() || config.volatile_write_cache);
    assert_eq!(backend.capacity() / u64::from(config.lba_size.bytes()), 256);
}

#[test]
fn iso_media_open_reports_a_missing_path() {
    // IsoMedia has no Debug, so expect_err does not compile here.
    let err = match IsoMedia::open(Path::new("/nonexistent/split-probe.iso")) {
        Ok(_) => panic!("a missing ISO must be an error"),
        Err(err) => err,
    };
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

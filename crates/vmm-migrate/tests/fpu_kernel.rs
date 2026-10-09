// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The XSAVE path against a real `/dev/vmm`: the layout the kernel
//! describes, an area it fills, and an area it takes back. Needs illumos
//! and the privilege to create a VM.
#![cfg(target_os = "illumos")]

use std::sync::Arc;

use bhyve_api::VmmCtlFd;
use vmm_core::hdl::{CreateOpts, VmmHdl};
use vmm_core::vcpu::Vcpu;
use vmm_migrate::xsave::{XsaveLayout, XsavePlan};

/// The legacy region's XMM0.
const XMM0: std::ops::Range<usize> = 160..176;
const XSTATE_BV: std::ops::Range<usize> = 512..520;
const SSE: u64 = 1 << 1;

/// Destroys the instance when the test ends, also after a panic.
struct Reaper(String);

impl Drop for Reaper {
    fn drop(&mut self) {
        if let Ok(ctl) = VmmCtlFd::open() {
            let _ = ctl.vm_destroy(self.0.as_bytes());
        }
    }
}

#[test]
fn an_xsave_area_survives_get_translate_and_set() {
    let name = format!("vmmmigrate-test-fpu-{}", std::process::id());
    let _reaper = Reaper(name.clone());
    let opts = CreateOpts {
        force: true,
        ..Default::default()
    };
    let hdl = Arc::new(
        VmmHdl::create(&name, &opts).expect("test host must allow VMs"),
    );
    let vcpu = Vcpu::new_for_thread(0, Arc::clone(&hdl));
    vcpu.activate().expect("activate vcpu 0");

    let layout = XsaveLayout::local(&hdl).expect("the host layout");
    let mut area = vcpu.get_fpu(layout.size()).expect("VM_GET_FPU");
    assert_eq!(area.len(), layout.size());

    area[XMM0].fill(0xA5);
    let bv = u64::from_le_bytes(area[XSTATE_BV].try_into().unwrap()) | SSE;
    area[XSTATE_BV].copy_from_slice(&bv.to_le_bytes());

    let plan = XsavePlan {
        source: &layout,
        local: &layout,
        guest: 0x3,
    };
    plan.check().expect("a host holds its own layout");
    let local = plan.translate(0, &area).expect("translate");
    vcpu.set_fpu(&local)
        .expect("VM_SET_FPU takes a translated area");

    let back = vcpu.get_fpu(layout.size()).expect("VM_GET_FPU again");
    assert_eq!(&back[XMM0], &[0xA5; 16]);
}

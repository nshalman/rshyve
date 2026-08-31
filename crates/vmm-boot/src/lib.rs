// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest boot protocols shared by every binary in this workspace.

pub mod bytes;
pub mod direct;
pub mod elf;
pub mod image;
pub mod initrd;
pub mod pvh;

// Cross-crate callers name these at the crate root. Re-export here so
// the module layout inside this crate can change later without
// breaking any of them.
pub use bytes::ImageBytes;
pub use direct::{
    load_kernel, setup_direct_boot_bsp, InitrdImage, KernelImage,
};
pub use elf::PvhKernel;
pub use image::{detect_boot_protocol, BootImage, BootProtocol};
pub use initrd::{place_initrd, LOWMEM_LIMIT};
pub use pvh::{load_pvh_kernel, setup_pvh_bsp};

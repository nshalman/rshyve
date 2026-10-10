// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The notify addresses the transport gives its backend follow what
//! the guest decodes.

use super::*;

use vmm_devices::pci::bits::RegCmd;

const PORT: u16 = 0x2000;
const WINDOW: u64 = 0xE000_0000;

fn sent(dev: &VirtioPciDevice<TrackingVirtioDevice>) -> Vec<(u16, u64)> {
    dev.device
        .state
        .lock()
        .expect("tracking state lock")
        .notify_addrs
        .clone()
}

fn decode(dev: &VirtioPciDevice<TrackingVirtioDevice>, cmd: RegCmd) {
    dev.cfg_write(0x04, 2, u32::from(cmd.bits()));
}

fn place(dev: &VirtioPciDevice<TrackingVirtioDevice>, port: u16, window: u64) {
    dev.cfg_write(0x10, 4, u32::from(port));
    dev.cfg_write(0x18, 4, window as u32);
}

fn notify_at(port: u16, window: u64) -> (u16, u64) {
    (
        port + bits::LEGACY_REG_QUEUE_NOTIFY,
        window + u64::from(bits::MODERN_BAR_NOTIFY_OFFSET),
    )
}

#[test]
fn the_backend_learns_the_notify_addresses_when_decoding_starts() {
    let dev = make_tracking_device();
    place(&dev, PORT, WINDOW);
    assert_eq!(sent(&dev), [], "nothing is decoded yet");

    decode(&dev, RegCmd::IO_EN | RegCmd::MMIO_EN);

    assert_eq!(sent(&dev), [notify_at(PORT, WINDOW)]);
}

#[test]
fn a_bar_move_moves_the_notify_address() {
    // A hook left behind takes kicks meant for whatever the guest puts
    // at the old address next.
    let dev = make_tracking_device();
    place(&dev, PORT, WINDOW);
    decode(&dev, RegCmd::IO_EN | RegCmd::MMIO_EN);

    dev.cfg_write(0x10, 4, u32::from(PORT + 0x100));

    assert_eq!(
        sent(&dev),
        [notify_at(PORT, WINDOW), notify_at(PORT + 0x100, WINDOW)],
    );
}

#[test]
fn decoding_off_takes_the_notify_address_away() {
    let dev = make_tracking_device();
    place(&dev, PORT, WINDOW);
    decode(&dev, RegCmd::IO_EN | RegCmd::MMIO_EN);

    decode(&dev, RegCmd::MMIO_EN);
    decode(&dev, RegCmd::empty());

    let (_, mmio) = notify_at(PORT, WINDOW);
    assert_eq!(sent(&dev), [notify_at(PORT, WINDOW), (0, mmio), (0, 0)],);
}

#[test]
fn a_write_that_moves_nothing_is_not_passed_on() {
    // A guest sizing a BAR with decoding off, or writing the command
    // register again, costs the backend nothing.
    let dev = make_tracking_device();
    place(&dev, PORT, WINDOW);
    decode(&dev, RegCmd::IO_EN | RegCmd::MMIO_EN);

    decode(&dev, RegCmd::IO_EN | RegCmd::MMIO_EN);
    dev.cfg_write(0x3C, 1, 11);

    assert_eq!(sent(&dev), [notify_at(PORT, WINDOW)]);
}

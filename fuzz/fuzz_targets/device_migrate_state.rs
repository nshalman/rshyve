// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A device state from an untrusted source must decode or fail, never
//! panic, and what decodes must survive a second trip.

#![no_main]

use libfuzzer_sys::fuzz_target;
use vmm_devices::lifecycle::DeviceMigrateState;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(state) = ron::de::from_str::<DeviceMigrateState>(text) else {
        return;
    };
    let again = ron::ser::to_string(&state).expect("a decoded state encodes");
    let back: DeviceMigrateState =
        ron::de::from_str(&again).expect("an encoded state decodes");
    assert_eq!(back, state);
});

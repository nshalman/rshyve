// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The wire decoders: a frame from the peer, and the payloads it
//! carries, must decode or fail, never panic.

#![no_main]

use libfuzzer_sys::fuzz_target;
use vmm_migrate::codec::{
    DeviceState, Message, MigrationPreamble, ProtocolOffer, TimeData,
};

fuzz_target!(|data: &[u8]| {
    let frame = tungstenite::Message::Binary(data.to_vec());
    if let Ok(Message::Serialized(payload)) = Message::decode(frame) {
        let _ = Message::deserialize::<DeviceState>(&payload);
        let _ = Message::deserialize::<MigrationPreamble>(&payload);
        let _ = Message::deserialize::<ProtocolOffer>(&payload);
        let _ = Message::deserialize::<TimeData>(&payload);
    }
    let _ = Message::deserialize::<DeviceState>(data);
});

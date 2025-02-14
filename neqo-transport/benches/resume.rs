// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A simulated transfer benchmark, asserting a minimum bandwidth.
//!
//! This is using [`test_fixture::sim`], i.e. does no I/O beyond the process
//! boundary and runs in simulated time. Given that [`test_fixture::sim`] is
//! deterministic, there is no need for multiple benchmark iterations. Still it
//! is a Rust benchmark instead of a unit test due to its runtime (> 10s) even
//! in Rust release mode.
use std::time::Duration;

use neqo_transport::{ConnectionParameters, SavedParameters, State, StreamType};
use test_fixture::{
    boxed,
    sim::{
        connection::{ConnectionNode, ReachState, ReceiveData, SendData},
        network::{Delay, Mtu, TailDrop},
        Simulator,
    },
};

#[allow(clippy::cast_precision_loss)]
pub fn main() {
    const MIB: usize = 1_024 * 1_024;

    const MBIT: usize = 1_000 * 1_000;

    const TRANSFER_AMOUNT: usize = 50 * MIB;

    const LINK_BANDWIDTH_DOWN: usize = 50 * MBIT;
    const LINK_BANDWIDTH_UP: usize = 5 * MBIT;
    const LINK_RTT_MS: usize = 600;

    const LINK_BDP: usize = LINK_BANDWIDTH_DOWN * LINK_RTT_MS / 1_000 / 8;

    const MINIMUM_EXPECTED_UTILIZATION: f64 = 0.5;

    let geo_sat_uplink = || {
        let rate_byte = LINK_BANDWIDTH_UP / 8;
        // Router buffer set to bandwidth-delay product.
        let capacity_byte = rate_byte * LINK_RTT_MS / 1_000;
        TailDrop::new(rate_byte, capacity_byte, Duration::ZERO)
    };

    let geo_sat_downlink = || {
        let rate_byte = LINK_BANDWIDTH_DOWN / 8;
        // Router buffer set to bandwidth-delay product.
        let capacity_byte = rate_byte * LINK_RTT_MS / 1_000;
        TailDrop::new(rate_byte, capacity_byte, Duration::ZERO)
    };
    let saved_rtt = SavedParameters {
        rtt: Duration::from_millis(LINK_RTT_MS as u64),
        cwnd: LINK_BDP,
        enabled: false,
    };

    let saved_parameters = SavedParameters {
        rtt: Duration::from_millis(LINK_RTT_MS as u64),
        cwnd: LINK_BDP,
        enabled: true,
    };

    let simulated_time = Simulator::new(
        "resume",
        boxed![
            ConnectionNode::new_client(
                ConnectionParameters::default()
                    .pacing(false)
                    .careful_resume(Some(saved_rtt))
                    .max_stream_data(StreamType::BiDi, true, 200_000_000)
                    .max_stream_data(StreamType::BiDi, false, 200_000_000)
                    .max_stream_data(StreamType::UniDi, true, 200_000_000),
                boxed![ReachState::new(State::Confirmed)],
                boxed![ReceiveData::new(TRANSFER_AMOUNT)]
            ),
            Mtu::new(1500),
            geo_sat_uplink(),
            Delay::new(Duration::from_millis(LINK_RTT_MS as u64 / 2)),
            ConnectionNode::new_server(
                ConnectionParameters::default()
                    .careful_resume(Some(saved_parameters))
                    .max_stream_data(StreamType::BiDi, true, 200_000_000)
                    .max_stream_data(StreamType::BiDi, false, 200_000_000)
                    .max_stream_data(StreamType::UniDi, true, 200_000_000),
                boxed![ReachState::new(State::Confirmed)],
                boxed![SendData::new(TRANSFER_AMOUNT)]
            ),
            Mtu::new(1500),
            geo_sat_downlink(),
            Delay::new(Duration::from_millis(LINK_RTT_MS as u64 / 2)),
        ],
    )
    .setup()
    .run();

    println!("sim time {}", simulated_time.as_secs_f64());
    println!(
        "[link] down {}, up {}, rtt {}, bdp {}",
        LINK_BANDWIDTH_DOWN / MBIT,
        LINK_BANDWIDTH_UP / MBIT,
        LINK_RTT_MS,
        LINK_BDP
    );

    let achieved_bandwidth = TRANSFER_AMOUNT as f64 * 8.0 / simulated_time.as_secs_f64();

    assert!(
        LINK_BANDWIDTH_DOWN as f64 * MINIMUM_EXPECTED_UTILIZATION < achieved_bandwidth,
        "expected to reach {MINIMUM_EXPECTED_UTILIZATION} of maximum bandwidth ({} Mbit/s) but got {} Mbit/s",
        LINK_BANDWIDTH_DOWN / MBIT,
        achieved_bandwidth / MBIT as f64,
    );
}

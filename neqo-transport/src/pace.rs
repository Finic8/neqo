// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Pacer

use std::{
    fmt::{Debug, Display},
    time::{Duration, Instant},
};

use neqo_common::{qdebug, qinfo};

/// This value determines how much faster the pacer operates than the
/// congestion window.
///
/// A value of 1 would cause all packets to be spaced over the entire RTT,
/// which is a little slow and might act as an additional restriction in
/// the case the congestion controller increases the congestion window.
/// This value spaces packets over half the congestion window, which matches
/// our current congestion controller, which double the window every RTT.
const PACER_SPEEDUP: usize = 1;

/// The number of packets we allow to burst from the pacer.
pub const PACING_BURST_SIZE: usize = 10;

/// A pacer that uses a leaky bucket.
pub struct Pacer {
    /// Whether pacing is enabled.
    enabled: bool,
    /// The last update time.
    last_update: Instant,

    next_time: Instant,
    /// The maximum capacity, or burst size, in bytes.
    capacity: usize,
    /// The current used capacity, in bytes.
    used: usize,
    /// The packet size or minimum capacity for sending, in bytes.
    mtu: usize,

    last_packet_size: Option<usize>,
}

impl Pacer {
    /// Create a new `Pacer`.  This takes the current time, the maximum burst size,
    /// and the packet size.
    ///
    /// The value of `m` is the maximum capacity in bytes.  `m` primes the pacer
    /// with credit and determines the burst size.  `m` must not exceed
    /// the initial congestion window, but it should probably be lower.
    ///
    /// The value of `p` is the packet size in bytes, which determines the minimum
    /// credit needed before a packet is sent.  This should be a substantial
    /// fraction of the maximum packet size, if not the packet size.
    pub fn new(enabled: bool, now: Instant, mtu: usize) -> Self {
        assert!(0 < mtu, "mtu must be larger than 0");
        Self {
            enabled,
            last_update: now,
            next_time: now,
            capacity: PACING_BURST_SIZE * mtu,
            used: 0,
            mtu,
            last_packet_size: None,
        }
    }

    pub const fn mtu(&self) -> usize {
        self.mtu
    }

    pub fn set_mtu(&mut self, mtu: usize) {
        self.mtu = mtu;
        self.capacity = 10 * mtu;
    }

    /// Determine when the next packet will be available based on the provided RTT
    /// and congestion window.  This doesn't update state.
    /// This returns a time, which could be in the past (this object doesn't know what
    /// the current time is).
    pub fn next(&self, _rtt: Duration, _cwnd: usize) -> Instant {
        if !self.enabled {
            return self.last_update;
        }
        self.next_time
    }

    /// Spend credit.  This cannot fail; users of this API are expected to call
    /// `next()` to determine when to spend.  This takes the current time (`now`),
    /// an estimate of the round trip time (`rtt`), the estimated congestion
    /// window (`cwnd`), and the number of bytes that were sent (`count`).
    pub fn spend(&mut self, now: Instant, rtt: Duration, cwnd: usize, bytes: usize) {
        if !self.enabled {
            self.last_update = now;
            return;
        }
        let rate = ((8 * cwnd) as f64 / rtt.as_secs_f64()) / 1_000.0;
        qinfo!(
            "[{self}] spending bytes: {bytes} rtt {rtt:?}, cwnd: {cwnd}, rate: {rate:.3} kBit/s"
        );

        // time to send burst capacity of data
        //  capacity         rtt
        // ---------- * ---------------
        //   cwnd        PACER_SPEEDUP
        let burst_duration = u64::try_from(
            rtt.as_nanos().saturating_mul(self.capacity as u128)
                / u128::try_from(cwnd * PACER_SPEEDUP).expect("usize fits into u128"),
        )
        .map(Duration::from_nanos)
        .unwrap_or(rtt);

        let elapsed = now.saturating_duration_since(self.last_update);
        if elapsed > burst_duration {
            qdebug!(
                "[{self}] elapesd: {elapsed:?} > burst_duration {burst_duration:?} -> resetting"
            );
            self.used = 0;
            self.last_update = now;
            self.next_time = now;
            self.last_packet_size = None;
        }

        self.used += bytes;

        let same_size = self.last_packet_size.map_or(true, |last| last == bytes);
        if bytes != 0 {
            self.last_packet_size = Some(bytes);
        }

        if self.used >= self.capacity || !same_size {
            let delay = u64::try_from(
                rtt.as_nanos().saturating_mul(self.used as u128)
                    / u128::try_from(cwnd * PACER_SPEEDUP).expect("usize fits into u128"),
            )
            .map(Duration::from_nanos)
            .unwrap_or(rtt);

            self.used = 0;
            self.next_time = now + delay;
            self.last_update = now;
            self.last_packet_size = None;
            qdebug!(
                "[{self}] waiting for: {:?}",
                self.next_time.saturating_duration_since(now)
            );
        }
    }
}

impl Display for Pacer {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Pacer {}/{}", self.used, self.capacity)
    }
}

impl Debug for Pacer {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "Pacer@{:?} {}/{}..{}",
            self.last_update, self.used, self.mtu, self.capacity
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use test_fixture::now;

    use super::Pacer;

    const RTT: Duration = Duration::from_millis(1000);
    const PACKET: usize = 1000;
    const CWND: usize = PACKET * 10;

    #[test]
    fn even() {
        let n = now();
        let mut p = Pacer::new(true, n, PACKET);
        assert_eq!(p.next(RTT, CWND), n);
        p.spend(n, RTT, CWND, PACKET);
        assert_eq!(p.next(RTT, CWND), n + (RTT / 20));
    }

    #[test]
    fn backwards_in_time() {
        let n = now();
        let mut p = Pacer::new(true, n + RTT, PACKET);
        assert_eq!(p.next(RTT, CWND), n + RTT);
        // Now spend some credit in the past using a time machine.
        p.spend(n, RTT, CWND, PACKET);
        assert_eq!(p.next(RTT, CWND), n + (RTT / 20));
    }

    #[test]
    fn pacing_disabled() {
        let n = now();
        let mut p = Pacer::new(false, n, PACKET);
        assert_eq!(p.next(RTT, CWND), n);
        p.spend(n, RTT, CWND, PACKET);
        assert_eq!(p.next(RTT, CWND), n);
    }

    #[test]
    fn send_immediately_below_granularity() {
        const SHORT_RTT: Duration = Duration::from_millis(10);
        let n = now();
        let mut p = Pacer::new(true, n, PACKET);
        assert_eq!(p.next(SHORT_RTT, CWND), n);
        p.spend(n, SHORT_RTT, CWND, PACKET);
        assert_eq!(
            p.next(SHORT_RTT, CWND),
            n,
            "Expect packet to be sent immediately, instead of being paced below timer granularity"
        );
    }
}

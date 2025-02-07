// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Pacer

use std::{
    cmp::min,
    fmt::{Debug, Display},
    time::{Duration, Instant},
};

use neqo_common::qwarn;

use crate::rtt::GRANULARITY;

/// This value determines how much faster the pacer operates than the
/// congestion window.
///
/// A value of 1 would cause all packets to be spaced over the entire RTT,
/// which is a little slow and might act as an additional restriction in
/// the case the congestion controller increases the congestion window.
/// This value spaces packets over half the congestion window, which matches
/// our current congestion controller, which double the window every RTT.
const PACER_SPEEDUP: usize = 1;

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

    iv: Duration,

    start_time: Instant,
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
    pub fn new(enabled: bool, now: Instant, m: usize, p: usize) -> Self {
        assert!(m >= p, "maximum capacity has to be at least one packet");
        Self {
            enabled,
            last_update: now,
            next_time: now,
            capacity: 10 * p,
            used: 0,
            mtu: p,
            last_packet_size: None,
            iv: Duration::ZERO,
            start_time: now,
        }
    }

    pub const fn mtu(&self) -> usize {
        self.mtu
    }

    pub fn set_mtu(&mut self, mtu: usize) {
        self.mtu = mtu;
    }

    /// Determine when the next packet will be available based on the provided RTT
    /// and congestion window.  This doesn't update state.
    /// This returns a time, which could be in the past (this object doesn't know what
    /// the current time is).
    pub const fn next(&self, _rtt: Duration, _cwnd: usize) -> Instant {
        if !self.enabled {
            return self.last_update;
        }
        self.next_time
        /*

        if self.used >= self.mtu {
            qwarn!(
                "[{self}] next {cwnd}/{rtt:?} no wait = {:?}",
                self.last_update
            );
            return self.last_update;
        }

        // This is the inverse of the function in `spend`:
        // self.t + rtt * (self.p - self.c) / (PACER_SPEEDUP * cwnd)
        let r = rtt.as_nanos();
        let d =
            r.saturating_mul(u128::try_from(self.mtu - self.used).expect("usize fits into u128"));
        let add = d / u128::try_from(cwnd * PACER_SPEEDUP).expect("usize fits into u128");
        let w = u64::try_from(add).map(Duration::from_nanos).unwrap_or(rtt);

        // If the increment is below the timer granularity, send immediately.
        if w < GRANULARITY {
            qwarn!("[{self:?}] next {cwnd}/{rtt:?} below granularity ({w:?})",);
            return self.last_update;
        }

        let nxt = self.last_update + w;
        qwarn!("[{self}] next {cwnd}/{rtt:?} wait {w:?} = {nxt:?}");
        nxt
        */
    }

    /// Spend credit.  This cannot fail; users of this API are expected to call
    /// `next()` to determine when to spend.  This takes the current time (`now`),
    /// an estimate of the round trip time (`rtt`), the estimated congestion
    /// window (`cwnd`), and the number of bytes that were sent (`count`).
    pub fn spend(&mut self, now: Instant, rtt: Duration, cwnd: usize, count: usize) {
        if !self.enabled {
            self.last_update = now;
            return;
        }
        qwarn!(
            "\nTIME passed: {:?}, count {count} rtt {rtt:?}, cwnd: {cwnd}",
            now.saturating_duration_since(self.start_time)
        );

        if !self.iv.is_zero() {
            qwarn!("[{self}] -> {:?}", self.iv);
            self.next_time = self.next_time.max(now) + self.iv;
            self.iv = Duration::ZERO;
        }

        let cwnd_interval = u64::try_from(
            rtt.as_nanos().saturating_mul(self.capacity as u128)
                / u128::try_from(cwnd * PACER_SPEEDUP).expect("usize fits into u128"),
        )
        .map(Duration::from_nanos)
        .unwrap_or(rtt);

        let elapsed = now.saturating_duration_since(self.last_update);
        qwarn!("[{self}] {:?} {:?}", elapsed, cwnd_interval);
        if elapsed > cwnd_interval {
            qwarn!("elapesd > cwnd_interval: resetting");
            self.used = 0;
            self.last_update = now;
            self.next_time = self.next_time.max(now);
            self.last_packet_size = None;
            self.iv = Duration::ZERO;
        }

        self.used += count;

        let same_size = self.last_packet_size.map_or(true, |last| last == count);
        self.last_packet_size = Some(count);

        if self.used >= self.capacity || !same_size {
            qwarn!("used > cap || same_size {:?}", same_size);
            let interval = u64::try_from(
                rtt.as_nanos().saturating_mul(self.used as u128)
                    / u128::try_from(cwnd * PACER_SPEEDUP).expect("usize fits into u128"),
            )
            .map(Duration::from_nanos)
            .unwrap_or(rtt);
            self.iv = interval;

            self.used = 0;
            self.last_update = now;
            self.last_packet_size = None;
        }

        /*
                qwarn!("[{self}] spend {count} over {cwnd}, {rtt:?}");
                // Increase the capacity by:
                //    `(now - self.t) * PACER_SPEEDUP * cwnd / rtt`
                // That is, the elapsed fraction of the RTT times rate that data is added.
                let incr = now
                    .saturating_duration_since(self.last_update)
                    .as_nanos()
                    .saturating_mul(u128::try_from(cwnd * PACER_SPEEDUP).expect("usize fits into u128"))
                    .checked_div(rtt.as_nanos())
                    .and_then(|i| usize::try_from(i).ok())
                    .unwrap_or(self.capacity);

                // Add the capacity up to a limit of `self.m`, then subtract `count`.
                self.used = min(self.capacity, (self.used + incr).saturating_sub(count));
                self.last_update = now;
        */
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
        let mut p = Pacer::new(true, n, PACKET, PACKET);
        assert_eq!(p.next(RTT, CWND), n);
        p.spend(n, RTT, CWND, PACKET);
        assert_eq!(p.next(RTT, CWND), n + (RTT / 20));
    }

    #[test]
    fn backwards_in_time() {
        let n = now();
        let mut p = Pacer::new(true, n + RTT, PACKET, PACKET);
        assert_eq!(p.next(RTT, CWND), n + RTT);
        // Now spend some credit in the past using a time machine.
        p.spend(n, RTT, CWND, PACKET);
        assert_eq!(p.next(RTT, CWND), n + (RTT / 20));
    }

    #[test]
    fn pacing_disabled() {
        let n = now();
        let mut p = Pacer::new(false, n, PACKET, PACKET);
        assert_eq!(p.next(RTT, CWND), n);
        p.spend(n, RTT, CWND, PACKET);
        assert_eq!(p.next(RTT, CWND), n);
    }

    #[test]
    fn send_immediately_below_granularity() {
        const SHORT_RTT: Duration = Duration::from_millis(10);
        let n = now();
        let mut p = Pacer::new(true, n, PACKET, PACKET);
        assert_eq!(p.next(SHORT_RTT, CWND), n);
        p.spend(n, SHORT_RTT, CWND, PACKET);
        assert_eq!(
            p.next(SHORT_RTT, CWND),
            n,
            "Expect packet to be sent immediately, instead of being paced below timer granularity"
        );
    }
}

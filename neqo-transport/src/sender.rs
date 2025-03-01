// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Congestion control

use std::{
    fmt::{self, Display},
    time::{Duration, Instant},
};

use neqo_common::{qdebug, qlog::NeqoQlog, qwarn};

use crate::{
    cc::{ClassicCongestionControl, CongestionControl, CongestionControlAlgorithm, Cubic, NewReno},
    hystartpp::HystartPP,
    pace::Pacer,
    pmtud::Pmtud,
    recovery::SentPacket,
    resume::{Resume, SavedParameters},
    rtt::RttEstimate,
    Stats,
};

#[derive(Debug)]
pub struct PacketSender {
    cc: Box<dyn CongestionControl>,
    pacer: Pacer,
    resume: Resume,
}

impl Display for PacketSender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} {}", self.cc, self.pacer)
    }
}

impl PacketSender {
    #[must_use]
    pub fn new(
        alg: CongestionControlAlgorithm,
        pacing_enabled: bool,
        resume: Option<&SavedParameters>,
        pmtud: Pmtud,
        now: Instant,
    ) -> Self {
        let mtu = pmtud.plpmtu();

        let hystart = match std::env::var_os("ENABLE_HYSTART") {
            Some(_) => HystartPP::new(),
            None => HystartPP::disabled(),
        };

        let mut cc: Box<dyn CongestionControl> = match alg {
            CongestionControlAlgorithm::NewReno => {
                Box::new(ClassicCongestionControl::new(NewReno::default(), pmtud))
            }
            CongestionControlAlgorithm::Cubic => {
                Box::new(ClassicCongestionControl::new(Cubic::default(), pmtud))
            }
        };
        cc.set_hystart(hystart);

        Self {
            cc,
            pacer: Pacer::new(pacing_enabled, now, mtu),
            resume: resume
                .copied()
                .map_or_else(Resume::disabled, Resume::with_paramters),
        }
    }

    pub fn set_qlog(&mut self, qlog: NeqoQlog) {
        self.resume.set_qlog(qlog.clone());
        self.cc.set_qlog(qlog);
    }

    pub fn pmtud(&self) -> &Pmtud {
        self.cc.pmtud()
    }

    pub fn pmtud_mut(&mut self) -> &mut Pmtud {
        self.cc.pmtud_mut()
    }

    #[must_use]
    pub fn cwnd(&self) -> usize {
        self.cc.cwnd()
    }

    #[must_use]
    pub fn cwnd_avail(&self) -> usize {
        self.cc.cwnd_avail()
    }

    #[cfg(test)]
    #[must_use]
    pub fn cwnd_min(&self) -> usize {
        self.cc.cwnd_min()
    }

    fn maybe_update_pacer_mtu(&mut self) {
        let current_mtu = self.pmtud().plpmtu();
        if current_mtu != self.pacer.mtu() {
            qdebug!(
                "PLPMTU changed from {} to {current_mtu}, updating pacer",
                self.pacer.mtu()
            );
            self.pacer.set_mtu(current_mtu);
        }
    }

    pub fn on_packets_acked(
        &mut self,
        acked_pkts: &[SentPacket],
        rtt_est: &RttEstimate,
        now: Instant,
        stats: &mut Stats,
    ) {
        for ack in acked_pkts {
            let (next_cwnd, next_sshthresh) = self.resume.on_ack(
                ack,
                rtt_est.estimate(),
                self.cc.bytes_in_flight(),
                self.cc.cwnd(),
                self.cc.cwnd_initial(),
                now,
            );

            if let Some(next_cwnd) = next_cwnd {
                self.cc.set_cwnd(next_cwnd, now);
                // reset Pacer
                self.pacer.spend(now, rtt_est.estimate(), next_cwnd, 0);
            }
            if let Some(next_sshthresh) = next_sshthresh {
                self.cc.set_ssthresh(next_sshthresh);
            }
        }
        self.cc.on_packets_acked(acked_pkts, rtt_est, now);
        self.pmtud_mut().on_packets_acked(acked_pkts, now, stats);
        self.maybe_update_pacer_mtu();
    }

    /// Called when packets are lost.  Returns true if the congestion window was reduced.
    pub fn on_packets_lost(
        &mut self,
        first_rtt_sample_time: Option<Instant>,
        prev_largest_acked_sent: Option<Instant>,
        pto: Duration,
        lost_packets: &[SentPacket],
        stats: &mut Stats,
        now: Instant,
    ) -> bool {
        let ret = self.cc.on_packets_lost(
            first_rtt_sample_time,
            prev_largest_acked_sent,
            pto,
            lost_packets,
            now,
        );

        if ret {
            if let Some(next_cwnd) = self.resume.on_packetloss(now) {
                qdebug!("resume reduced cwnd to {next_cwnd}");
                self.cc.set_cwnd(next_cwnd, now);
            }
        }

        // Call below may change the size of MTU probes, so it needs to happen after the CC
        // reaction above, which needs to ignore probes based on their size.
        self.pmtud_mut().on_packets_lost(lost_packets, stats, now);
        self.maybe_update_pacer_mtu();
        ret
    }

    /// Called when ECN CE mark received.  Returns true if the congestion window was reduced.
    pub fn on_ecn_ce_received(&mut self, largest_acked_pkt: &SentPacket, now: Instant) -> bool {
        if let Some(next_cwnd) = self.resume.on_ecn(now) {
            qdebug!("resume reduced cwnd to {next_cwnd}");
            self.cc.set_cwnd(next_cwnd, now);
        }

        self.cc.on_ecn_ce_received(largest_acked_pkt, now)
    }

    pub fn discard(&mut self, pkt: &SentPacket, now: Instant) {
        self.cc.discard(pkt, now);
    }

    /// When we migrate, the congestion controller for the previously active path drops
    /// all bytes in flight.
    pub fn discard_in_flight(&mut self, now: Instant) {
        self.cc.discard_in_flight(now);
    }

    pub fn on_packet_sent(&mut self, pkt: &SentPacket, rtt: Duration, now: Instant) {
        if pkt.ack_eliciting() {
            self.pacer
                .spend(pkt.time_sent(), rtt, self.cc.cwnd(), pkt.len());
        } else {
            qwarn!("pkt is not ack eliciting");
        }
        self.cc.on_packet_sent(pkt, now);

        if let Some(jump) = self.resume.on_sent(
            self.cc.cwnd(),
            pkt.pn(),
            rtt,
            self.cc.bytes_in_flight(),
            false,
            now,
        ) {
            self.cc.set_cwnd(jump, now);
        }
    }

    #[must_use]
    pub fn next_paced(&self, rtt: Duration) -> Option<Instant> {
        // Only pace if there are bytes in flight.
        (self.cc.bytes_in_flight() > 0).then(|| self.pacer.next(rtt, self.cc.cwnd()))
    }

    #[must_use]
    pub fn recovery_packet(&self) -> bool {
        self.cc.recovery_packet()
    }
}

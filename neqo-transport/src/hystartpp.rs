use std::{
    fmt::Display,
    time::{Duration, Instant},
};

use neqo_common::{qerror, qinfo, qwarn};

use crate::recovery::SentPacket;

/// From RFC 9406:
/// The delay increase sensitivity is determined by `MIN_RTT_THRESH` and `MAX_RTT_THRESH`.
/// Smaller values of `MIN_RTT_THRESH` may cause spurious exits from slow start.
/// Larger values of `MAX_RTT_THRESH` may result in slow start not exiting
/// until loss is encountered for connections on large RTT paths.
const MIN_RTT_THRESH: Duration = Duration::from_millis(4);
const MAX_RTT_THRESH: Duration = Duration::from_millis(16);
/// `MIN_RTT_DIVISOR` is a fraction of RTT to compute the delay threshold.
/// A smaller value would mean a larger threshold and thus less sensitivity to delay increase, and vice versa.
const MIN_RTT_DIVISOR: u32 = 8;
/// While all TCP implementations are REQUIRED to take at least one RTT sample each round,
/// implementations of `HyStart++` are RECOMMENDED to take at least `N_RTT_SAMPLE` RTT samples.
/// Using lower values of `N_RTT_SAMPLE` will lower the accuracy of the measured RTT for the round;
/// higher values will improve accuracy at the cost of more processing.
const N_RTT_SAMPLE: usize = 8;
/// The minimum value of `CSS_GROWTH_DIVISOR` MUST be at least 2.
/// A value of 1 results in the same aggressive behavior as regular slow start.
/// Values larger than 4 will cause the algorithm to be less aggressive and maybe less performant.
const CSS_GROWTH_DIVISOR: usize = 4;
/// Smaller values of `CSS_ROUNDS` may miss detecting jitter, and larger values may limit performance.
const CSS_ROUNDS: usize = 5;
// const L = infinity if paced, L = 8 if non-paced

#[derive(Debug, Default)]
pub enum State {
    #[default]
    SlowStart,
    CSS {
        baseline_min_rtt: Duration,
        rounds: usize,
    },
    CongestionAvoidance,
}
impl Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SlowStart => write!(f, "SS"),
            Self::CSS {
                baseline_min_rtt,
                rounds,
            } => write!(f, "CSS round {rounds}: {baseline_min_rtt:?}"),
            Self::CongestionAvoidance => write!(f, "CA"),
        }
    }
}

#[derive(Debug, Default)]
pub struct HystartPP {
    enabled: bool,
    state: State,
    last_round_min_rtt: Duration,
    current_round_min_rtt: Duration,
    rtt_sample_count: usize,
    window_end: Option<u64>,
}

impl Display for HystartPP {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.enabled {
            write!(f, "Hystart++ {}", self.state)
        } else {
            write!(f, "Hystart++ disabled")
        }
    }
}

impl HystartPP {
    pub fn disabled() -> Self {
        Self::default()
    }
    pub fn new() -> Self {
        Self {
            enabled: true,
            state: State::default(),
            last_round_min_rtt: Duration::MAX,
            current_round_min_rtt: Duration::MAX,
            rtt_sample_count: 0,
            window_end: None,
        }
    }

    /// At the start of each round during standard slow start RFC5681 and CSS,
    /// initialize the variables used to compute the last round's and current round's minimum RTT:
    pub fn on_sent(&mut self, pkt_num: u64) {
        if !self.enabled || self.window_end.is_some() {
            return;
        }
        self.window_end = Some(pkt_num);
        self.last_round_min_rtt = self.current_round_min_rtt;
        self.current_round_min_rtt = Duration::MAX;
        self.rtt_sample_count = 0;
        qerror!("[{self}] start round: {pkt_num}");
    }

    pub fn on_ack(&mut self, ack: &SentPacket, rtt: Duration, now: Instant) {
        if !self.enabled {
            return;
        }

        self.rtt_sample_count += 1;
        self.current_round_min_rtt = self.current_round_min_rtt.min(rtt);
        qerror!(
            "[{self}] samples: {} {:?} current: {rtt:?}",
            self.rtt_sample_count,
            self.current_round_min_rtt
        );

        match self.state {
            State::SlowStart => {
                if self.window_end.is_some_and(|end_pkt| end_pkt <= ack.pn()) {
                    self.window_end = None;
                    qwarn!("[{self}] round finished {}", ack.pn());
                }
                // For rounds where at least N_RTT_SAMPLE RTT samples have been obtained
                // and currentRoundMinRTT and lastRoundMinRTT are valid,
                // check to see if delay increase triggers slow start exit:
                if self.rtt_sample_count >= N_RTT_SAMPLE
                    && self.current_round_min_rtt != Duration::MAX
                    && self.last_round_min_rtt != Duration::MAX
                {
                    let rtt_thresh = (self.last_round_min_rtt / MIN_RTT_DIVISOR)
                        .clamp(MIN_RTT_THRESH, MAX_RTT_THRESH);

                    qinfo!("[{self}] rtt_thresh {:?}", rtt_thresh);
                    qinfo!(
                        "curr {:?}, last {:?}, critical {:?}",
                        self.current_round_min_rtt,
                        self.last_round_min_rtt,
                        self.last_round_min_rtt + rtt_thresh
                    );
                    if self.current_round_min_rtt
                        >= self.last_round_min_rtt.saturating_add(rtt_thresh)
                    {
                        qerror!("[{self}] going to CSS");
                        self.state = State::CSS {
                            baseline_min_rtt: self.current_round_min_rtt,
                            // If the transition into CSS happens in the middle of a round,
                            // that partial round counts towards the limit.
                            rounds: self.window_end.is_some().into(),
                        };
                    }
                }
            }
            State::CSS {
                baseline_min_rtt,
                mut rounds,
            } => {
                //  For CSS rounds where at least N_RTT_SAMPLE RTT samples have been obtained,
                //  check to see if the current round's minRTT drops below baseline (cssBaselineMinRtt)
                //  indicating that slow start exit was spurious:
                if self.rtt_sample_count >= N_RTT_SAMPLE {
                    // TODO: quiche resets rtt_sample_count

                    if self.current_round_min_rtt < baseline_min_rtt {
                        qerror!("[{self}] going to SS");
                        self.state = State::SlowStart;
                    }
                }
                // If CSS_ROUNDS rounds are complete, enter congestion avoidance by setting the ssthresh to the current cwnd.
                if self.window_end.is_some_and(|end_pkt| end_pkt <= ack.pn()) {
                    self.window_end = None;
                    rounds += 1;
                    qwarn!("[{self}] round finished");

                    self.state = if rounds >= CSS_ROUNDS {
                        qerror!("[{self}] going to CA");
                        State::CongestionAvoidance
                    } else {
                        qerror!("[{self}] going to CSS");
                        State::CSS {
                            baseline_min_rtt,
                            rounds,
                        }
                    };
                }
            }
            State::CongestionAvoidance => {
                qerror!("[{self}]");
            }
        }
    }

    pub fn on_congestion(&mut self) {
        if !self.enabled {
            return;
        }
        qerror!("[{self}] going to CA");
        self.state = State::CongestionAvoidance;
    }

    pub fn cwnd_increase(&self, increase: usize, max_datagram_size: usize) -> usize {
        if !self.enabled {
            return increase;
        }

        match self.state {
            State::CSS { .. } => {
                qwarn!("[{self}] reducing cwnd increase");
                increase / CSS_GROWTH_DIVISOR
            }
            State::SlowStart => increase,
            State::CongestionAvoidance => increase,
        }
    }
}

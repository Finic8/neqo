use std::{
    time::{Duration, Instant},
    u64,
};

use neqo_common::{qdebug, qerror, qlog::NeqoQlog, qwarn};
use qlog::events::{
    resume::{CarefulResumePhase, CarefulResumeRestoredParameters, CarefulResumeTrigger},
    EventData,
};

use crate::recovery::SentPacket;

#[derive(Default, Debug, Copy, Clone, Eq, PartialEq)]
pub enum ResumeState {
    #[default]
    Reconnaissance,
    // The next two states store the first packet sent when entering that state
    Unvalidated(u64),
    Validating(u64),
    // Stores the last packet sent during the Unvalidated Phase
    SafeRetreat(u64),
    Normal,
}

#[derive(Debug)]
pub struct Resume {
    qlog: NeqoQlog,

    state: ResumeState,
    enable_safe_retreat: bool,

    pipesize: usize,
    largest_pkt_sent: u64,

    saved_rtt: Duration,
    saved_cwnd: usize,
}

impl Resume {
    pub fn new() -> Self {
        Self {
            qlog: NeqoQlog::disabled(),
            state: ResumeState::default(),
            enable_safe_retreat: true,
            pipesize: 0,
            largest_pkt_sent: 0,
            saved_rtt: Duration::from_millis(600),
            saved_cwnd: 3_750_000,
        }
    }

    pub fn set_qlog(&mut self, qlog: NeqoQlog) {
        self.qlog = qlog;
    }

    pub fn on_ack(
        &mut self,
        ack: &SentPacket,
        flightsize: usize,
        now: Instant,
    ) -> (Option<usize>, Option<usize>) {
        match self.state {
            ResumeState::Unvalidated(first_unvalidated_packet) => {
                self.pipesize += ack.len();
                if ack.pn() < first_unvalidated_packet {
                    return (None, None);
                }

                if self.pipesize < flightsize {
                    qerror!("CAREFULERESUME: next stage validating");
                    self.change_state(
                        ResumeState::Validating(self.largest_pkt_sent),
                        CarefulResumeTrigger::FirstUnvalidatedPacketAcknowledged,
                        now,
                    );
                    (Some(flightsize), None)
                } else {
                    qerror!("CAREFULERESUME: complete skipping validating");
                    self.change_state(
                        ResumeState::Normal,
                        CarefulResumeTrigger::FirstUnvalidatedPacketAcknowledged,
                        now,
                    );
                    (Some(self.pipesize), None)
                }
            }
            ResumeState::Validating(last_unvalidated_packet) => {
                self.pipesize += ack.len();

                if last_unvalidated_packet <= ack.pn() {
                    qerror!("CAREFULERESUME: complete going to normal");
                    self.change_state(
                        ResumeState::Normal,
                        CarefulResumeTrigger::LastUnvalidatedPacketAcknowledged,
                        now,
                    );
                }
                (None, None)
            }
            ResumeState::SafeRetreat(_) => todo!(),
            _ => (None, None),
        }
    }

    pub fn on_sent(
        &mut self,
        rtt: Duration,
        cwnd: usize,
        largest_pkt_sent: u64,
        app_limited: bool,
        iw_acked: bool,
        now: Instant,
    ) -> Option<usize> {
        self.largest_pkt_sent = largest_pkt_sent;

        if largest_pkt_sent == 0 {
            let event = EventData::CarefulResumePhaseUpdated(
                qlog::events::resume::CarefulResumePhaseUpdated {
                    old_phase: None,
                    new_phase: self.state.into(),
                    state_data: qlog::events::resume::CarefulResumeStateParameters {
                        pipesize: self.pipesize as u64,
                        first_unvalidated_packet: 0,
                        last_unvalidated_packet: 0,
                        congestion_window: Some(12345),
                        ssthresh: Some(u64::MAX),
                    },
                    restored_data: Some(CarefulResumeRestoredParameters {
                        saved_congestion_window: self.saved_cwnd as u64,
                        saved_rtt: self.saved_rtt.as_secs_f32() * 1000.0,
                    }),
                    trigger: None,
                },
            );

            qdebug!("Sending qlog");
            self.qlog.add_event_data_with_instant(|| Some(event), now);
        }

        if app_limited {
            return None;
        }
        if !iw_acked {
            return None;
        }

        if self.state != ResumeState::Reconnaissance {
            return None;
        }

        let jump_cwnd = self.saved_cwnd / 2;

        if jump_cwnd <= cwnd {
            qerror!("CAREFULERESUME: abort cr: jump smaller than cwnd");
            self.change_state(
                ResumeState::Normal,
                CarefulResumeTrigger::CongestionWindowLimited,
                now,
            );
            return None;
        }

        // FIXME: quiche has rtt as Optional, maybe need to extra checks to validate
        if rtt <= self.saved_rtt / 2 || self.saved_rtt * 10 <= rtt {
            qerror!(
                "CAREFULERESUME: Abort cr: current RTT too divergent from previous RTT rtt_sample={:?} previous_rtt={:?}",
                rtt,
                self.saved_rtt
            );
            self.change_state(
                ResumeState::Normal,
                CarefulResumeTrigger::RttNotValidated,
                now,
            );
            return None;
        }

        qerror!("CAREFULERESUME: cr: going to unvalidated");
        self.change_state(
            ResumeState::Unvalidated(largest_pkt_sent),
            CarefulResumeTrigger::CongestionWindowLimited, // TODO: right trigger??
            now,
        );
        self.pipesize = cwnd;
        Some(jump_cwnd)
    }

    pub fn on_congestion(&mut self, largest_pkt_sent: u64, now: Instant) -> Option<usize> {
        // TODO: mark CR parameters as invalid
        qerror!("CAREFULERESUME: on_congestion");
        match self.state {
            ResumeState::Unvalidated(_) if self.enable_safe_retreat => {
                self.change_state(
                    ResumeState::SafeRetreat(largest_pkt_sent),
                    CarefulResumeTrigger::PacketLoss,
                    now,
                );
                Some(self.pipesize / 2)
            }
            ResumeState::Unvalidated(p) if self.enable_safe_retreat => {
                // TODO: how is this different from unvalidated?
                self.change_state(
                    ResumeState::SafeRetreat(p),
                    CarefulResumeTrigger::PacketLoss,
                    now,
                );
                Some(self.pipesize / 2)
            }
            ResumeState::Unvalidated(_)
            | ResumeState::Validating(_)
            | ResumeState::Reconnaissance => {
                qerror!("CAREFULERESUME: packetloss");
                // self.change_state(ResumeState::Normal, CarefulResumeTrigger::PacketLoss, now);
                None
            }
            _ => None,
        }
    }

    fn change_state(
        &mut self,
        next_state: ResumeState,
        trigger: CarefulResumeTrigger,
        now: Instant,
    ) {
        let event =
            EventData::CarefulResumePhaseUpdated(qlog::events::resume::CarefulResumePhaseUpdated {
                old_phase: Some(self.state.into()),
                new_phase: next_state.into(),
                state_data: qlog::events::resume::CarefulResumeStateParameters {
                    pipesize: self.pipesize as u64,
                    first_unvalidated_packet: 0,
                    last_unvalidated_packet: 0,
                    congestion_window: Some(13328),
                    ssthresh: Some(u64::MAX),
                },
                restored_data: Some(CarefulResumeRestoredParameters {
                    saved_congestion_window: self.saved_cwnd as u64,
                    saved_rtt: self.saved_rtt.as_secs_f32() * 1000.0,
                }),
                trigger: Some(trigger),
            });

        qdebug!("Sending qlog");
        self.qlog.add_event_data_with_instant(|| Some(event), now);

        self.state = next_state;
    }
}

impl From<ResumeState> for CarefulResumePhase {
    fn from(value: ResumeState) -> Self {
        match value {
            ResumeState::Reconnaissance => Self::Reconnaissance,
            ResumeState::Unvalidated(_) => Self::Unvalidated,
            ResumeState::Validating(_) => Self::Validating,
            ResumeState::SafeRetreat(_) => Self::SafeRetreat,
            ResumeState::Normal => Self::Normal,
        }
    }
}

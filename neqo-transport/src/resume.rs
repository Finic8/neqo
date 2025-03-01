use std::time::{Duration, Instant};

use neqo_common::{qdebug, qerror, qinfo, qlog::NeqoQlog};
use qlog::events::{
    resume::{
        CarefulResumePhase, CarefulResumeRestoredParameters, CarefulResumeStateParameters,
        CarefulResumeTrigger,
    },
    EventData,
};

use crate::recovery::SentPacket;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum State {
    Reconnaissance {
        acked_bytes: usize,
    },
    /// Once the last packet of the initial window is acked,
    /// cwnd is inreased to allow pacer and cc to adjust
    /// However the first unvalidated packet is not sent yet,
    /// therefore the packet number is not yet available
    Jumping,
    Unvalidated,
    Validating,
    // Stores the last packet sent during the Unvalidated Phase
    SafeRetreat,
    Normal,
}

impl Default for State {
    fn default() -> Self {
        Self::Reconnaissance { acked_bytes: 0 }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Default)]
pub struct SavedParameters {
    pub rtt: Duration,
    pub cwnd: usize,
    pub enabled: bool,
}

impl From<SavedParameters> for CarefulResumeRestoredParameters {
    fn from(val: SavedParameters) -> Self {
        Self {
            saved_rtt: val.rtt.as_secs_f32() * 1000.0,
            saved_congestion_window: val.cwnd as u64,
        }
    }
}

#[derive(Debug, Default)]
pub struct Resume {
    qlog: NeqoQlog,
    enabled: bool,

    state: State,

    cwnd: usize,
    pipesize: usize,
    first_unvalidated_pkt: u64,
    last_unvalidated_pkt: u64,

    saved: SavedParameters,
    start: Option<Instant>,
}

impl From<&Resume> for CarefulResumeStateParameters {
    fn from(value: &Resume) -> Self {
        Self {
            pipesize: value.pipesize as u64,
            first_unvalidated_packet: value.first_unvalidated_pkt,
            last_unvalidated_packet: value.last_unvalidated_pkt,
            congestion_window: Some(value.cwnd as u64),
            ssthresh: None,
        }
    }
}
impl From<&mut Resume> for CarefulResumeStateParameters {
    fn from(value: &mut Resume) -> Self {
        Self {
            pipesize: value.pipesize as u64,
            first_unvalidated_packet: value.first_unvalidated_pkt,
            last_unvalidated_packet: value.last_unvalidated_pkt,
            congestion_window: Some(value.cwnd as u64),
            ssthresh: None,
        }
    }
}

impl Resume {
    pub fn disabled() -> Self {
        Self {
            qlog: NeqoQlog::disabled(),
            enabled: false,
            ..Default::default()
        }
    }

    pub fn with_paramters(saved: SavedParameters) -> Self {
        Self {
            qlog: NeqoQlog::disabled(),
            enabled: saved.enabled,
            state: State::default(),
            cwnd: 0,
            pipesize: 0,
            first_unvalidated_pkt: 0,
            last_unvalidated_pkt: 0,
            saved,
            start: Some(Instant::now()),
        }
    }

    pub fn set_qlog(&mut self, qlog: NeqoQlog) {
        self.qlog = qlog;
    }

    fn maybe_jump(&mut self, rtt: Duration, initial_cwnd: usize, now: Instant) -> Option<usize> {
        match self.state {
            State::Reconnaissance { acked_bytes } if acked_bytes >= initial_cwnd => {}
            _ => {
                return None;
            }
        }

        let jump_cwnd = self.saved.cwnd / 2;

        if jump_cwnd <= self.cwnd {
            qerror!("CAREFULERESUME: abort cr: jump smaller than cwnd");
            self.change_state(
                State::Normal,
                CarefulResumeTrigger::CongestionWindowLimited,
                now,
            );
            return None;
        }

        if rtt <= self.saved.rtt / 2 || self.saved.rtt * 10 <= rtt {
            qerror!(
                "CAREFULERESUME: Abort cr: current RTT too divergent from previous RTT rtt_sample={:?} previous_rtt={:?}",
                rtt,
                self.saved.rtt
            );
            self.change_state(State::Normal, CarefulResumeTrigger::RttNotValidated, now);
            return None;
        }

        qerror!("CAREFULERESUME: cr: going to unvalidated");
        self.pipesize = self.cwnd;
        self.cwnd = jump_cwnd;
        self.state = State::Jumping;
        Some(jump_cwnd)
    }

    pub fn on_ack(
        &mut self,
        ack: &SentPacket,
        rtt: Duration,
        flightsize: usize,
        cwnd: usize,
        initial_cwnd: usize,
        now: Instant,
    ) -> (Option<usize>, Option<usize>) {
        if !self.enabled {
            return (None, None);
        }
        self.cwnd = cwnd;

        match self.state {
            State::Reconnaissance { mut acked_bytes } => {
                acked_bytes += ack.len();
                self.state = State::Reconnaissance { acked_bytes };

                (self.maybe_jump(rtt, initial_cwnd, now), None)
            }
            State::Unvalidated => {
                self.pipesize += ack.len();
                if ack.pn() < self.first_unvalidated_pkt {
                    return (None, None);
                }

                if self.pipesize < flightsize {
                    qerror!("CAREFULERESUME: next stage validating");
                    self.change_state(
                        State::Validating,
                        CarefulResumeTrigger::FirstUnvalidatedPacketAcknowledged,
                        now,
                    );
                    (Some(flightsize), None)
                } else {
                    qerror!("CAREFULERESUME: complete skipping validating");
                    self.change_state(State::Normal, CarefulResumeTrigger::RateLimited, now);
                    (Some(self.pipesize), None)
                }
            }
            State::Validating => {
                self.pipesize += ack.len();

                if self.last_unvalidated_pkt <= ack.pn() {
                    qerror!("CAREFULERESUME: complete going to normal");
                    self.change_state(
                        State::Normal,
                        CarefulResumeTrigger::LastUnvalidatedPacketAcknowledged,
                        now,
                    );
                }
                (None, None)
            }
            State::SafeRetreat => todo!(),
            _ => (None, None),
        }
    }

    pub fn on_sent(
        &mut self,
        cwnd: usize,
        largest_pkt_sent: u64,
        app_limited: bool,
        initial_cwnd: usize,
        now: Instant,
    ) -> Option<usize> {
        if !self.enabled {
            return None;
        }

        self.cwnd = cwnd;

        if app_limited {
            return None;
        }

        match self.state {
            State::Reconnaissance { .. } if largest_pkt_sent == 0 => {
                let event = EventData::CarefulResumePhaseUpdated(
                    qlog::events::resume::CarefulResumePhaseUpdated {
                        old_phase: None,
                        new_phase: self.state.into(),
                        state_data: self.into(),
                        restored_data: Some(self.saved.into()),
                        trigger: None,
                    },
                );

                qdebug!("Sending qlog");
                self.qlog.add_event_data_with_instant(|| Some(event), now);
                None
            }
            State::Reconnaissance { acked_bytes } if acked_bytes < initial_cwnd => {
                qinfo!(
                    "!!! CWND @ {:?} {}/ {} initial: {}",
                    now.saturating_duration_since(self.start.unwrap_or(now)),
                    acked_bytes,
                    cwnd,
                    initial_cwnd
                );
                None
            }
            State::Jumping => {
                self.first_unvalidated_pkt = largest_pkt_sent;
                self.change_state(
                    State::Unvalidated,
                    CarefulResumeTrigger::CongestionWindowLimited,
                    now,
                );
                None
            }
            State::Unvalidated => {
                self.last_unvalidated_pkt = largest_pkt_sent;
                None
            }
            _ => None,
        }
    }

    pub fn on_congestion(&mut self, _largest_pkt_sent: u64, now: Instant) -> Option<usize> {
        if !self.enabled {
            return None;
        }
        // TODO: mark CR parameters as invalid
        qerror!("CAREFULERESUME: on_congestion");
        match self.state {
            State::Unvalidated => {
                self.change_state(State::SafeRetreat, CarefulResumeTrigger::PacketLoss, now);
                Some(self.pipesize / 2)
            }
            State::Validating | State::Reconnaissance { .. } => {
                qerror!("CAREFULERESUME: packetloss");
                // self.change_state(ResumeState::Normal, CarefulResumeTrigger::PacketLoss, now);
                None
            }
            _ => None,
        }
    }

    fn change_state(&mut self, next_state: State, trigger: CarefulResumeTrigger, now: Instant) {
        let event =
            EventData::CarefulResumePhaseUpdated(qlog::events::resume::CarefulResumePhaseUpdated {
                old_phase: Some(self.state.into()),
                new_phase: next_state.into(),
                state_data: self.into(),
                restored_data: Some(self.saved.into()),
                trigger: Some(trigger),
            });

        qdebug!("Sending qlog");
        self.qlog.add_event_data_with_instant(|| Some(event), now);

        self.state = next_state;
    }
}

impl From<State> for CarefulResumePhase {
    fn from(value: State) -> Self {
        match value {
            State::Reconnaissance { .. } | State::Jumping => Self::Reconnaissance,
            State::Unvalidated => Self::Unvalidated,
            State::Validating => Self::Validating,
            State::SafeRetreat => Self::SafeRetreat,
            State::Normal => Self::Normal,
        }
    }
}

use std::{
    fmt::{write, Display},
    time::{Duration, Instant},
};

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
    Unvalidated {
        start: Instant,
    },
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
impl Display for SavedParameters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rtt: {:?}, cwnd: {}", self.rtt, self.cwnd)
    }
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
    ssthresh: None,

    saved: SavedParameters,
}

impl Display for Resume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.enabled {
            return write!(f, "CarefulResume disabled");
        }
        write!(f, "CarefulResume[{}] @ {:?}", self.saved, self.state)
    }
}

impl From<&Resume> for CarefulResumeStateParameters {
    fn from(value: &Resume) -> Self {
        Self {
            pipesize: value.pipesize as u64,
            first_unvalidated_packet: value.first_unvalidated_pkt,
            last_unvalidated_packet: value.last_unvalidated_pkt,
            congestion_window: Some(value.cwnd as u64),
            ssthresh: value.ssthresh,
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
            ssthresh: None,
            saved,
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
            qerror!("[{self}] abort: jump smaller than cwnd");
            self.change_state(
                State::Normal,
                CarefulResumeTrigger::CongestionWindowLimited,
                now,
            );
            return None;
        }

        if rtt <= self.saved.rtt / 2 || self.saved.rtt * 10 <= rtt {
            qerror!(
                "[{self}] abort: current RTT too divergent from previous RTT rtt_sample={:?} previous_rtt={:?}",
                rtt,
                self.saved.rtt
            );
            self.change_state(State::Normal, CarefulResumeTrigger::RttNotValidated, now);
            return None;
        }

        qerror!("[{self}] going to unvalidated");
        self.pipesize = self.cwnd;
        self.cwnd = jump_cwnd;
        self.state = State::Jumping;
        Some(jump_cwnd)
    }

    fn maybe_rtt_exceeded(
        &mut self,
        start: Instant,
        now: Instant,
        rtt: Duration,
        flightsize: usize,
    ) -> Option<usize> {
        if now.saturating_duration_since(start) < rtt {
            return None;
        }
        qerror!("[{self}] rtt exceeded, going to validating");
        self.change_state(State::Validating, CarefulResumeTrigger::RttExceeded, now);
        Some(flightsize)
    }

    fn enter_validating(&mut self, flightsize: usize, now: Instant) -> usize {
        if self.pipesize < flightsize {
            // On entry to the Validating Phase (when flight_size is greater
            // than the PipeSize), the CWND is set to the flight_size.

            qerror!("[{self}] next stage validating");
            self.change_state(
                State::Validating,
                CarefulResumeTrigger::FirstUnvalidatedPacketAcknowledged,
                now,
            );
            flightsize
        } else {
            // On entry to the Validating Phase, if the flight_size is less than or equal to the PipeSize,
            // the Normal Phase is entered with the CWND reset to the PipeSize.
            // (The PipeSize does not include the part of the jump_cwnd that was not utilised.)

            qerror!("[{self}] rate limited, skipping validating");
            self.change_state(State::Normal, CarefulResumeTrigger::RateLimited, now);
            self.pipesize
        }
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
            State::Unvalidated { start } => {
                // The variable PipeSize is increased by the volume of data acknowledged by each received ACK.
                // (This indicates a previously unvalidated packet has been successfully sent over the path.)
                self.pipesize += ack.len();

                // The sender enters the Validating Phase when an acknowledgement is received
                // for the first packet number (or higher) that was sent in the Unvalidated Phase
                if self.first_unvalidated_pkt <= ack.pn() {
                    return (Some(self.enter_validating(flightsize, now)), None);
                }

                // A sender enters the Validating Phase if more than one RTT has elapsed while in the Unvalidated Phase
                (self.maybe_rtt_exceeded(start, now, rtt, flightsize), None)
            }
            State::Validating => {
                self.pipesize += ack.len();

                if self.last_unvalidated_pkt <= ack.pn() {
                    qerror!("[{self}] complete going to normal");
                    self.change_state(
                        State::Normal,
                        CarefulResumeTrigger::LastUnvalidatedPacketAcknowledged,
                        now,
                    );
                }
                (None, None)
            }
            State::SafeRetreat => {
                self.pipesize += ack.len();
                if ack.pn() < self.last_unvalidated_pkt {
                    return (None, None);
                }
                qerror!("[{self}] safe retreat complete");
                self.ssthresh = Some(self.pipesize);
                self.change_state(State::Normal, CarefulResumeTrigger::ExitRecovery, now);
                (None, self.ssthresh)
            }
            _ => (None, None),
        }
    }

    pub fn on_sent(
        &mut self,
        cwnd: usize,
        largest_pkt_sent: u64,
        rtt: Duration,
        flightsize: usize,
        app_limited: bool,
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
            State::Jumping => {
                self.first_unvalidated_pkt = largest_pkt_sent;
                self.change_state(
                    State::Unvalidated { start: now },
                    CarefulResumeTrigger::CongestionWindowLimited,
                    now,
                );
                None
            }
            State::Unvalidated { start } => {
                self.last_unvalidated_pkt = largest_pkt_sent;
                if flightsize >= cwnd {
                    // The sender enters the Validating Phase when the flight_size equals the CWND.
                    return Some(self.enter_validating(flightsize, now));
                }
                // A sender enters the Validating Phase if more than one RTT has elapsed while in the Unvalidated Phase
                self.maybe_rtt_exceeded(start, now, rtt, flightsize)
            }
            _ => None,
        }
    }

    pub fn on_ecn(&mut self, now: Instant) -> Option<usize> {
        self.on_congestion(CarefulResumeTrigger::EcnCe, now)
    }

    pub fn on_packetloss(&mut self, now: Instant) -> Option<usize> {
        self.on_congestion(CarefulResumeTrigger::PacketLoss, now)
    }

    fn on_congestion(&mut self, trigger: CarefulResumeTrigger, now: Instant) -> Option<usize> {
        if !self.enabled {
            return None;
        }
        qerror!("[{self}] on_congestion");
        match self.state {
            State::Unvalidated { .. } | State::Validating => {
                // TODO: mark CR parameters as invalid
                self.change_state(State::SafeRetreat, trigger, now);
                Some(self.pipesize / 2)
            }
            State::Reconnaissance { .. } => {
                self.change_state(State::Normal, trigger, now);
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
            State::Unvalidated { .. } => Self::Unvalidated,
            State::Validating => Self::Validating,
            State::SafeRetreat => Self::SafeRetreat,
            State::Normal => Self::Normal,
        }
    }
}

//! The MTP2 signalling-link state machine (ITU-T Q.703 §3-§8, §11).
//!
//! [`Mtp2Link`] is a deterministic, I/O-free engine. It consumes inbound signal
//! units and timer ticks and produces outbound signal units plus events for
//! MTP3. Nothing here touches a socket, a card, or the clock: the composing
//! runtime pulls SUs with [`Mtp2Link::poll_transmit`], pushes received SUs with
//! [`Mtp2Link::handle_su`] / [`Mtp2Link::handle_corrupted_su`], advances time
//! with [`Mtp2Link::tick`], and drains [`Event`]s with [`Mtp2Link::poll_event`].
//! Because it is pure, two links can be wired back-to-back through an in-memory
//! pipe and driven to In Service entirely in a unit test.
//!
//! It folds together the Q.703 sub-functions:
//!
//! * **Link State Control (LSC)** - the [`Mtp2State`] lifecycle: Out of Service →
//!   Not Aligned → Aligned → Proving → Aligned Ready → In Service, plus the
//!   Aligned Not Ready and Processor Outage branches.
//! * **Initial Alignment Control (IAC)** - the SIO/SIN/SIE/SIOS status exchange
//!   that drives alignment, with normal vs emergency proving.
//! * **AERM / SUERM** - the error-rate monitors ([`crate::monitor`]) that abort
//!   proving or fail an in-service link.
//! * **Transmission / Reception Control (TXC/RXC)** - the Basic and PCR
//!   retransmission methods ([`crate::retransmission`]).
//! * **Processor Outage / Flow Control** - SIPO and SIB (busy) handling.

use std::collections::VecDeque;
use std::fmt;

use crate::monitor::{
    Aerm, Suerm, AERM_THRESHOLD_EMERGENCY, AERM_THRESHOLD_NORMAL, SUERM_DECREMENT_INTERVAL,
    SUERM_THRESHOLD,
};
use crate::retransmission::{RetransmissionMethod, RxControl, RxOutcome, TxBody, TxControl};
use crate::signal_unit::{SignalUnit, StatusIndication, SuHeader};
use crate::Mtp2Error;

/// The Link State Control states (Q.703 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mtp2State {
    /// Idle / failed: the link carries no MTP3 traffic.
    OutOfService,
    /// Alignment started; sending SIO, waiting to hear the peer.
    NotAligned,
    /// Heard the peer; sending SIN/SIE, waiting for the peer to start proving.
    Aligned,
    /// Proving the link; the AERM is watching the error rate.
    Proving,
    /// Proving complete and the local processor is available; sending FISUs.
    AlignedReady,
    /// Proving complete but the local processor is unavailable; sending SIPO.
    AlignedNotReady,
    /// Carrying MTP3 traffic.
    InService,
    /// In service but the local processor is out; sending SIPO, holding traffic.
    ProcessorOutage,
}

impl fmt::Display for Mtp2State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::OutOfService => "Out of Service",
            Self::NotAligned => "Not Aligned",
            Self::Aligned => "Aligned",
            Self::Proving => "Proving",
            Self::AlignedReady => "Aligned Ready",
            Self::AlignedNotReady => "Aligned Not Ready",
            Self::InService => "In Service",
            Self::ProcessorOutage => "Processor Outage",
        };
        f.write_str(s)
    }
}

/// Why a link left service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutOfServiceReason {
    /// MTP3 asked to take the link down.
    Stopped,
    /// The peer sent SIOS (out of service).
    ReceivedSios,
    /// The peer restarted alignment (SIO/SIN/SIE received while in service).
    AlignmentLost,
    /// The SUERM declared the in-service error rate too high.
    SuermFailure,
    /// The remote-congestion timer T6 expired (peer stayed busy too long).
    T6Expired,
    /// The excessive-acknowledgement-delay timer T7 expired.
    T7Expired,
}

/// Why initial alignment failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignmentFailure {
    /// The AERM aborted proving and all proving attempts were used.
    AermTripped,
    /// The peer sent SIOS during alignment.
    ReceivedSios,
    /// T2 expired in Not Aligned (no response from the peer).
    T2Expired,
    /// T3 expired in Aligned (the peer never started proving).
    T3Expired,
    /// T1 expired in Aligned Ready (the link never reached In Service).
    T1Expired,
}

/// Events the link raises for MTP3 and management.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The link reached In Service.
    InService,
    /// The link left service.
    OutOfService(OutOfServiceReason),
    /// Initial alignment failed.
    AlignmentFailed(AlignmentFailure),
    /// An in-sequence MSU was accepted; deliver its SIO + SIF to MTP3.
    Msu { sio: u8, sif: Vec<u8> },
    /// The peer signalled a processor outage (SIPO).
    RemoteProcessorOutage,
    /// The peer's processor recovered.
    RemoteProcessorRecovered,
    /// The peer signalled congestion (SIB).
    RemoteBusy,
    /// The peer's congestion cleared.
    RemoteBusyEnded,
    /// A gap was detected and a retransmission was requested (Basic method).
    RetransmissionRequested,
}

/// Link timers and error-rate monitor parameters (Q.703 §12). Durations are in
/// abstract ticks; the caller defines what one [`Mtp2Link::tick`] represents (a
/// real deployment maps it to the framer's SU clock or a millisecond timer).
#[derive(Debug, Clone, Copy)]
pub struct Mtp2Config {
    /// Error-correction method.
    pub method: RetransmissionMethod,
    /// Request emergency alignment (short proving).
    pub emergency: bool,
    /// T1 - Aligned Ready guard.
    pub t1_aligned_ready: u32,
    /// T2 - Not Aligned guard.
    pub t2_not_aligned: u32,
    /// T3 - Aligned guard.
    pub t3_aligned: u32,
    /// T4(n) - normal proving period.
    pub t4_proving_normal: u32,
    /// T4(e) - emergency proving period.
    pub t4_proving_emergency: u32,
    /// T6 - remote-congestion guard.
    pub t6_remote_congestion: u32,
    /// T7 - excessive acknowledgement delay guard.
    pub t7_excessive_delay: u32,
    /// AERM abort threshold for normal proving.
    pub aerm_threshold_normal: u32,
    /// AERM abort threshold for emergency proving.
    pub aerm_threshold_emergency: u32,
    /// SUERM failure threshold T.
    pub suerm_threshold: u32,
    /// SUERM leak interval D.
    pub suerm_decrement_interval: u32,
    /// Maximum proving attempts before alignment fails (Q.703 Cp).
    pub proving_attempts: u32,
}

impl Default for Mtp2Config {
    fn default() -> Self {
        // Tick counts are illustrative; a deployment provisions them from the
        // Q.703 §12 timer table scaled to its tick period.
        Self {
            method: RetransmissionMethod::Basic,
            emergency: false,
            t1_aligned_ready: 500,
            t2_not_aligned: 250,
            t3_aligned: 20,
            t4_proving_normal: 120,
            t4_proving_emergency: 6,
            t6_remote_congestion: 60,
            t7_excessive_delay: 20,
            aerm_threshold_normal: AERM_THRESHOLD_NORMAL,
            aerm_threshold_emergency: AERM_THRESHOLD_EMERGENCY,
            suerm_threshold: SUERM_THRESHOLD,
            suerm_decrement_interval: SUERM_DECREMENT_INTERVAL,
            proving_attempts: 5,
        }
    }
}

/// Decrement a running timer; return `true` if it expired on this step.
fn step_timer(t: &mut Option<u32>) -> bool {
    if let Some(remaining) = t {
        if *remaining <= 1 {
            *t = None;
            return true;
        }
        *remaining -= 1;
    }
    false
}

/// The MTP2 signalling-link engine.
pub struct Mtp2Link {
    config: Mtp2Config,
    state: Mtp2State,
    /// Whether the link is powered: after `start` it transmits (SIOS while out of
    /// service so a failed link still notifies the peer); after `stop` it is dark.
    active: bool,
    /// Effective emergency alignment (local request OR a received SIE).
    emergency: bool,
    local_processor_outage: bool,
    remote_processor_outage: bool,
    local_busy: bool,
    remote_busy: bool,
    proving_attempt: u32,
    tx: TxControl,
    rx: RxControl,
    aerm: Aerm,
    suerm: Suerm,
    // Timers (remaining ticks; None = stopped).
    t1: Option<u32>,
    t2: Option<u32>,
    t3: Option<u32>,
    t4: Option<u32>,
    t6: Option<u32>,
    t7: Option<u32>,
    events: VecDeque<Event>,
}

impl Mtp2Link {
    /// Create a powered-off link with `config`. Call [`start`](Self::start) to
    /// begin initial alignment.
    pub fn new(config: Mtp2Config) -> Self {
        let aerm = Aerm::new(config.aerm_threshold_normal);
        let suerm = Suerm::new(config.suerm_threshold, config.suerm_decrement_interval);
        Self {
            config,
            state: Mtp2State::OutOfService,
            active: false,
            emergency: config.emergency,
            local_processor_outage: false,
            remote_processor_outage: false,
            local_busy: false,
            remote_busy: false,
            proving_attempt: 0,
            tx: TxControl::new(config.method),
            rx: RxControl::new(config.method),
            aerm,
            suerm,
            t1: None,
            t2: None,
            t3: None,
            t4: None,
            t6: None,
            t7: None,
            events: VecDeque::new(),
        }
    }

    /// The current link state.
    pub fn state(&self) -> Mtp2State {
        self.state
    }

    /// Pull the next raised event, if any.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    fn emit(&mut self, e: Event) {
        self.events.push_back(e);
    }

    fn stop_all_timers(&mut self) {
        self.t1 = None;
        self.t2 = None;
        self.t3 = None;
        self.t4 = None;
        self.t6 = None;
        self.t7 = None;
    }

    // ── MTP3 / management primitives ────────────────────────────────────────────

    /// Start initial alignment (Emergency-or-Normal). No-op unless out of service.
    pub fn start(&mut self) {
        if self.state != Mtp2State::OutOfService {
            return;
        }
        self.active = true;
        self.emergency = self.config.emergency;
        self.remote_processor_outage = false;
        self.remote_busy = false;
        self.local_busy = false;
        self.proving_attempt = 0;
        self.tx = TxControl::new(self.config.method);
        self.rx = RxControl::new(self.config.method);
        self.suerm.reset();
        self.aerm.restart(self.config.aerm_threshold_normal);
        self.stop_all_timers();
        self.state = Mtp2State::NotAligned;
        self.t2 = Some(self.config.t2_not_aligned);
    }

    /// Take the link out of service on MTP3's request.
    pub fn stop(&mut self) {
        self.active = false;
        self.stop_all_timers();
        self.state = Mtp2State::OutOfService;
        self.emit(Event::OutOfService(OutOfServiceReason::Stopped));
    }

    /// Request emergency alignment for the next/ongoing alignment.
    pub fn set_emergency(&mut self, emergency: bool) {
        self.emergency = emergency;
    }

    /// Signal a local processor outage (MTP3 unavailable).
    pub fn local_processor_outage(&mut self) {
        self.local_processor_outage = true;
        match self.state {
            Mtp2State::InService => self.state = Mtp2State::ProcessorOutage,
            Mtp2State::AlignedReady => {
                self.t1 = None;
                self.state = Mtp2State::AlignedNotReady;
            }
            _ => {}
        }
    }

    /// Signal that the local processor has recovered.
    pub fn local_processor_recovered(&mut self) {
        self.local_processor_outage = false;
        match self.state {
            Mtp2State::ProcessorOutage => self.state = Mtp2State::InService,
            Mtp2State::AlignedNotReady => {
                self.state = Mtp2State::AlignedReady;
                self.t1 = Some(self.config.t1_aligned_ready);
            }
            _ => {}
        }
    }

    /// Signal local congestion: the link sends SIB until [`local_busy_ended`].
    ///
    /// [`local_busy_ended`]: Self::local_busy_ended
    pub fn local_busy(&mut self) {
        self.local_busy = true;
    }

    /// Signal that local congestion has cleared.
    pub fn local_busy_ended(&mut self) {
        self.local_busy = false;
    }

    /// Queue an MSU (SIO + SIF) for transmission. It flows once the link is in
    /// service; queued beforehand it waits. Fails only if the body exceeds the
    /// link maximum.
    pub fn submit_msu(&mut self, sio: u8, sif: Vec<u8>) -> Result<(), Mtp2Error> {
        let body = 1 + sif.len();
        if body > crate::MAX_MSU_BODY {
            return Err(Mtp2Error::MsuTooLarge(body));
        }
        self.tx.submit(sio, sif);
        Ok(())
    }

    // ── Outbound ─────────────────────────────────────────────────────────────────

    fn header_for_control(&self) -> SuHeader {
        // BSN/BIB acknowledge the peer; FSN/FIB are the current transmit values.
        SuHeader {
            bsn: self.rx.bsn(),
            bib: self.rx.bib(),
            fsn: self.tx.last_fsn(),
            fib: self.tx.fib(),
        }
    }

    fn lssu(&self, status: StatusIndication) -> SignalUnit {
        SignalUnit::lssu(self.header_for_control(), status)
    }

    /// Produce the next signal unit to transmit. An active link always has
    /// something to send (a FISU when idle); a powered-off link returns `None`.
    pub fn poll_transmit(&mut self) -> Option<SignalUnit> {
        let alignment_status = if self.emergency {
            StatusIndication::EmergencyAlignment
        } else {
            StatusIndication::NormalAlignment
        };

        let su = match self.state {
            Mtp2State::OutOfService => {
                if self.active {
                    self.lssu(StatusIndication::OutOfService)
                } else {
                    return None;
                }
            }
            Mtp2State::NotAligned => self.lssu(StatusIndication::OutOfAlignment),
            Mtp2State::Aligned | Mtp2State::Proving => self.lssu(alignment_status),
            Mtp2State::AlignedNotReady | Mtp2State::ProcessorOutage => {
                self.lssu(StatusIndication::ProcessorOutage)
            }
            Mtp2State::AlignedReady => SignalUnit::fisu(self.header_for_control()),
            Mtp2State::InService => {
                if self.local_busy {
                    self.lssu(StatusIndication::Busy)
                } else if self.remote_processor_outage || self.remote_busy {
                    // Hold MTP3 traffic; keep the link filled and acknowledging.
                    SignalUnit::fisu(self.header_for_control())
                } else {
                    let frame = self.tx.poll();
                    let header = SuHeader {
                        bsn: self.rx.bsn(),
                        bib: self.rx.bib(),
                        fsn: frame.fsn,
                        fib: frame.fib,
                    };
                    match frame.body {
                        TxBody::Fisu => SignalUnit::fisu(header),
                        TxBody::Msu { sio, sif } => {
                            // Body size was validated at submit; unwrap-free build.
                            match SignalUnit::msu(header, sio, sif) {
                                Ok(su) => su,
                                Err(_) => SignalUnit::fisu(header),
                            }
                        }
                    }
                }
            }
        };

        // Guard against a peer that never acknowledges our outstanding MSUs (T7).
        if self.state == Mtp2State::InService && self.tx.unacked_len() > 0 && self.t7.is_none() {
            self.t7 = Some(self.config.t7_excessive_delay);
        }
        Some(su)
    }

    // ── Inbound ──────────────────────────────────────────────────────────────────

    /// Feed a correctly-received (CRC-valid) signal unit into the link.
    pub fn handle_su(&mut self, su: &SignalUnit) {
        // In service, every received SU feeds the SUERM leak counter.
        if matches!(
            self.state,
            Mtp2State::InService | Mtp2State::ProcessorOutage
        ) && self.suerm.record(false)
        {
            self.fail_out_of_service(OutOfServiceReason::SuermFailure);
            return;
        }

        match su {
            SignalUnit::Lssu { status, .. } => self.handle_lssu(*status),
            SignalUnit::Fisu { header } => self.handle_fisu_or_msu(*header, None),
            SignalUnit::Msu { header, sio, sif } => {
                self.handle_fisu_or_msu(*header, Some((*sio, sif.clone())))
            }
        }
    }

    /// Report a signal unit that the layer-1 framer received with a bad CRC-16.
    /// This feeds the AERM (during proving) or the SUERM (in service).
    pub fn handle_corrupted_su(&mut self) {
        // Short-circuit keeps the monitor call state-scoped: the AERM only counts
        // while proving, the SUERM only while carrying traffic.
        if self.state == Mtp2State::Proving && self.aerm.record_error() {
            self.proving_aborted();
        } else if matches!(
            self.state,
            Mtp2State::InService | Mtp2State::ProcessorOutage
        ) && self.suerm.record(true)
        {
            self.fail_out_of_service(OutOfServiceReason::SuermFailure);
        }
    }

    fn handle_lssu(&mut self, status: StatusIndication) {
        use Mtp2State as S;
        use StatusIndication as St;

        // A received SIE upgrades alignment to emergency at any alignment phase.
        if matches!(status, St::EmergencyAlignment)
            && matches!(self.state, S::NotAligned | S::Aligned | S::Proving)
        {
            self.emergency = true;
        }

        match (self.state, status) {
            // ── Not Aligned ──────────────────────────────────────────────────
            (S::NotAligned, St::OutOfAlignment | St::NormalAlignment | St::EmergencyAlignment) => {
                self.t2 = None;
                self.state = S::Aligned;
                self.t3 = Some(self.config.t3_aligned);
            }
            (S::NotAligned, St::OutOfService) => { /* peer not ready yet; T2 guards */ }

            // ── Aligned ──────────────────────────────────────────────────────
            (S::Aligned, St::NormalAlignment | St::EmergencyAlignment) => {
                self.t3 = None;
                self.enter_proving();
            }
            (S::Aligned, St::OutOfAlignment) => { /* peer restarted; keep waiting */ }
            (S::Aligned, St::OutOfService) => {
                self.fail_alignment(AlignmentFailure::ReceivedSios);
            }

            // ── Proving ──────────────────────────────────────────────────────
            (S::Proving, St::NormalAlignment | St::EmergencyAlignment) => {
                if matches!(status, St::EmergencyAlignment) {
                    // Switch to (shorter) emergency proving.
                    self.enter_proving();
                }
            }
            (S::Proving, St::OutOfAlignment) => {
                // Peer fell back to Not Aligned: return to Aligned.
                self.t4 = None;
                self.state = S::Aligned;
                self.t3 = Some(self.config.t3_aligned);
            }
            (S::Proving, St::OutOfService) => {
                self.fail_alignment(AlignmentFailure::ReceivedSios);
            }

            // ── Aligned Ready ────────────────────────────────────────────────
            (S::AlignedReady, St::ProcessorOutage) => {
                self.enter_in_service();
                self.remote_processor_outage = true;
                self.emit(Event::RemoteProcessorOutage);
            }
            (S::AlignedReady, St::OutOfService) => {
                self.fail_alignment(AlignmentFailure::ReceivedSios);
            }
            (S::AlignedReady, _) => { /* peer still aligning; keep sending FISU */ }

            // ── Aligned Not Ready ────────────────────────────────────────────
            (S::AlignedNotReady, St::OutOfService) => {
                self.fail_alignment(AlignmentFailure::ReceivedSios);
            }
            (S::AlignedNotReady, _) => {}

            // ── In Service / Processor Outage ────────────────────────────────
            (S::InService | S::ProcessorOutage, St::ProcessorOutage) => {
                if !self.remote_processor_outage {
                    self.remote_processor_outage = true;
                    self.emit(Event::RemoteProcessorOutage);
                }
            }
            (S::InService | S::ProcessorOutage, St::Busy) => {
                if !self.remote_busy {
                    self.remote_busy = true;
                    self.emit(Event::RemoteBusy);
                }
                self.t6 = Some(self.config.t6_remote_congestion);
            }
            (S::InService | S::ProcessorOutage, St::OutOfService) => {
                self.fail_out_of_service(OutOfServiceReason::ReceivedSios);
            }
            (
                S::InService | S::ProcessorOutage,
                St::OutOfAlignment | St::NormalAlignment | St::EmergencyAlignment,
            ) => {
                // Peer restarted alignment: the link has failed.
                self.fail_out_of_service(OutOfServiceReason::AlignmentLost);
            }

            // ── Everything else ──────────────────────────────────────────────
            // Out of Service (realignment is initiated by MTP3 via start()), and
            // status indications with no effect in the current state (e.g. SIPO or
            // SIB received mid-alignment): ignore.
            _ => {}
        }
    }

    fn handle_fisu_or_msu(&mut self, header: SuHeader, msu: Option<(u8, Vec<u8>)>) {
        use Mtp2State::*;
        match self.state {
            AlignedReady => {
                // The peer finished proving and is transmitting FISUs/MSUs.
                self.enter_in_service();
                if let Some((sio, sif)) = msu {
                    self.process_in_service_su(header, Some((sio, sif)));
                } else {
                    self.process_in_service_su(header, None);
                }
            }
            InService | ProcessorOutage => self.process_in_service_su(header, msu),
            _ => { /* FISU/MSU is unexpected during alignment; ignore */ }
        }
    }

    /// Common in-service reception: apply the acknowledgement, clear remote
    /// outage/busy, and (for an MSU) run the sequence check.
    fn process_in_service_su(&mut self, header: SuHeader, msu: Option<(u8, Vec<u8>)>) {
        // Positive/negative acknowledgement of our outstanding MSUs.
        self.tx.on_ack(header.bsn, header.bib);
        if self.tx.unacked_len() == 0 {
            self.t7 = None;
        } else {
            self.t7 = Some(self.config.t7_excessive_delay);
        }

        // Any FISU/MSU ends a remote processor outage or busy condition.
        if self.remote_processor_outage {
            self.remote_processor_outage = false;
            self.emit(Event::RemoteProcessorRecovered);
        }
        if self.remote_busy {
            self.remote_busy = false;
            self.t6 = None;
            self.emit(Event::RemoteBusyEnded);
        }

        if let Some((sio, sif)) = msu {
            // During a local processor outage MTP3 cannot take MSUs; drop payload.
            if self.state == Mtp2State::ProcessorOutage {
                return;
            }
            match self.rx.on_msu(header.fsn, sio, sif) {
                RxOutcome::Accepted { sio, sif } => self.emit(Event::Msu { sio, sif }),
                RxOutcome::RetransmissionRequested => self.emit(Event::RetransmissionRequested),
                RxOutcome::Discarded => {}
            }
        }
    }

    // ── State helpers ────────────────────────────────────────────────────────────

    fn enter_proving(&mut self) {
        self.state = Mtp2State::Proving;
        self.proving_attempt += 1;
        let (period, threshold) = if self.emergency {
            (
                self.config.t4_proving_emergency,
                self.config.aerm_threshold_emergency,
            )
        } else {
            (
                self.config.t4_proving_normal,
                self.config.aerm_threshold_normal,
            )
        };
        self.aerm.restart(threshold);
        self.t4 = Some(period);
    }

    fn proving_succeeded(&mut self) {
        self.t4 = None;
        if self.local_processor_outage {
            self.state = Mtp2State::AlignedNotReady;
        } else {
            self.state = Mtp2State::AlignedReady;
            self.t1 = Some(self.config.t1_aligned_ready);
        }
    }

    fn proving_aborted(&mut self) {
        if self.proving_attempt < self.config.proving_attempts {
            // Another proving attempt (Q.703 Cp): re-prove from Aligned.
            self.t4 = None;
            self.state = Mtp2State::Aligned;
            self.t3 = Some(self.config.t3_aligned);
        } else {
            self.fail_alignment(AlignmentFailure::AermTripped);
        }
    }

    fn enter_in_service(&mut self) {
        self.t1 = None;
        self.suerm.reset();
        self.state = Mtp2State::InService;
        self.emit(Event::InService);
    }

    fn fail_alignment(&mut self, failure: AlignmentFailure) {
        self.stop_all_timers();
        self.state = Mtp2State::OutOfService;
        self.emit(Event::AlignmentFailed(failure));
    }

    fn fail_out_of_service(&mut self, reason: OutOfServiceReason) {
        self.stop_all_timers();
        self.remote_processor_outage = false;
        self.remote_busy = false;
        self.local_busy = false;
        self.state = Mtp2State::OutOfService;
        self.emit(Event::OutOfService(reason));
    }

    // ── Time ─────────────────────────────────────────────────────────────────────

    /// Advance the link's timers by one tick and process any expiry.
    pub fn tick(&mut self) {
        if step_timer(&mut self.t2) && self.state == Mtp2State::NotAligned {
            self.fail_alignment(AlignmentFailure::T2Expired);
            return;
        }
        if step_timer(&mut self.t3) && self.state == Mtp2State::Aligned {
            self.fail_alignment(AlignmentFailure::T3Expired);
            return;
        }
        if step_timer(&mut self.t4) && self.state == Mtp2State::Proving {
            self.proving_succeeded();
            return;
        }
        if step_timer(&mut self.t1) && self.state == Mtp2State::AlignedReady {
            self.fail_alignment(AlignmentFailure::T1Expired);
            return;
        }
        if step_timer(&mut self.t6)
            && matches!(
                self.state,
                Mtp2State::InService | Mtp2State::ProcessorOutage
            )
        {
            self.fail_out_of_service(OutOfServiceReason::T6Expired);
            return;
        }
        if step_timer(&mut self.t7)
            && matches!(
                self.state,
                Mtp2State::InService | Mtp2State::ProcessorOutage
            )
        {
            self.fail_out_of_service(OutOfServiceReason::T7Expired);
        }
    }
}

impl fmt::Display for Mtp2Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MTP2 Link [state={}]", self.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast_config() -> Mtp2Config {
        // Large guard timers, short proving: only T4 drives the handshake here.
        Mtp2Config {
            t1_aligned_ready: 10_000,
            t2_not_aligned: 10_000,
            t3_aligned: 10_000,
            t4_proving_normal: 3,
            t7_excessive_delay: 10_000,
            t6_remote_congestion: 10_000,
            ..Mtp2Config::default()
        }
    }

    fn drain_states(link: &mut Mtp2Link) -> Vec<Event> {
        let mut out = Vec::new();
        while let Some(e) = link.poll_event() {
            out.push(e);
        }
        out
    }

    #[test]
    fn start_enters_not_aligned_and_sends_sio() {
        let mut link = Mtp2Link::new(fast_config());
        link.start();
        assert_eq!(link.state(), Mtp2State::NotAligned);
        match link.poll_transmit() {
            Some(SignalUnit::Lssu { status, .. }) => {
                assert_eq!(status, StatusIndication::OutOfAlignment)
            }
            other => panic!("expected LSSU SIO, got {other:?}"),
        }
    }

    #[test]
    fn alignment_sequence_reaches_aligned_ready() {
        let mut link = Mtp2Link::new(fast_config());
        link.start(); // NotAligned, sending SIO

        // Peer sends SIO → Aligned, now sending SIN.
        link.handle_su(&SignalUnit::lssu(
            SuHeader::new(127, true, 127, true).unwrap(),
            StatusIndication::OutOfAlignment,
        ));
        assert_eq!(link.state(), Mtp2State::Aligned);
        assert!(matches!(
            link.poll_transmit(),
            Some(SignalUnit::Lssu {
                status: StatusIndication::NormalAlignment,
                ..
            })
        ));

        // Peer sends SIN → Proving.
        link.handle_su(&SignalUnit::lssu(
            SuHeader::new(127, true, 127, true).unwrap(),
            StatusIndication::NormalAlignment,
        ));
        assert_eq!(link.state(), Mtp2State::Proving);

        // Proving period elapses (T4 = 3) with no errors → Aligned Ready.
        for _ in 0..3 {
            link.tick();
        }
        assert_eq!(link.state(), Mtp2State::AlignedReady);
    }

    #[test]
    fn aerm_trips_and_exhausts_attempts_failing_alignment() {
        let cfg = Mtp2Config {
            aerm_threshold_normal: 2,
            proving_attempts: 1, // no retries
            ..fast_config()
        };
        let mut link = Mtp2Link::new(cfg);
        link.start();
        link.handle_su(&lssu(StatusIndication::OutOfAlignment));
        link.handle_su(&lssu(StatusIndication::NormalAlignment));
        assert_eq!(link.state(), Mtp2State::Proving);

        // Two corrupted SUs during proving trip the AERM.
        link.handle_corrupted_su();
        link.handle_corrupted_su();
        assert_eq!(link.state(), Mtp2State::OutOfService);
        assert!(drain_states(&mut link)
            .contains(&Event::AlignmentFailed(AlignmentFailure::AermTripped)));
    }

    #[test]
    fn suerm_trips_in_service_and_fails_link() {
        let cfg = Mtp2Config {
            suerm_threshold: 3,
            suerm_decrement_interval: 10_000,
            ..fast_config()
        };
        let mut link = Mtp2Link::new(cfg);
        bring_in_service(&mut link);
        assert_eq!(link.state(), Mtp2State::InService);

        link.handle_corrupted_su();
        link.handle_corrupted_su();
        link.handle_corrupted_su();
        assert_eq!(link.state(), Mtp2State::OutOfService);
        assert!(drain_states(&mut link)
            .contains(&Event::OutOfService(OutOfServiceReason::SuermFailure)));
    }

    #[test]
    fn received_sios_in_service_fails_link() {
        let mut link = Mtp2Link::new(fast_config());
        bring_in_service(&mut link);
        link.handle_su(&lssu(StatusIndication::OutOfService));
        assert_eq!(link.state(), Mtp2State::OutOfService);
    }

    #[test]
    fn local_processor_outage_transitions_and_recovers() {
        let mut link = Mtp2Link::new(fast_config());
        bring_in_service(&mut link);
        link.local_processor_outage();
        assert_eq!(link.state(), Mtp2State::ProcessorOutage);
        // Sends SIPO while out.
        assert!(matches!(
            link.poll_transmit(),
            Some(SignalUnit::Lssu {
                status: StatusIndication::ProcessorOutage,
                ..
            })
        ));
        link.local_processor_recovered();
        assert_eq!(link.state(), Mtp2State::InService);
    }

    #[test]
    fn remote_processor_outage_event_and_recovery() {
        let mut link = Mtp2Link::new(fast_config());
        bring_in_service(&mut link);
        let _ = drain_states(&mut link);
        link.handle_su(&lssu(StatusIndication::ProcessorOutage));
        assert!(drain_states(&mut link).contains(&Event::RemoteProcessorOutage));
        // A following FISU clears it.
        link.handle_su(&SignalUnit::fisu(SuHeader::new(0, true, 0, true).unwrap()));
        assert!(drain_states(&mut link).contains(&Event::RemoteProcessorRecovered));
    }

    #[test]
    fn stop_forces_out_of_service() {
        let mut link = Mtp2Link::new(fast_config());
        bring_in_service(&mut link);
        link.stop();
        assert_eq!(link.state(), Mtp2State::OutOfService);
        assert!(link.poll_transmit().is_none()); // powered off
    }

    // Helpers.
    fn lssu(status: StatusIndication) -> SignalUnit {
        SignalUnit::lssu(SuHeader::new(127, true, 127, true).unwrap(), status)
    }

    fn bring_in_service(link: &mut Mtp2Link) {
        link.start();
        link.handle_su(&lssu(StatusIndication::OutOfAlignment));
        link.handle_su(&lssu(StatusIndication::NormalAlignment));
        for _ in 0..3 {
            link.tick();
        }
        assert_eq!(link.state(), Mtp2State::AlignedReady);
        // Peer FISU → In Service.
        link.handle_su(&SignalUnit::fisu(SuHeader::new(0, true, 0, true).unwrap()));
    }
}

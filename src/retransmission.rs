//! Error-correction / retransmission control (Q.703 §5).
//!
//! MTP2 defines two error-correction methods; a link uses exactly one, chosen at
//! configuration time.
//!
//! * **Basic method** (§5.2, §5.3) - for terrestrial links with short round trips.
//!   Every MSU carries a Forward Sequence Number (FSN); the peer acknowledges with
//!   the Backward Sequence Number (BSN). A receiver that sees a gap discards the
//!   out-of-sequence MSU and *negatively acknowledges* by toggling its Backward
//!   Indicator Bit (BIB); the transmitter, seeing its FIB no longer match the
//!   returned BIB, retransmits everything from the buffer. This is
//!   retransmit-on-negative-acknowledgement.
//!
//! * **Preventive Cyclic Retransmission (PCR)** (§5.4) - for long-propagation
//!   links (satellite), where waiting for a negative acknowledgement costs too
//!   much. There is no negative acknowledgement and the indicator bits never
//!   toggle: whenever the transmitter has no *new* MSU to send it cyclically
//!   retransmits every unacknowledged MSU, so a lost MSU is corrected without a
//!   round trip. When the unacknowledged backlog reaches `N1` MSUs or `N2` octets,
//!   *forced retransmission* stops new MSUs until the buffer drains.
//!
//! Both methods share the receive-side sequence check and the positive
//! acknowledgement (BSN) that frees the retransmission buffer. The two sides are
//! modelled as [`TxControl`] (assigns FSNs, owns the retransmission buffer) and
//! [`RxControl`] (checks sequence, drives BSN/BIB), which [`crate::Mtp2Link`]
//! composes into whole signal units.

use std::collections::VecDeque;

use crate::signal_unit::SEQ_MODULUS;

/// Half the sequence space; used for the standard "is a ahead of b" comparison.
const SEQ_HALF: u16 = SEQ_MODULUS / 2;

/// `(a + b) mod 128`.
fn seq_add(a: u8, b: u8) -> u8 {
    ((a as u16 + b as u16) % SEQ_MODULUS) as u8
}

/// `(a - b) mod 128` - the forward distance from `b` to `a`.
fn seq_forward(a: u8, b: u8) -> u8 {
    ((a as u16 + SEQ_MODULUS - b as u16) % SEQ_MODULUS) as u8
}

/// The MTP2 error-correction method a link runs (Q.703 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetransmissionMethod {
    /// Basic method: retransmit on negative acknowledgement (BIB/FIB).
    Basic,
    /// Preventive Cyclic Retransmission: cyclic retransmit when idle, no NACK.
    Pcr(PcrParams),
}

/// PCR forced-retransmission thresholds (Q.703 §5.4): stop sending new MSUs once
/// the unacknowledged buffer holds `n1` MSUs or `n2` octets, and force-retransmit
/// until it drains below them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcrParams {
    /// Maximum number of unacknowledged MSUs before forced retransmission.
    pub n1: usize,
    /// Maximum number of unacknowledged MSU octets before forced retransmission.
    pub n2: usize,
}

impl Default for PcrParams {
    fn default() -> Self {
        // Representative defaults; the concrete N1/N2 depend on link bit rate and
        // propagation delay and are provisioned per link.
        Self { n1: 127, n2: 8000 }
    }
}

/// One MSU held in the retransmission buffer with the FSN it was sent under.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BufferedMsu {
    fsn: u8,
    sio: u8,
    sif: Vec<u8>,
}

/// The body of a frame the transmitter wants to put on the link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxBody {
    /// Nothing to send: a Fill-In Signal Unit keeps the link busy and carries acks.
    Fisu,
    /// An MSU (new, retransmitted, or cyclically retransmitted).
    Msu { sio: u8, sif: Vec<u8> },
}

/// A frame the transmit side wants to emit: the forward fields plus a body. The
/// backward fields (BSN/BIB) are supplied by [`RxControl`] when the link
/// assembles the signal unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxFrame {
    pub fsn: u8,
    pub fib: bool,
    pub body: TxBody,
}

/// Transmit-side error correction: FSN assignment, the retransmission buffer, and
/// the method-specific choice of what to send next.
#[derive(Debug, Clone)]
pub struct TxControl {
    method: RetransmissionMethod,
    /// FSN of the most recently assigned MSU. Starts at 127 so the first MSU is 0.
    last_fsn: u8,
    /// Forward Indicator Bit. Initial value 1 (Q.703 §5).
    fib: bool,
    /// New MSUs from MTP3 awaiting their first transmission.
    pending: VecDeque<(u8, Vec<u8>)>,
    /// Transmitted but unacknowledged MSUs (the retransmission buffer).
    unacked: VecDeque<BufferedMsu>,
    /// Basic method: index into `unacked` of the next MSU to retransmit after a
    /// negative acknowledgement (`None` when not retransmitting).
    basic_cursor: Option<usize>,
    /// PCR: cursor for cyclic retransmission of `unacked`.
    pcr_cursor: usize,
    /// FSN most recently acknowledged by the peer's BSN.
    last_acked: u8,
}

impl TxControl {
    /// Create a transmit controller for `method`, with the Q.703 initial sequence
    /// state (FSN 127, FIB 1).
    pub fn new(method: RetransmissionMethod) -> Self {
        Self {
            method,
            last_fsn: (SEQ_MODULUS - 1) as u8,
            fib: true,
            pending: VecDeque::new(),
            unacked: VecDeque::new(),
            basic_cursor: None,
            pcr_cursor: 0,
            last_acked: (SEQ_MODULUS - 1) as u8,
        }
    }

    /// Current Forward Indicator Bit.
    pub fn fib(&self) -> bool {
        self.fib
    }

    /// FSN carried by a FISU/LSSU: the most recently assigned MSU sequence number.
    pub fn last_fsn(&self) -> u8 {
        self.last_fsn
    }

    /// Number of unacknowledged MSUs held for retransmission.
    pub fn unacked_len(&self) -> usize {
        self.unacked.len()
    }

    /// Queue a new MSU (SIO + SIF) from MTP3 for transmission.
    pub fn submit(&mut self, sio: u8, sif: Vec<u8>) {
        self.pending.push_back((sio, sif));
    }

    /// Total octets currently unacknowledged (for the PCR N2 threshold).
    fn unacked_octets(&self) -> usize {
        self.unacked.iter().map(|m| 1 + m.sif.len()).sum()
    }

    /// Decide the next frame to transmit. An active link always transmits, so this
    /// returns a FISU when there is nothing else to send.
    pub fn poll(&mut self) -> TxFrame {
        match self.method {
            RetransmissionMethod::Basic => self.poll_basic(),
            RetransmissionMethod::Pcr(params) => self.poll_pcr(params),
        }
    }

    fn poll_basic(&mut self) -> TxFrame {
        // 1. A negative acknowledgement is being serviced: retransmit the buffer.
        if let Some(cursor) = self.basic_cursor {
            if cursor < self.unacked.len() {
                let m = &self.unacked[cursor];
                let frame = TxFrame {
                    fsn: m.fsn,
                    fib: self.fib,
                    body: TxBody::Msu {
                        sio: m.sio,
                        sif: m.sif.clone(),
                    },
                };
                self.basic_cursor = Some(cursor + 1);
                return frame;
            }
            // Buffer fully retransmitted; resume normal transmission.
            self.basic_cursor = None;
        }

        // 2. A new MSU is waiting: assign the next FSN and buffer it.
        if let Some((sio, sif)) = self.pending.pop_front() {
            let fsn = seq_add(self.last_fsn, 1);
            self.last_fsn = fsn;
            self.unacked.push_back(BufferedMsu {
                fsn,
                sio,
                sif: sif.clone(),
            });
            return TxFrame {
                fsn,
                fib: self.fib,
                body: TxBody::Msu { sio, sif },
            };
        }

        // 3. Nothing to send: FISU carrying the current FSN/FIB.
        TxFrame {
            fsn: self.last_fsn,
            fib: self.fib,
            body: TxBody::Fisu,
        }
    }

    fn poll_pcr(&mut self, params: PcrParams) -> TxFrame {
        let forced = self.unacked.len() >= params.n1 || self.unacked_octets() >= params.n2;

        // New MSU and not in forced retransmission: send it.
        if !forced {
            if let Some((sio, sif)) = self.pending.pop_front() {
                let fsn = seq_add(self.last_fsn, 1);
                self.last_fsn = fsn;
                self.unacked.push_back(BufferedMsu {
                    fsn,
                    sio,
                    sif: sif.clone(),
                });
                // A fresh MSU restarts the cyclic sweep from the buffer head.
                self.pcr_cursor = 0;
                return TxFrame {
                    fsn,
                    fib: self.fib,
                    body: TxBody::Msu { sio, sif },
                };
            }
        }

        // Idle (or forced): cyclically retransmit the unacknowledged buffer.
        if !self.unacked.is_empty() {
            if self.pcr_cursor >= self.unacked.len() {
                self.pcr_cursor = 0;
            }
            let m = &self.unacked[self.pcr_cursor];
            let frame = TxFrame {
                fsn: m.fsn,
                fib: self.fib,
                body: TxBody::Msu {
                    sio: m.sio,
                    sif: m.sif.clone(),
                },
            };
            self.pcr_cursor += 1;
            return frame;
        }

        // Nothing outstanding: FISU.
        TxFrame {
            fsn: self.last_fsn,
            fib: self.fib,
            body: TxBody::Fisu,
        }
    }

    /// Apply the peer's acknowledgement carried in every received SU: the BSN
    /// positively acknowledges (frees the buffer) and, under the Basic method, a
    /// BIB that no longer matches our FIB is a negative acknowledgement that
    /// triggers retransmission.
    pub fn on_ack(&mut self, bsn: u8, bib: bool) {
        // Positive acknowledgement: drop every buffered MSU at or before BSN.
        let mut freed = false;
        while let Some(front) = self.unacked.front() {
            // front is acknowledged when BSN is at or ahead of its FSN.
            if (seq_forward(bsn, front.fsn) as u16) < SEQ_HALF {
                self.unacked.pop_front();
                freed = true;
            } else {
                break;
            }
        }
        if freed {
            self.last_acked = bsn;
            self.pcr_cursor = 0;
            if let Some(cursor) = self.basic_cursor.as_mut() {
                // Buffer shrank from the front; keep the retransmit cursor valid.
                *cursor = cursor.saturating_sub(1);
            }
        }

        // Negative acknowledgement is Basic-method only (PCR never toggles bits).
        if matches!(self.method, RetransmissionMethod::Basic) && bib != self.fib {
            self.fib = bib;
            if !self.unacked.is_empty() {
                self.basic_cursor = Some(0);
            }
        }
    }
}

/// The receive side's decision about a signal unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RxOutcome {
    /// An in-sequence MSU whose SIO + SIF is delivered to MTP3.
    Accepted { sio: u8, sif: Vec<u8> },
    /// A duplicate or otherwise ignorable SU; no action.
    Discarded,
    /// A gap was detected and (Basic method) the BIB was toggled to request
    /// retransmission.
    RetransmissionRequested,
}

/// Receive-side error correction: the sequence check that decides acceptance, and
/// the BSN/BIB the transmit side echoes back to the peer.
#[derive(Debug, Clone)]
pub struct RxControl {
    method: RetransmissionMethod,
    /// FSN of the last accepted MSU. Starts at 127; the first expected FSN is 0.
    last_received: u8,
    /// Backward Indicator Bit. Initial value 1 (Q.703 §5).
    bib: bool,
    /// Whether a gap is currently outstanding (Basic: BIB already toggled once).
    nack_pending: bool,
}

impl RxControl {
    /// Create a receive controller for `method`, with the Q.703 initial sequence
    /// state (BSN 127, BIB 1).
    pub fn new(method: RetransmissionMethod) -> Self {
        Self {
            method,
            last_received: (SEQ_MODULUS - 1) as u8,
            bib: true,
            nack_pending: false,
        }
    }

    /// The BSN to place in outgoing SUs: the FSN of the last accepted MSU.
    pub fn bsn(&self) -> u8 {
        self.last_received
    }

    /// The BIB to place in outgoing SUs.
    pub fn bib(&self) -> bool {
        self.bib
    }

    /// Process a received MSU's forward sequence number and payload, returning
    /// whether it is delivered, discarded, or provokes a retransmission request.
    pub fn on_msu(&mut self, fsn: u8, sio: u8, sif: Vec<u8>) -> RxOutcome {
        let dist = seq_forward(fsn, self.last_received);
        if dist == 1 {
            // In sequence: accept and advance.
            self.last_received = fsn;
            self.nack_pending = false;
            return RxOutcome::Accepted { sio, sif };
        }

        // Not the next expected FSN.
        let is_gap = (dist as u16) < SEQ_HALF; // fsn is ahead → something was lost
        match self.method {
            RetransmissionMethod::Basic if is_gap => {
                if !self.nack_pending {
                    self.bib = !self.bib; // toggle once to negatively acknowledge
                    self.nack_pending = true;
                }
                RxOutcome::RetransmissionRequested
            }
            // Duplicates (fsn behind) under either method, and any out-of-sequence
            // SU under PCR, are simply dropped - no bit toggling.
            _ => RxOutcome::Discarded,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msu_body(frame: &TxFrame) -> Option<(u8, &[u8])> {
        match &frame.body {
            TxBody::Msu { sio, sif } => Some((*sio, sif)),
            TxBody::Fisu => None,
        }
    }

    #[test]
    fn basic_assigns_sequential_fsns_from_zero() {
        let mut tx = TxControl::new(RetransmissionMethod::Basic);
        tx.submit(0x83, vec![1]);
        tx.submit(0x83, vec![2]);
        let f0 = tx.poll();
        let f1 = tx.poll();
        assert_eq!(f0.fsn, 0);
        assert_eq!(f1.fsn, 1);
        assert_eq!(tx.unacked_len(), 2);
    }

    #[test]
    fn basic_positive_ack_frees_buffer() {
        let mut tx = TxControl::new(RetransmissionMethod::Basic);
        tx.submit(0x83, vec![1]);
        tx.submit(0x83, vec![2]);
        tx.poll();
        tx.poll();
        assert_eq!(tx.unacked_len(), 2);
        tx.on_ack(1, true); // BSN 1 acknowledges FSN 0 and 1
        assert_eq!(tx.unacked_len(), 0);
        // Idle now → FISU.
        assert_eq!(tx.poll().body, TxBody::Fisu);
    }

    #[test]
    fn basic_negative_ack_retransmits_from_buffer() {
        // Transmitter sends FSN 0,1,2; peer negatively acknowledges after FSN 0.
        let mut tx = TxControl::new(RetransmissionMethod::Basic);
        for i in 0..3u8 {
            tx.submit(0x83, vec![i]);
            tx.poll();
        }
        assert_eq!(tx.unacked_len(), 3);
        // Peer acks FSN 0 and toggles BIB (was 1 → 0): NACK requesting 1,2.
        tx.on_ack(0, false);
        assert_eq!(tx.unacked_len(), 2); // FSN 0 freed
        assert!(!tx.fib()); // FIB now echoes the toggled BIB
                            // Next polls retransmit FSN 1 then FSN 2 from the buffer.
        let r1 = tx.poll();
        let r2 = tx.poll();
        assert_eq!(r1.fsn, 1);
        assert_eq!(msu_body(&r1).unwrap().1, &[1]);
        assert_eq!(r2.fsn, 2);
        assert_eq!(msu_body(&r2).unwrap().1, &[2]);
        // Buffer exhausted → back to FISU.
        assert_eq!(tx.poll().body, TxBody::Fisu);
    }

    #[test]
    fn rx_accepts_in_sequence_and_delivers() {
        let mut rx = RxControl::new(RetransmissionMethod::Basic);
        let out = rx.on_msu(0, 0x83, vec![0xAA]);
        assert_eq!(
            out,
            RxOutcome::Accepted {
                sio: 0x83,
                sif: vec![0xAA]
            }
        );
        assert_eq!(rx.bsn(), 0);
    }

    #[test]
    fn rx_basic_gap_toggles_bib_once() {
        let mut rx = RxControl::new(RetransmissionMethod::Basic);
        rx.on_msu(0, 0x83, vec![0]); // accept FSN 0
        let bib_before = rx.bib();
        // FSN 2 arrives (1 was lost) → gap → NACK, BIB toggles.
        assert_eq!(
            rx.on_msu(2, 0x83, vec![2]),
            RxOutcome::RetransmissionRequested
        );
        assert_eq!(rx.bib(), !bib_before);
        assert_eq!(rx.bsn(), 0); // still acknowledging FSN 0
                                 // A second out-of-sequence SU does not toggle again.
        assert_eq!(
            rx.on_msu(3, 0x83, vec![3]),
            RxOutcome::RetransmissionRequested
        );
        assert_eq!(rx.bib(), !bib_before);
    }

    #[test]
    fn basic_lost_msu_end_to_end_recovers_in_order() {
        // Model a lossy link between one TxControl and one RxControl (Basic).
        let mut tx = TxControl::new(RetransmissionMethod::Basic);
        let mut rx = RxControl::new(RetransmissionMethod::Basic);
        for i in 0..3u8 {
            tx.submit(0x83, vec![i]);
        }
        let mut delivered: Vec<u8> = Vec::new();

        // Send FSN 0 → delivered.
        let f0 = tx.poll();
        if let TxBody::Msu { sio, sif } = f0.body.clone() {
            if let RxOutcome::Accepted { sif, .. } = rx.on_msu(f0.fsn, sio, sif) {
                delivered.push(sif[0]);
            }
        }
        // Send FSN 1 but LOSE it (never handed to rx).
        let _f1_lost = tx.poll();
        // Send FSN 2 → rx sees a gap, NACKs.
        let f2 = tx.poll();
        if let TxBody::Msu { sio, sif } = f2.body.clone() {
            assert_eq!(
                rx.on_msu(f2.fsn, sio, sif),
                RxOutcome::RetransmissionRequested
            );
        }
        // The NACK (rx.bsn=0, rx.bib toggled) reaches tx.
        tx.on_ack(rx.bsn(), rx.bib());
        // tx retransmits FSN 1 then FSN 2; both now delivered in order.
        for _ in 0..2 {
            let r = tx.poll();
            if let TxBody::Msu { sio, sif } = r.body.clone() {
                if let RxOutcome::Accepted { sif, .. } = rx.on_msu(r.fsn, sio, sif) {
                    delivered.push(sif[0]);
                }
            }
        }
        assert_eq!(delivered, vec![0, 1, 2]);
    }

    #[test]
    fn pcr_cyclically_retransmits_when_idle() {
        let params = PcrParams::default();
        let mut tx = TxControl::new(RetransmissionMethod::Pcr(params));
        tx.submit(0x83, vec![1]);
        tx.submit(0x83, vec![2]);
        let f0 = tx.poll(); // new FSN 0
        let f1 = tx.poll(); // new FSN 1
        assert_eq!(f0.fsn, 0);
        assert_eq!(f1.fsn, 1);
        // Idle now: cyclic retransmission replays FSN 0, 1, 0, 1, ...
        assert_eq!(tx.poll().fsn, 0);
        assert_eq!(tx.poll().fsn, 1);
        assert_eq!(tx.poll().fsn, 0);
        // No BIB toggling in PCR: FIB is unchanged.
        assert!(tx.fib());
    }

    #[test]
    fn pcr_never_nacks_and_recovers_by_cyclic_retransmit() {
        let params = PcrParams::default();
        let mut tx = TxControl::new(RetransmissionMethod::Pcr(params));
        let mut rx = RxControl::new(RetransmissionMethod::Pcr(params));
        tx.submit(0x83, vec![10]);
        tx.submit(0x83, vec![11]);
        let mut delivered = Vec::new();

        let f0 = tx.poll(); // FSN 0
        if let TxBody::Msu { sio, sif } = f0.body.clone() {
            if let RxOutcome::Accepted { sif, .. } = rx.on_msu(f0.fsn, sio, sif) {
                delivered.push(sif[0]);
            }
        }
        // FSN 1 sent but LOST.
        let _lost = tx.poll();
        // Idle: PCR retransmits cyclically. FSN 0 (duplicate → discarded), then FSN 1.
        for _ in 0..2 {
            let r = tx.poll();
            if let TxBody::Msu { sio, sif } = r.body.clone() {
                match rx.on_msu(r.fsn, sio, sif) {
                    RxOutcome::Accepted { sif, .. } => delivered.push(sif[0]),
                    // A duplicate is silently dropped; PCR never requests retransmit.
                    RxOutcome::Discarded => {}
                    RxOutcome::RetransmissionRequested => panic!("PCR must not NACK"),
                }
            }
        }
        assert_eq!(delivered, vec![10, 11]);
    }

    #[test]
    fn pcr_forced_retransmission_holds_new_msus() {
        // N1 = 1: after one unacknowledged MSU, new MSUs are held.
        let params = PcrParams { n1: 1, n2: 100_000 };
        let mut tx = TxControl::new(RetransmissionMethod::Pcr(params));
        tx.submit(0x83, vec![1]);
        tx.submit(0x83, vec![2]);
        let f0 = tx.poll();
        assert_eq!(f0.fsn, 0); // first MSU goes out
                               // Now unacked_len == 1 == N1 → forced: the second MSU is held, FSN 0 replays.
        let f = tx.poll();
        assert_eq!(f.fsn, 0);
        assert_eq!(tx.unacked_len(), 1);
    }
}

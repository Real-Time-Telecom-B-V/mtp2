//! Signal-unit framing per ITU-T Q.703 §2.
//!
//! MTP2 carries exactly three kinds of signal unit (SU) over a signalling link:
//!
//! * **FISU** - Fill-In Signal Unit. Header only; keeps the link busy and carries
//!   acknowledgements when there is no traffic.
//! * **LSSU** - Link Status Signal Unit. Header + a 1- or 2-octet Status Field
//!   carrying one of the six status indications that drive alignment and flow
//!   control.
//! * **MSU** - Message Signal Unit. Header + Service Information Octet (SIO) +
//!   Service Information Field (SIF); this is the MTP3 payload.
//!
//! # What this module models, and what it deliberately does not
//!
//! On a real E1/T1 timeslot the layer-1 framer wraps every SU between opening and
//! closing flags (`01111110`), performs zero-bit stuffing so the flag pattern
//! never appears inside the SU, and appends a 16-bit CRC (the "check bits", CK)
//! computed over the SU. Those three jobs - flag delimitation, bit stuffing, and
//! the CRC-16 - belong to the hardware framer (or a future card-driver binding),
//! not to this state machine. This module therefore treats a signal unit as the
//! **content between the flags and before the check bits**: the BSN/BIB, FSN/FIB
//! and LI header, plus the Status Field (LSSU) or SIO+SIF (MSU). Where a real
//! framer would insert the flags and CK is called out in the field diagram below.
//!
//! ```text
//!  transmission order →
//! +--------+--------+-----------+-----+-----------+--------+
//! |  Flag  |   CK   | SIF / SF  | SIO |  header   |  Flag  |
//! | (L1)   | (L1)   | (this mod)      | (this mod)| (L1)   |
//! +--------+--------+-----------+-----+-----------+--------+
//! ```
//!
//! # Header octet layout (Q.703 §2.2)
//!
//! Three octets precede the body. Each octet is drawn LSB (bit 1) on the right:
//!
//! ```text
//!  octet 0:  B B B B B B B  | I      BSN (bits 1-7)  + BIB (bit 8)
//!  octet 1:  F F F F F F F  | I      FSN (bits 1-7)  + FIB (bit 8)
//!  octet 2:  L L L L L L  | s s      LI  (bits 1-6)  + 2 spare bits
//! ```
//!
//! The Length Indicator selects the SU type: `0` → FISU, `1`/`2` → LSSU (one or
//! two status octets), `3..=63` → MSU. Because a 6-bit LI saturates at 63, an MSU
//! whose SIO+SIF is 63 octets or longer always carries `LI = 63`; the true length
//! comes from the flag delimitation at layer 1.

use std::fmt;

use crate::error::Mtp2Error;

/// LI value for a Fill-In Signal Unit.
pub const LI_FISU: u8 = 0;
/// LI value for a one-octet Link Status Signal Unit.
pub const LI_LSSU_ONE: u8 = 1;
/// LI value for a two-octet Link Status Signal Unit.
pub const LI_LSSU_TWO: u8 = 2;
/// Largest value the 6-bit Length Indicator can hold; also the saturating LI for
/// any MSU whose SIO+SIF is this long or longer.
pub const LI_MAX: u8 = 63;
/// Largest sequence number the 7-bit BSN/FSN fields can hold.
pub const SEQ_MODULUS: u16 = 128;

/// The six status indications carried in an LSSU Status Field (Q.703 §11).
///
/// The 3-bit code lives in the least-significant bits of the first status octet;
/// the remaining bits (and the optional second octet) are spare.
///
/// ```text
///   code  mnemonic  meaning
///   ----  --------  -------------------------
///    0    SIO       Status Indication "O"  - out of alignment
///    1    SIN       Status Indication "N"  - normal alignment
///    2    SIE       Status Indication "E"  - emergency alignment
///    3    SIOS      Status Indication "OS" - out of service
///    4    SIPO      Status Indication "PO" - processor outage
///    5    SIB       Status Indication "B"  - busy
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StatusIndication {
    /// SIO - out of alignment.
    OutOfAlignment = 0,
    /// SIN - normal alignment.
    NormalAlignment = 1,
    /// SIE - emergency alignment.
    EmergencyAlignment = 2,
    /// SIOS - out of service.
    OutOfService = 3,
    /// SIPO - processor outage.
    ProcessorOutage = 4,
    /// SIB - busy.
    Busy = 5,
}

impl StatusIndication {
    /// Parse the 3-bit status code from an LSSU status octet.
    pub fn from_u8(value: u8) -> Result<Self, Mtp2Error> {
        match value & 0x07 {
            0 => Ok(Self::OutOfAlignment),
            1 => Ok(Self::NormalAlignment),
            2 => Ok(Self::EmergencyAlignment),
            3 => Ok(Self::OutOfService),
            4 => Ok(Self::ProcessorOutage),
            5 => Ok(Self::Busy),
            other => Err(Mtp2Error::InvalidStatusIndication(other)),
        }
    }

    /// The short Q.703 mnemonic (`SIO`, `SIN`, `SIE`, `SIOS`, `SIPO`, `SIB`).
    pub fn mnemonic(self) -> &'static str {
        match self {
            Self::OutOfAlignment => "SIO",
            Self::NormalAlignment => "SIN",
            Self::EmergencyAlignment => "SIE",
            Self::OutOfService => "SIOS",
            Self::ProcessorOutage => "SIPO",
            Self::Busy => "SIB",
        }
    }
}

impl fmt::Display for StatusIndication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.mnemonic())
    }
}

/// The signal-unit header common to every SU: the two sequence numbers with their
/// indicator bits (Q.703 §2.2). The Length Indicator is not stored here - it is a
/// function of the SU type and is derived on [`SignalUnit::encode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuHeader {
    /// Backward Sequence Number (7-bit): FSN of the last MSU accepted from the peer.
    pub bsn: u8,
    /// Backward Indicator Bit: toggled to negatively acknowledge (Basic method).
    pub bib: bool,
    /// Forward Sequence Number (7-bit): sequence number of this SU's MSU.
    pub fsn: u8,
    /// Forward Indicator Bit: echoes the peer's BIB after a retransmission request.
    pub fib: bool,
}

impl SuHeader {
    /// Build a header, validating that both sequence numbers fit the 7-bit field.
    pub fn new(bsn: u8, bib: bool, fsn: u8, fib: bool) -> Result<Self, Mtp2Error> {
        if bsn as u16 >= SEQ_MODULUS {
            return Err(Mtp2Error::SequenceNumberOutOfRange(bsn as u16));
        }
        if fsn as u16 >= SEQ_MODULUS {
            return Err(Mtp2Error::SequenceNumberOutOfRange(fsn as u16));
        }
        Ok(Self { bsn, bib, fsn, fib })
    }

    /// Encode the two header sequence octets (BSN+BIB, FSN+FIB). The LI octet is
    /// written by [`SignalUnit::encode`], which knows the SU type.
    fn encode_seq_octets(&self) -> [u8; 2] {
        let bsn = (self.bsn & 0x7f) | if self.bib { 0x80 } else { 0 };
        let fsn = (self.fsn & 0x7f) | if self.fib { 0x80 } else { 0 };
        [bsn, fsn]
    }

    /// Decode BSN/BIB/FSN/FIB from the first two header octets.
    fn decode_seq_octets(b0: u8, b1: u8) -> Self {
        Self {
            bsn: b0 & 0x7f,
            bib: b0 & 0x80 != 0,
            fsn: b1 & 0x7f,
            fib: b1 & 0x80 != 0,
        }
    }
}

impl fmt::Display for SuHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bsn={} bib={} fsn={} fib={}",
            self.bsn, self.bib as u8, self.fsn, self.fib as u8
        )
    }
}

/// A signal unit: the FISU / LSSU / MSU triad of Q.703 §2.
///
/// Every variant carries the common [`SuHeader`]; the Length Indicator that
/// selects the variant on the wire is computed at encode time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalUnit {
    /// Fill-In Signal Unit (LI = 0). Header only.
    Fisu { header: SuHeader },
    /// Link Status Signal Unit (LI = 1 or 2). Carries a status indication.
    Lssu {
        header: SuHeader,
        status: StatusIndication,
        /// `true` for the two-octet status field (LI = 2); the second octet is
        /// spare. Most implementations send the one-octet form.
        extended: bool,
    },
    /// Message Signal Unit (LI = 3..=63). Carries the MTP3 SIO + SIF.
    Msu {
        header: SuHeader,
        /// Service Information Octet (network indicator + service indicator).
        sio: u8,
        /// Service Information Field (the MTP3 routing label + payload).
        sif: Vec<u8>,
    },
}

impl SignalUnit {
    /// Construct a FISU with the given acknowledgement header.
    pub fn fisu(header: SuHeader) -> Self {
        Self::Fisu { header }
    }

    /// Construct a one-octet LSSU carrying `status`.
    pub fn lssu(header: SuHeader, status: StatusIndication) -> Self {
        Self::Lssu {
            header,
            status,
            extended: false,
        }
    }

    /// Construct an MSU, validating the SIO+SIF against the link maximum.
    pub fn msu(header: SuHeader, sio: u8, sif: Vec<u8>) -> Result<Self, Mtp2Error> {
        let body = 1 + sif.len();
        if body > crate::MAX_MSU_BODY {
            return Err(Mtp2Error::MsuTooLarge(body));
        }
        Ok(Self::Msu { header, sio, sif })
    }

    /// The common header of any SU.
    pub fn header(&self) -> SuHeader {
        match self {
            Self::Fisu { header } | Self::Lssu { header, .. } | Self::Msu { header, .. } => *header,
        }
    }

    /// The Length Indicator this SU encodes to (Q.703 §2.3.3). For an MSU whose
    /// SIO+SIF is 63 octets or longer this saturates at [`LI_MAX`].
    pub fn length_indicator(&self) -> u8 {
        match self {
            Self::Fisu { .. } => LI_FISU,
            Self::Lssu { extended, .. } => {
                if *extended {
                    LI_LSSU_TWO
                } else {
                    LI_LSSU_ONE
                }
            }
            Self::Msu { sif, .. } => {
                let body = 1 + sif.len();
                if body >= LI_MAX as usize {
                    LI_MAX
                } else {
                    body as u8
                }
            }
        }
    }

    /// Serialise the SU to its on-link content (header + body), excluding the
    /// layer-1 flags and CRC-16 check bits.
    pub fn encode(&self) -> Result<Vec<u8>, Mtp2Error> {
        let header = self.header();
        // Re-validate sequence numbers so a hand-built header can't smuggle an
        // out-of-range value onto the wire.
        SuHeader::new(header.bsn, header.bib, header.fsn, header.fib)?;

        let seq = header.encode_seq_octets();
        let li = self.length_indicator();
        let mut out = Vec::with_capacity(3 + 2);
        out.push(seq[0]);
        out.push(seq[1]);
        out.push(li & 0x3f);

        match self {
            Self::Fisu { .. } => {}
            Self::Lssu {
                status, extended, ..
            } => {
                out.push(*status as u8 & 0x07);
                if *extended {
                    out.push(0); // spare second status octet
                }
            }
            Self::Msu { sio, sif, .. } => {
                let body = 1 + sif.len();
                if body > crate::MAX_MSU_BODY {
                    return Err(Mtp2Error::MsuTooLarge(body));
                }
                out.push(*sio);
                out.extend_from_slice(sif);
            }
        }
        Ok(out)
    }

    /// Parse an SU from its on-link content (header + body). The caller has
    /// already stripped the layer-1 flags and validated/removed the CRC-16.
    pub fn decode(bytes: &[u8]) -> Result<Self, Mtp2Error> {
        if bytes.len() < 3 {
            return Err(Mtp2Error::TooShort {
                expected: 3,
                actual: bytes.len(),
            });
        }
        let header = SuHeader::decode_seq_octets(bytes[0], bytes[1]);
        let li = bytes[2] & 0x3f;
        let body = &bytes[3..];

        match li {
            LI_FISU => {
                if !body.is_empty() {
                    return Err(Mtp2Error::LengthIndicatorMismatch {
                        li,
                        octets: body.len(),
                    });
                }
                Ok(Self::Fisu { header })
            }
            LI_LSSU_ONE | LI_LSSU_TWO => {
                let want = li as usize;
                if body.len() != want {
                    return Err(Mtp2Error::LengthIndicatorMismatch {
                        li,
                        octets: body.len(),
                    });
                }
                let status = StatusIndication::from_u8(body[0])?;
                Ok(Self::Lssu {
                    header,
                    status,
                    extended: li == LI_LSSU_TWO,
                })
            }
            _ => {
                // MSU: body is SIO followed by SIF.
                if body.is_empty() {
                    return Err(Mtp2Error::TooShort {
                        expected: 4,
                        actual: bytes.len(),
                    });
                }
                // For LI < 63 the indicator states the exact body length; at 63 it
                // saturates and the real length comes from the frame delimiter.
                if li < LI_MAX && body.len() != li as usize {
                    return Err(Mtp2Error::LengthIndicatorMismatch {
                        li,
                        octets: body.len(),
                    });
                }
                if li == LI_MAX && body.len() < LI_MAX as usize {
                    return Err(Mtp2Error::LengthIndicatorMismatch {
                        li,
                        octets: body.len(),
                    });
                }
                let sio = body[0];
                let sif = body[1..].to_vec();
                let total = 1 + sif.len();
                if total > crate::MAX_MSU_BODY {
                    return Err(Mtp2Error::MsuTooLarge(total));
                }
                Ok(Self::Msu { header, sio, sif })
            }
        }
    }
}

impl fmt::Display for SignalUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fisu { header } => write!(f, "FISU [{header}]"),
            Self::Lssu { header, status, .. } => write!(f, "LSSU {status} [{header}]"),
            Self::Msu { header, sio, sif } => {
                write!(f, "MSU [{header}] sio={sio:#04x} sif_len={}", sif.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(bsn: u8, bib: bool, fsn: u8, fib: bool) -> SuHeader {
        SuHeader::new(bsn, bib, fsn, fib).unwrap()
    }

    // ── Known-answer vectors (exact bytes, hand-assembled from Q.703 §2.2) ──────

    #[test]
    fn fisu_kat_exact_bytes() {
        // bsn=0 bib=1 fsn=0 fib=1 → octet0 0x80, octet1 0x80, LI=0.
        let su = SignalUnit::fisu(hdr(0, true, 0, true));
        assert_eq!(su.encode().unwrap(), vec![0x80, 0x80, 0x00]);
        assert_eq!(su.length_indicator(), 0);
    }

    #[test]
    fn lssu_sin_kat_exact_bytes() {
        // bsn=127 bib=0 fsn=0 fib=1, SIN (=1), one-octet SF, LI=1.
        // octet0 0x7f, octet1 0x80, LI 0x01, SF 0x01.
        let su = SignalUnit::lssu(hdr(127, false, 0, true), StatusIndication::NormalAlignment);
        assert_eq!(su.encode().unwrap(), vec![0x7f, 0x80, 0x01, 0x01]);
    }

    #[test]
    fn lssu_sios_two_octet_kat() {
        // SIOS (=3), two-octet SF (LI=2), spare second octet 0.
        let su = SignalUnit::Lssu {
            header: hdr(0, false, 0, false),
            status: StatusIndication::OutOfService,
            extended: true,
        };
        assert_eq!(su.encode().unwrap(), vec![0x00, 0x00, 0x02, 0x03, 0x00]);
    }

    #[test]
    fn msu_kat_exact_bytes() {
        // bsn=5 bib=1 fsn=6 fib=1, SIO=0x83, SIF=[01 02 03], body=4 → LI=4.
        // octet0 0x85, octet1 0x86, LI 0x04, SIO 0x83, SIF 01 02 03.
        let su = SignalUnit::msu(hdr(5, true, 6, true), 0x83, vec![0x01, 0x02, 0x03]).unwrap();
        assert_eq!(
            su.encode().unwrap(),
            vec![0x85, 0x86, 0x04, 0x83, 0x01, 0x02, 0x03]
        );
        assert_eq!(su.length_indicator(), 4);
    }

    #[test]
    fn decode_real_shaped_hex_fields() {
        // Decode an MSU hex string and assert every parsed field.
        let bytes = hex::decode("85860483010203").unwrap();
        let su = SignalUnit::decode(&bytes).unwrap();
        match su {
            SignalUnit::Msu { header, sio, sif } => {
                assert_eq!(header.bsn, 5);
                assert!(header.bib);
                assert_eq!(header.fsn, 6);
                assert!(header.fib);
                assert_eq!(sio, 0x83);
                assert_eq!(sif, vec![0x01, 0x02, 0x03]);
            }
            other => panic!("expected MSU, got {other}"),
        }
    }

    #[test]
    fn decode_lssu_status_fields() {
        // One-octet SIE.
        let su = SignalUnit::decode(&hex::decode("00000102").unwrap()).unwrap();
        match su {
            SignalUnit::Lssu {
                status, extended, ..
            } => {
                assert_eq!(status, StatusIndication::EmergencyAlignment);
                assert!(!extended);
            }
            other => panic!("expected LSSU, got {other}"),
        }
    }

    // ── Round trips + validation ────────────────────────────────────────────────

    #[test]
    fn round_trip_all_status_indications() {
        for code in 0u8..=5 {
            let status = StatusIndication::from_u8(code).unwrap();
            let su = SignalUnit::lssu(hdr(1, false, 2, true), status);
            let decoded = SignalUnit::decode(&su.encode().unwrap()).unwrap();
            assert_eq!(su, decoded);
        }
    }

    #[test]
    fn status_indication_rejects_reserved_code() {
        assert_eq!(
            StatusIndication::from_u8(6),
            Err(Mtp2Error::InvalidStatusIndication(6))
        );
        assert_eq!(
            StatusIndication::from_u8(7),
            Err(Mtp2Error::InvalidStatusIndication(7))
        );
    }

    #[test]
    fn decode_rejects_too_short() {
        assert_eq!(
            SignalUnit::decode(&[0x00, 0x00]),
            Err(Mtp2Error::TooShort {
                expected: 3,
                actual: 2
            })
        );
    }

    #[test]
    fn decode_rejects_li_body_mismatch() {
        // LI says FISU (0) but a body follows.
        assert!(matches!(
            SignalUnit::decode(&[0x00, 0x00, 0x00, 0xAA]),
            Err(Mtp2Error::LengthIndicatorMismatch { .. })
        ));
        // LI says 1-octet LSSU but two body octets follow.
        assert!(matches!(
            SignalUnit::decode(&[0x00, 0x00, 0x01, 0x01, 0x00]),
            Err(Mtp2Error::LengthIndicatorMismatch { .. })
        ));
    }

    #[test]
    fn large_msu_saturates_li_at_63() {
        // A 100-octet SIF (+ SIO) exceeds the 6-bit LI, which saturates at 63.
        let sif = vec![0xABu8; 100];
        let su = SignalUnit::msu(hdr(0, false, 0, false), 0x83, sif.clone()).unwrap();
        assert_eq!(su.length_indicator(), LI_MAX);
        let decoded = SignalUnit::decode(&su.encode().unwrap()).unwrap();
        match decoded {
            SignalUnit::Msu { sif: got, .. } => assert_eq!(got, sif),
            other => panic!("expected MSU, got {other}"),
        }
    }

    #[test]
    fn msu_body_over_maximum_rejected() {
        let sif = vec![0u8; crate::MAX_MSU_BODY]; // +SIO pushes past the max
        assert!(matches!(
            SignalUnit::msu(hdr(0, false, 0, false), 0x83, sif),
            Err(Mtp2Error::MsuTooLarge(_))
        ));
    }

    #[test]
    fn header_rejects_out_of_range_sequence() {
        assert_eq!(
            SuHeader::new(128, false, 0, false),
            Err(Mtp2Error::SequenceNumberOutOfRange(128))
        );
    }

    #[test]
    fn display_forms() {
        assert_eq!(StatusIndication::ProcessorOutage.to_string(), "SIPO");
        let su = SignalUnit::fisu(hdr(3, false, 4, true));
        assert!(su.to_string().starts_with("FISU"));
    }
}

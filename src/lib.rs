//! MTP2 - Message Transfer Part Level 2 (ITU-T Q.703) signal-unit framing and
//! signalling-link state machine, in pure Rust.
//!
//! MTP2 is the SS7 data-link layer: it turns a single 64 kbit/s (or n×64)
//! timeslot on an E1/T1 span into a reliable signalling link for MTP3. It is the
//! layer that [`m2pa`](https://crates.io/crates/m2pa) replaces when the same
//! MTP3 traffic rides SCTP/IP instead of TDM - so this crate slots underneath an
//! MTP3 exactly where m2pa does, exposing the same shape (signal-unit framing +
//! a link state machine + a driver), just over a real timeslot.
//!
//! # Scope: the pure Q.703 core, no hardware I/O
//!
//! This crate is deterministic and I/O-free. It models:
//!
//! * **Signal-unit framing** ([`signal_unit`]) - FISU, LSSU and MSU, with the
//!   BSN/BIB, FSN/FIB and Length-Indicator header.
//! * **The link state machine** ([`link`]) - initial alignment, proving, in
//!   service, processor outage, and flow control, driven by the LSSU status
//!   exchange (SIO/SIN/SIE/SIOS/SIPO/SIB).
//! * **The error-rate monitors** ([`monitor`]) - AERM (proving) and SUERM (in
//!   service).
//! * **Both retransmission methods** ([`retransmission`]) - Basic (negative
//!   acknowledgement) and Preventive Cyclic Retransmission.
//!
//! ## What is deliberately out of scope (the layer-1 framer's job)
//!
//! On a real span, an E1/T1 line framer inserts the opening/closing flags
//! (`01111110`), performs zero-bit stuffing, and appends the 16-bit CRC "check
//! bits" (CK). Those functions belong to the hardware and are **not** in this
//! crate; a signal unit here is the content *between* the flags and *before* the
//! CK. Binding this state machine to a specific E1/T1 card driver is a separate,
//! future concern - it would live behind its own feature flag or in a sibling
//! crate, and is not built here (no card, no ioctl, no libc I/O in this crate).
//!
//! # Example
//!
//! ```
//! use mtp2::{SignalUnit, SuHeader, StatusIndication};
//!
//! // Build an LSSU carrying "normal alignment" (SIN) and serialise it.
//! let su = SignalUnit::lssu(
//!     SuHeader::new(0, true, 0, true).unwrap(),
//!     StatusIndication::NormalAlignment,
//! );
//! let bytes = su.encode().unwrap();
//! assert_eq!(SignalUnit::decode(&bytes).unwrap(), su);
//! ```

pub mod error;
pub mod link;
pub mod monitor;
pub mod retransmission;
pub mod signal_unit;

#[cfg(feature = "python")]
pub mod python;

pub use error::Mtp2Error;
pub use link::{AlignmentFailure, Event, Mtp2Config, Mtp2Link, Mtp2State, OutOfServiceReason};
pub use retransmission::{PcrParams, RetransmissionMethod, RxOutcome};
pub use signal_unit::{SignalUnit, StatusIndication, SuHeader};

/// Maximum MSU body (SIO + SIF) an MTP2 link carries, in octets. The Service
/// Information Field is at most 272 octets (Q.703 §2.3.8); the Service
/// Information Octet adds one.
pub const MAX_MSU_BODY: usize = 273;

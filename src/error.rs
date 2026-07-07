/// Errors that can occur while framing or parsing MTP2 signal units (ITU-T Q.703).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Mtp2Error {
    /// A signal unit was shorter than its mandatory header.
    #[error("signal unit too short: expected at least {expected} octets, got {actual}")]
    TooShort { expected: usize, actual: usize },

    /// The Length Indicator did not match the signal unit body that followed it.
    ///
    /// LI ∈ {0} → FISU, {1, 2} → LSSU, {3..=63} → MSU (Q.703 §2.3.3).
    #[error("length indicator {li} inconsistent with a {octets}-octet body")]
    LengthIndicatorMismatch { li: u8, octets: usize },

    /// The 3-bit status indication in an LSSU status field was not one of the
    /// six defined codes (Q.703 §11).
    #[error("invalid status indication: {0}")]
    InvalidStatusIndication(u8),

    /// A sequence number (BSN/FSN) exceeded its 7-bit field (0..=127).
    #[error("sequence number {0} out of range (must be 0..=127)")]
    SequenceNumberOutOfRange(u16),

    /// An MSU's SIF+SIO exceeded the maximum a signalling link may carry.
    #[error("message signal unit body too large: {0} octets (max {max})", max = crate::MAX_MSU_BODY)]
    MsuTooLarge(usize),
}

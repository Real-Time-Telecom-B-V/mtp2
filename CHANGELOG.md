# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). See
[VERSIONING.md](VERSIONING.md) for the compatibility policy.

## [1.0.0]

First published release - the MTP2 (ITU-T Q.703) signal-unit codec + link state
machine, shipped as both a Rust crate (crates.io) and a Rust-backed Python wheel
(PyPI) from one source tree.

### Added
- **Signal-unit framing** - `SignalUnit` (FISU / LSSU / MSU) encode/decode with
  the `SuHeader` (BSN+BIB, FSN+FIB, 6-bit Length Indicator) and all six
  `StatusIndication`s (SIO/SIN/SIE/SIOS/SIPO/SIB). The unit is modelled as the
  content between the layer-1 flags and before the CRC-16 check bits.
- **Link state machine** - `Mtp2Link` / `Mtp2State`: the Q.703 lifecycle (Out of
  Service → Not Aligned → Aligned → Proving → Aligned Ready → In Service), plus
  Aligned Not Ready and Processor Outage. Driven by the SIO/SIN/SIE/SIOS status
  exchange with normal and emergency proving; I/O-free (inbound SUs + ticks in,
  outbound SUs + `Event`s out).
- **Error-rate monitors** - `monitor::Aerm` (proving) and `monitor::Suerm` (in
  service).
- **Retransmission** - both Q.703 §5 methods: Basic (FSN/BSN with BIB/FIB
  negative-acknowledgement retransmission) and Preventive Cyclic Retransmission
  (cyclic + forced N1/N2 retransmission). Flow control via SIB (busy).
- **Typed errors** - `Mtp2Error` (`thiserror`) for every rejection.
- **Python bindings** (`pip install mtp2`, feature `python`) - `Fisu`, `Lssu`,
  `Msu`, `decode()`, `StatusIndication`, `Mtp2State`, and the `Link` driver with
  its `Event`s. Declared `gil_used = false` for free-threaded CPython. A
  `register(py, parent)` entry point mounts `mtp2` as a submodule of a host
  extension.
- **Quality bar** - criterion benches (`benches/codec.rs`), a counting-allocator
  leak check (`examples/leak_check.rs` + `scripts/mem_leak_test.sh`), pytest
  parity tests, Q.703-derived integration vectors (framing KATs, the
  alignment/proving sequence, AERM/SUERM trips, Basic + PCR retransmission over a
  lossy pipe, and a link-to-link In-Service + MSU pass-through), and CI (fmt /
  clippy / test / bench-compile / leak gate / wheel + pytest / cargo-deny).

### Not included (by design)
- Layer-1 flag delimitation, zero-bit stuffing, and the CRC-16 check bits - the
  hardware framer's job.
- The E1/T1 card / driver binding (timeslot I/O) - a future feature flag or
  sibling crate.

[1.0.0]: https://github.com/Real-Time-Telecom-B-V/mtp2/releases/tag/v1.0.0

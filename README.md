# mtp2

[![crates.io](https://img.shields.io/crates/v/mtp2.svg)](https://crates.io/crates/mtp2)
[![docs.rs](https://docs.rs/mtp2/badge.svg)](https://docs.rs/mtp2)
[![CI](https://github.com/Real-Time-Telecom-B-V/mtp2/actions/workflows/ci.yaml/badge.svg)](https://github.com/Real-Time-Telecom-B-V/mtp2/actions/workflows/ci.yaml)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A pure-Rust **MTP2 ([ITU-T Q.703](https://www.itu.int/rec/T-REC-Q.703))**
signal-unit codec and signalling-link state machine - the SS7 data-link layer
that turns a single 64 kbit/s TDM timeslot into a reliable link for MTP3. It
ships as **both** a Rust crate (`cargo add mtp2`) and a Rust-backed Python wheel
(`pip install mtp2`), built from one source tree and one version.

MTP2 is the layer that [`m2pa`](https://crates.io/crates/m2pa) replaces when the
same MTP3 traffic rides SCTP/IP instead of a real E1/T1 span. `mtp2` slots
underneath an MTP3 in exactly the same place, exposing the same shape - signal
framing, a link state machine, and a driver - so MTP3 and everything above it
(SCCP/TCAP/MAP/ISUP) do not change. This crate is the **wire format** (FISU /
LSSU / MSU) plus the **Q.703 link state machine** (alignment, proving, in
service, processor outage, flow control) and **both retransmission methods**. It
does no I/O.

```rust
use mtp2::{SignalUnit, SuHeader, StatusIndication};

// Build an LSSU carrying "normal alignment" (SIN) and serialise it.
let su = SignalUnit::lssu(
    SuHeader::new(0, true, 0, true).unwrap(),
    StatusIndication::NormalAlignment,
);
let bytes = su.encode().unwrap();                 // header + status field
let decoded = SignalUnit::decode(&bytes).unwrap();
assert_eq!(decoded, su);
```

```python
import mtp2

su = mtp2.Lssu(mtp2.StatusIndication.SIN)         # a Link Status Signal Unit
wire = su.encode()                                # bytes
msg = mtp2.decode(wire)                            # -> Fisu | Lssu | Msu

link = mtp2.Link("basic")                          # the Q.703 state machine
link.start()                                       # begin initial alignment
```

## What's in the box

| Piece | Type |
|---|---|
| Signal-unit header - BSN+BIB, FSN+FIB, LI | `SuHeader` |
| FISU / LSSU / MSU framing with encode/decode | `SignalUnit` |
| The six LSSU status indications (SIO/SIN/SIE/SIOS/SIPO/SIB) | `StatusIndication` |
| Link state machine (initial alignment … in service) | `Mtp2Link`, `Mtp2State` |
| Error-rate monitors - AERM (proving), SUERM (in service) | `monitor::Aerm`, `monitor::Suerm` |
| Retransmission - Basic (NACK) and PCR (cyclic) | `RetransmissionMethod` |
| Link events for MTP3 | `Event` |
| Typed errors | `Mtp2Error` |

## Q.703 coverage

| Feature | Status |
|---|---|
| Signal-unit framing - FISU (LI 0), LSSU (LI 1/2), MSU (LI 3..=63) | ✅ pack/unpack + validation |
| SU header - BSN/BIB, FSN/FIB (7-bit + indicator), 6-bit LI | ✅ |
| Status field - all six indications (§11) | ✅ `StatusIndication` |
| Link state control - OOS → Not Aligned → Aligned → Proving → Aligned Ready → In Service | ✅ `Mtp2Link` |
| Initial alignment control - SIO/SIN/SIE/SIOS exchange, normal vs emergency proving | ✅ |
| AERM (proving) / SUERM (in service) error-rate monitors | ✅ `monitor` |
| Processor outage (SIPO) and busy / flow control (SIB) | ✅ |
| Basic error correction - FSN/BSN, BIB/FIB negative acknowledgement | ✅ |
| Preventive Cyclic Retransmission - cyclic + forced (N1/N2) retransmission | ✅ |
| Link timers T1-T4, T6, T7 | ✅ tick-driven |
| Flag delimitation, zero-bit stuffing, CRC-16 check bits | ⛔ layer-1 framer's job (see below) |
| E1/T1 card / driver binding (timeslot I/O) | ⛔ future feature / sibling crate (see below) |

## Boundary: what this crate does and doesn't do

MTP2's job splits cleanly:

- **This crate (pure, no I/O):** serialise/parse FISU/LSSU/MSU, run the Q.703
  link state machine (alignment/proving/in-service, AERM/SUERM, processor
  outage, flow control), and drive both retransmission methods. The engine
  consumes inbound SUs plus timer ticks and emits outbound SUs plus events, so it
  is fully unit-testable and can be wired link-to-link over an in-memory pipe.
- **The layer-1 framer (hardware):** on a real span, an E1/T1 line framer inserts
  the opening/closing flags (`01111110`), performs zero-bit stuffing, and appends
  the 16-bit CRC "check bits" (CK). A signal unit in this crate is the content
  *between* the flags and *before* the CK - those three functions belong to the
  hardware, not here.

### Future work: the hardware binding

Binding this state machine to a concrete **E1/T1 card driver** (timeslot I/O over
an ioctl/DMA interface) is a separate, deliberately deferred concern. It would
live behind its own feature flag or in a sibling crate and is **not** built here
(no card ioctl, no libc I/O in this crate). Keeping the core pure is what lets the
exact same logic back the Rust crate and the Python wheel, and makes it trivial
to test against Q.703 vectors.

## Performance

Single-core, `cargo bench` ([`benches/codec.rs`](benches/codec.rs)); the codec is
allocation-light and the state machine is branch-only. A counting-allocator
[leak check](examples/leak_check.rs) (`./scripts/mem_leak_test.sh`) hammers
encode/decode and a full alignment and asserts **live bytes stay flat** (Δ 0 over
millions of cycles). Both run in CI.

The Python wheel is the same Rust code behind PyO3; per-call overhead is the
Python↔Rust boundary, not the codec. The module is declared `gil_used = false`,
so it loads on free-threaded ("no-GIL") CPython 3.13t / 3.14t.

## Install

```bash
cargo add mtp2          # Rust crate (zero pyo3 in the default build)
pip install mtp2        # Rust-backed Python wheel
```

## Development

```bash
cargo test                              # unit + integration + doctests
cargo test --features python            # + the PyO3 binding face
cargo clippy --all-targets -- -D warnings
cargo bench --no-run
./scripts/mem_leak_test.sh              # live-bytes leak check (PASS/FAIL)
cargo deny check                        # advisories, licenses, sources

# Python wheel
maturin develop && pytest python/tests -q
```

## License

MIT - see [LICENSE](LICENSE).

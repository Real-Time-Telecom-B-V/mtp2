"""mtp2 - Rust-backed MTP2 (ITU-T Q.703) signal-unit framing + link state machine.

MTP2 is the SS7 data-link layer: it turns one 64 kbit/s TDM timeslot into a
reliable signalling link for MTP3. It is the layer that ``m2pa`` replaces when
the same MTP3 traffic rides SCTP/IP instead of a real span, so this package
slots underneath an MTP3 in the same place, exposing the same shape (signal-unit
framing + a link state machine + a driver).

This package exposes the same framing and state machine the Rust crate
(``cargo add mtp2``) ships, from one source tree / one version. The wire work
(header pack/unpack, body copy, the Q.703 state transitions and error-rate
monitors) runs in Rust; Python just builds, inspects, and drives.

Layer-1 concerns (E1/T1 flag delimitation, zero-bit stuffing, the CRC-16 check
bits) and any hardware/card-driver binding are deliberately out of scope: a
signal unit here is the content between the flags and before the check bits.
"""

from __future__ import annotations

from importlib.metadata import PackageNotFoundError, version

from ._mtp2 import (
    MAX_MSU_BODY,
    Event,
    Fisu,
    Link,
    Lssu,
    Msu,
    Mtp2Error,
    Mtp2State,
    StatusIndication,
    decode,
)

try:
    __version__ = version("mtp2")
except PackageNotFoundError:  # running from a source checkout without an installed dist
    __version__ = "0.0.0+unknown"

__all__ = [
    # signal-unit framing + codec
    "Fisu",
    "Lssu",
    "Msu",
    "decode",
    "Mtp2Error",
    # enums
    "StatusIndication",
    "Mtp2State",
    # link state machine driver + its events
    "Link",
    "Event",
    # constants
    "MAX_MSU_BODY",
    "__version__",
]

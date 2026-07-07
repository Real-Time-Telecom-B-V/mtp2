"""Type stubs for the Rust-backed ``mtp2._mtp2`` extension module."""

from __future__ import annotations

# ── Constants ────────────────────────────────────────────────────────────────
MAX_MSU_BODY: int

class Mtp2Error(Exception):
    """MTP2 framing / state-machine error (ITU-T Q.703)."""

class StatusIndication:
    """The six LSSU status indications (Q.703 §11).

    A PyO3 enum: members compare equal to their on-wire 3-bit code
    (``int(StatusIndication.SIOS) == 3``), but it is not a Python
    ``enum.IntEnum`` (no iteration, no ``.value``).
    """

    SIO: StatusIndication
    SIN: StatusIndication
    SIE: StatusIndication
    SIOS: StatusIndication
    SIPO: StatusIndication
    SIB: StatusIndication
    def __int__(self) -> int: ...
    def __eq__(self, other: object) -> bool: ...
    def __hash__(self) -> int: ...

class Mtp2State:
    """The local link state tracked by :class:`Link` (Q.703 §7)."""

    OutOfService: Mtp2State
    NotAligned: Mtp2State
    Aligned: Mtp2State
    Proving: Mtp2State
    AlignedReady: Mtp2State
    AlignedNotReady: Mtp2State
    InService: Mtp2State
    ProcessorOutage: Mtp2State
    def __int__(self) -> int: ...
    def __eq__(self, other: object) -> bool: ...
    def __hash__(self) -> int: ...

class Fisu:
    """A Fill-In Signal Unit (LI = 0): header only."""

    bsn: int
    bib: bool
    fsn: int
    fib: bool
    def __init__(
        self, *, bsn: int = 127, bib: bool = True, fsn: int = 127, fib: bool = True
    ) -> None: ...
    def encode(self) -> bytes:
        """Encode the SU's header+body (no layer-1 flags/CRC)."""

class Lssu:
    """A Link Status Signal Unit (LI = 1 or 2): carries a status indication."""

    status: StatusIndication
    bsn: int
    bib: bool
    fsn: int
    fib: bool
    extended: bool
    def __init__(
        self,
        status: StatusIndication,
        *,
        bsn: int = 127,
        bib: bool = True,
        fsn: int = 127,
        fib: bool = True,
        extended: bool = False,
    ) -> None: ...
    def encode(self) -> bytes:
        """Encode the SU's header+body (no layer-1 flags/CRC)."""

class Msu:
    """A Message Signal Unit (LI = 3..=63): carries the MTP3 SIO + SIF."""

    sio: int
    sif: bytes
    bsn: int
    bib: bool
    fsn: int
    fib: bool
    def __init__(
        self,
        sio: int,
        sif: bytes,
        *,
        bsn: int = 0,
        bib: bool = True,
        fsn: int = 0,
        fib: bool = True,
    ) -> None: ...
    def encode(self) -> bytes:
        """Encode the SU's header+body (no layer-1 flags/CRC)."""

class Event:
    """An event raised by :class:`Link`."""

    kind: str
    sio: int | None
    sif: bytes | None
    detail: str | None

class Link:
    """The MTP2 signalling-link engine (Q.703 state machine driver)."""

    def __init__(
        self,
        method: str = "basic",
        *,
        emergency: bool = False,
        pcr_n1: int = 127,
        pcr_n2: int = 8000,
        t1: int | None = None,
        t2: int | None = None,
        t3: int | None = None,
        t4_normal: int | None = None,
        t4_emergency: int | None = None,
        t6: int | None = None,
        t7: int | None = None,
        aerm_normal: int | None = None,
        aerm_emergency: int | None = None,
        suerm_threshold: int | None = None,
        suerm_interval: int | None = None,
        proving_attempts: int | None = None,
    ) -> None: ...
    @property
    def state(self) -> Mtp2State: ...
    def start(self) -> None:
        """Start initial alignment."""
    def stop(self) -> None:
        """Take the link out of service."""
    def set_emergency(self, emergency: bool) -> None: ...
    def local_processor_outage(self) -> None: ...
    def local_processor_recovered(self) -> None: ...
    def local_busy(self) -> None: ...
    def local_busy_ended(self) -> None: ...
    def submit_msu(self, sio: int, sif: bytes) -> None:
        """Queue an MSU (SIO + SIF) for transmission."""
    def poll_transmit(self) -> Fisu | Lssu | Msu | None:
        """Next SU to transmit, or ``None`` when powered off."""
    def handle_su(self, data: bytes) -> None:
        """Feed a received SU (raw header+body bytes, e.g. from ``su.encode()``)."""
    def handle_corrupted_su(self) -> None:
        """Report a bad-CRC SU (feeds the AERM/SUERM)."""
    def tick(self) -> None:
        """Advance the link's timers by one tick."""
    def poll_event(self) -> Event | None:
        """Next event raised by the link, or ``None``."""

def decode(data: bytes) -> Fisu | Lssu | Msu:
    """Decode a signal unit (header+body, without flags/CRC) into its type."""

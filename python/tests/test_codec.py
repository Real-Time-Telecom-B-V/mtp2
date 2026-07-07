"""Parity / round-trip tests for the mtp2 wheel.

These exercise the same Rust framing and state machine the crate ships, through
the Python surface: ``encode`` must match the Q.703 wire form, ``decode`` must
recover the fields, the error-rate monitors must trip, and two link engines wired
back to back must reach In Service and pass an MSU.
"""

from __future__ import annotations

import pytest

import mtp2

# Q.703 wire forms (header + body, without the layer-1 flags/CRC), hand-assembled
# from §2.2: octet0 = BSN|BIB<<7, octet1 = FSN|FIB<<7, octet2 = LI, then the body.
GOLDEN_FISU = bytes.fromhex("808000")  # bsn=0 bib=1 fsn=0 fib=1, LI=0
GOLDEN_LSSU_SIN = bytes.fromhex("7f800101")  # bsn=127 fsn=0 fib=1, SIN, LI=1
GOLDEN_MSU = bytes.fromhex("85860483010203")  # bsn=5 fsn=6, SIO=0x83 SIF=010203

ALL_STATUS = [
    mtp2.StatusIndication.SIO,
    mtp2.StatusIndication.SIN,
    mtp2.StatusIndication.SIE,
    mtp2.StatusIndication.SIOS,
    mtp2.StatusIndication.SIPO,
    mtp2.StatusIndication.SIB,
]


def test_constants() -> None:
    assert mtp2.MAX_MSU_BODY == 273


def test_status_wire_codes() -> None:
    assert int(mtp2.StatusIndication.SIO) == 0
    assert int(mtp2.StatusIndication.SIOS) == 3
    assert int(mtp2.StatusIndication.SIB) == 5


def test_fisu_matches_golden() -> None:
    assert mtp2.Fisu(bsn=0, bib=True, fsn=0, fib=True).encode() == GOLDEN_FISU


def test_lssu_matches_golden() -> None:
    su = mtp2.Lssu(mtp2.StatusIndication.SIN, bsn=127, bib=False, fsn=0, fib=True)
    assert su.encode() == GOLDEN_LSSU_SIN


def test_msu_matches_golden() -> None:
    su = mtp2.Msu(0x83, bytes([1, 2, 3]), bsn=5, bib=True, fsn=6, fib=True)
    assert su.encode() == GOLDEN_MSU


def test_decode_msu_fields() -> None:
    su = mtp2.decode(GOLDEN_MSU)
    assert isinstance(su, mtp2.Msu)
    assert su.sio == 0x83
    assert su.sif == bytes([1, 2, 3])
    assert su.bsn == 5 and su.fsn == 6
    assert su.bib is True and su.fib is True


def test_decode_fisu_and_lssu_types() -> None:
    assert isinstance(mtp2.decode(GOLDEN_FISU), mtp2.Fisu)
    lssu = mtp2.decode(GOLDEN_LSSU_SIN)
    assert isinstance(lssu, mtp2.Lssu)
    assert lssu.status == mtp2.StatusIndication.SIN


@pytest.mark.parametrize("status", ALL_STATUS)
def test_lssu_round_trip_all_status(status) -> None:
    wire = mtp2.Lssu(status, bsn=1, bib=False, fsn=2, fib=True).encode()
    decoded = mtp2.decode(wire)
    assert isinstance(decoded, mtp2.Lssu)
    assert decoded.status == status
    assert decoded.encode() == wire


def test_msu_round_trip_large_sif() -> None:
    sif = bytes(range(64)) * 4  # 256 octets, forces LI to saturate at 63
    su = mtp2.Msu(0x83, sif, bsn=10, fsn=11)
    decoded = mtp2.decode(su.encode())
    assert isinstance(decoded, mtp2.Msu)
    assert decoded.sif == sif


def test_decode_rejects_truncated() -> None:
    with pytest.raises(mtp2.Mtp2Error):
        mtp2.decode(b"\x00\x00")


def test_decode_rejects_bad_status() -> None:
    # LI=1 (LSSU) with status code 6 (reserved).
    with pytest.raises(mtp2.Mtp2Error):
        mtp2.decode(bytes.fromhex("00000106"))


def _fast_link(method: str = "basic") -> "mtp2.Link":
    # Large guard timers, short proving so only T4 drives the handshake.
    return mtp2.Link(
        method,
        t1=100000,
        t2=100000,
        t3=100000,
        t4_normal=4,
        t6=100000,
        t7=100000,
    )


def test_link_alignment_to_in_service() -> None:
    link = _fast_link()
    assert link.state == mtp2.Mtp2State.OutOfService
    link.start()
    assert link.state == mtp2.Mtp2State.NotAligned

    peer_sio = mtp2.Lssu(mtp2.StatusIndication.SIO).encode()
    peer_sin = mtp2.Lssu(mtp2.StatusIndication.SIN).encode()
    link.handle_su(peer_sio)
    assert link.state == mtp2.Mtp2State.Aligned
    link.handle_su(peer_sin)
    assert link.state == mtp2.Mtp2State.Proving
    for _ in range(4):
        link.tick()
    assert link.state == mtp2.Mtp2State.AlignedReady
    link.handle_su(mtp2.Fisu(bsn=0, fsn=0).encode())
    assert link.state == mtp2.Mtp2State.InService


def test_suerm_fails_in_service_link() -> None:
    link = mtp2.Link(
        "basic",
        t1=100000,
        t2=100000,
        t3=100000,
        t4_normal=4,
        t7=100000,
        suerm_threshold=4,
        suerm_interval=100000,
    )
    _bring_in_service(link)
    for _ in range(4):
        link.handle_corrupted_su()
    assert link.state == mtp2.Mtp2State.OutOfService


def _bring_in_service(link: "mtp2.Link") -> None:
    link.start()
    link.handle_su(mtp2.Lssu(mtp2.StatusIndication.SIO).encode())
    link.handle_su(mtp2.Lssu(mtp2.StatusIndication.SIN).encode())
    for _ in range(4):
        link.tick()
    link.handle_su(mtp2.Fisu(bsn=0, fsn=0).encode())
    assert link.state == mtp2.Mtp2State.InService


def _step(a: "mtp2.Link", b: "mtp2.Link") -> None:
    a_su = a.poll_transmit()
    b_su = b.poll_transmit()
    if a_su is not None:
        b.handle_su(a_su.encode())
    if b_su is not None:
        a.handle_su(b_su.encode())
    a.tick()
    b.tick()


def test_two_links_reach_in_service_and_pass_msu() -> None:
    a, b = _fast_link(), _fast_link()
    a.start()
    b.start()
    for _ in range(64):
        _step(a, b)
        if a.state == mtp2.Mtp2State.InService and b.state == mtp2.Mtp2State.InService:
            break
    assert a.state == mtp2.Mtp2State.InService
    assert b.state == mtp2.Mtp2State.InService

    sif = bytes([0x83, 0x01, 0x00, 0x00, 0x09])
    a.submit_msu(0x83, sif)
    got = None
    for _ in range(12):
        _step(a, b)
        while (ev := b.poll_event()) is not None:
            if ev.kind == "msu":
                got = (ev.sio, ev.sif)
        if got is not None:
            break
    assert got == (0x83, sif)

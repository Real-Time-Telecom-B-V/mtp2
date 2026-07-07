//! Integration tests - MTP2 signal-unit framing against Q.703 wire vectors, the
//! alignment/proving state machine, the AERM/SUERM error-rate monitors, both
//! retransmission methods, and a full in-memory link-to-link handshake.
//!
//! Every vector is derived from ITU-T Q.703 (§2 framing, §7 link state control,
//! §11 status field), not captured traffic. MSU payloads are synthetic MTP3
//! shapes. Where a test pins a hex string, an accompanying encode assertion
//! proves the crate reproduces it.

use mtp2::{
    Event, Mtp2Config, Mtp2Link, Mtp2State, OutOfServiceReason, RetransmissionMethod, SignalUnit,
    StatusIndication, SuHeader,
};

fn hdr(bsn: u8, bib: bool, fsn: u8, fib: bool) -> SuHeader {
    SuHeader::new(bsn, bib, fsn, fib).unwrap()
}

// ── §2 Signal-unit framing (known-answer vectors) ────────────────────────────

#[test]
fn fisu_wire_form_is_three_octets() {
    // bsn=0 bib=1 fsn=0 fib=1, LI=0 → 0x80 0x80 0x00.
    let su = SignalUnit::fisu(hdr(0, true, 0, true));
    assert_eq!(su.encode().unwrap(), hex::decode("808000").unwrap());
}

#[test]
fn lssu_sios_wire_form() {
    // bsn=0 bib=0 fsn=0 fib=0, SIOS(=3), one-octet SF, LI=1 → 00 00 01 03.
    let su = SignalUnit::lssu(hdr(0, false, 0, false), StatusIndication::OutOfService);
    assert_eq!(su.encode().unwrap(), hex::decode("00000103").unwrap());
}

#[test]
fn msu_wire_form_and_decode_fields() {
    // bsn=5 bib=1 fsn=6 fib=1, SIO=0x83, SIF=[01 02 03], body=4 → LI=4.
    let bytes = hex::decode("85860483010203").unwrap();
    let su = SignalUnit::msu(hdr(5, true, 6, true), 0x83, vec![0x01, 0x02, 0x03]).unwrap();
    assert_eq!(su.encode().unwrap(), bytes);

    // Decode the same hex and assert every field.
    match SignalUnit::decode(&bytes).unwrap() {
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
fn all_status_indications_round_trip_and_carry_wire_code() {
    let cases = [
        (0u8, StatusIndication::OutOfAlignment),
        (1, StatusIndication::NormalAlignment),
        (2, StatusIndication::EmergencyAlignment),
        (3, StatusIndication::OutOfService),
        (4, StatusIndication::ProcessorOutage),
        (5, StatusIndication::Busy),
    ];
    for (code, status) in cases {
        let su = SignalUnit::lssu(hdr(1, false, 2, true), status);
        let wire = su.encode().unwrap();
        assert_eq!(wire[3], code, "status {status} must encode to code {code}");
        assert_eq!(SignalUnit::decode(&wire).unwrap(), su);
    }
}

// ── §7 Link state control: alignment / proving sequence ──────────────────────

fn fast_config() -> Mtp2Config {
    // Large guard timers; short proving so only T4 drives the sequence here.
    Mtp2Config {
        method: RetransmissionMethod::Basic,
        t1_aligned_ready: 100_000,
        t2_not_aligned: 100_000,
        t3_aligned: 100_000,
        t4_proving_normal: 4,
        t7_excessive_delay: 100_000,
        t6_remote_congestion: 100_000,
        ..Mtp2Config::default()
    }
}

fn peer_lssu(status: StatusIndication) -> SignalUnit {
    SignalUnit::lssu(hdr(127, true, 127, true), status)
}

fn transmitted_lssu(link: &mut Mtp2Link) -> StatusIndication {
    match link.poll_transmit() {
        Some(SignalUnit::Lssu { status, .. }) => status,
        other => panic!("expected an LSSU, got {other:?}"),
    }
}

#[test]
fn alignment_drives_sios_sio_sin_proving_to_in_service() {
    let mut link = Mtp2Link::new(fast_config());

    // Powered off: nothing on the wire.
    assert!(link.poll_transmit().is_none());
    assert_eq!(link.state(), Mtp2State::OutOfService);

    // Start → Not Aligned, transmitting SIO.
    link.start();
    assert_eq!(link.state(), Mtp2State::NotAligned);
    assert_eq!(
        transmitted_lssu(&mut link),
        StatusIndication::OutOfAlignment
    );

    // Peer SIO → Aligned, now transmitting SIN.
    link.handle_su(&peer_lssu(StatusIndication::OutOfAlignment));
    assert_eq!(link.state(), Mtp2State::Aligned);
    assert_eq!(
        transmitted_lssu(&mut link),
        StatusIndication::NormalAlignment
    );

    // Peer SIN → Proving (still transmitting SIN).
    link.handle_su(&peer_lssu(StatusIndication::NormalAlignment));
    assert_eq!(link.state(), Mtp2State::Proving);
    assert_eq!(
        transmitted_lssu(&mut link),
        StatusIndication::NormalAlignment
    );

    // Proving period (T4 = 4) elapses without errors → Aligned Ready, sending FISU.
    for _ in 0..4 {
        link.tick();
    }
    assert_eq!(link.state(), Mtp2State::AlignedReady);
    assert!(matches!(
        link.poll_transmit(),
        Some(SignalUnit::Fisu { .. })
    ));

    // Peer FISU (it finished proving) → In Service.
    link.handle_su(&SignalUnit::fisu(hdr(0, true, 0, true)));
    assert_eq!(link.state(), Mtp2State::InService);
    assert!(events(&mut link).contains(&Event::InService));
}

#[test]
fn emergency_alignment_uses_emergency_proving() {
    let cfg = Mtp2Config {
        emergency: true,
        t4_proving_emergency: 2,
        ..fast_config()
    };
    let mut link = Mtp2Link::new(cfg);
    link.start();
    // Emergency links send SIE, not SIN.
    link.handle_su(&peer_lssu(StatusIndication::OutOfAlignment));
    assert_eq!(
        transmitted_lssu(&mut link),
        StatusIndication::EmergencyAlignment
    );
    link.handle_su(&peer_lssu(StatusIndication::EmergencyAlignment));
    assert_eq!(link.state(), Mtp2State::Proving);
    for _ in 0..2 {
        link.tick();
    }
    assert_eq!(link.state(), Mtp2State::AlignedReady);
}

// ── §6 AERM / §10 SUERM error-rate monitors ──────────────────────────────────

#[test]
fn aerm_aborts_proving_and_fails_alignment() {
    let cfg = Mtp2Config {
        aerm_threshold_normal: 4,
        proving_attempts: 1, // no retries: first abort fails alignment
        ..fast_config()
    };
    let mut link = Mtp2Link::new(cfg);
    link.start();
    link.handle_su(&peer_lssu(StatusIndication::OutOfAlignment));
    link.handle_su(&peer_lssu(StatusIndication::NormalAlignment));
    assert_eq!(link.state(), Mtp2State::Proving);

    // Four corrupted SUs during proving reach the AERM threshold.
    for _ in 0..4 {
        link.handle_corrupted_su();
    }
    assert_eq!(link.state(), Mtp2State::OutOfService);
    assert!(events(&mut link)
        .iter()
        .any(|e| matches!(e, Event::AlignmentFailed(_))));
}

#[test]
fn suerm_fails_an_in_service_link() {
    let cfg = Mtp2Config {
        suerm_threshold: 5,
        suerm_decrement_interval: 100_000,
        ..fast_config()
    };
    let mut link = Mtp2Link::new(cfg);
    bring_in_service(&mut link);
    let _ = events(&mut link);

    for _ in 0..5 {
        link.handle_corrupted_su();
    }
    assert_eq!(link.state(), Mtp2State::OutOfService);
    assert!(events(&mut link).contains(&Event::OutOfService(OutOfServiceReason::SuermFailure)));
}

// ── §5 Retransmission: Basic (NACK) and PCR (cyclic) ─────────────────────────

#[test]
fn basic_method_recovers_a_lost_msu_over_a_link() {
    // Two Basic-method links back to back, but the transport drops one MSU.
    let (mut a, mut b) = in_service_pair(RetransmissionMethod::Basic);

    a.submit_msu(0x83, vec![0xA0]).unwrap();
    a.submit_msu(0x83, vec![0xA1]).unwrap();
    a.submit_msu(0x83, vec![0xA2]).unwrap();

    let mut delivered: Vec<u8> = Vec::new();
    // Pull three SUs from A but drop the SECOND on the way to B.
    for i in 0..3 {
        if let Some(su) = a.poll_transmit() {
            if i != 1 {
                b.handle_su(&su);
            }
        }
        collect_msu(&mut b, &mut delivered);
    }
    // B saw a gap and NACKed; run the loop so the NACK reaches A and A retransmits.
    for _ in 0..8 {
        step(&mut a, &mut b);
        collect_msu(&mut b, &mut delivered);
    }
    assert_eq!(delivered, vec![0xA0, 0xA1, 0xA2], "MSUs recovered in order");
}

#[test]
fn pcr_method_recovers_a_lost_msu_without_nack() {
    let (mut a, mut b) = in_service_pair(RetransmissionMethod::Pcr(Default::default()));

    a.submit_msu(0x83, vec![0xB0]).unwrap();
    a.submit_msu(0x83, vec![0xB1]).unwrap();

    let mut delivered: Vec<u8> = Vec::new();
    // Drop the first MSU; PCR cyclically retransmits so it still arrives.
    let mut dropped_one = false;
    for _ in 0..20 {
        if let Some(su) = a.poll_transmit() {
            let drop = !dropped_one && matches!(su, SignalUnit::Msu { .. });
            if drop {
                dropped_one = true;
            } else {
                b.handle_su(&su);
            }
        }
        if let Some(su) = b.poll_transmit() {
            a.handle_su(&su);
        }
        a.tick();
        b.tick();
        collect_msu(&mut b, &mut delivered);
        // PCR must never emit a retransmission request.
        assert!(!drain(&mut b)
            .iter()
            .any(|e| matches!(e, Event::RetransmissionRequested)));
    }
    assert_eq!(delivered, vec![0xB0, 0xB1]);
}

// ── In-memory link-to-link ───────────────────────────────────────────────────

#[test]
fn two_links_reach_in_service_and_exchange_an_msu() {
    let (mut a, mut b) = in_service_pair(RetransmissionMethod::Basic);
    assert_eq!(a.state(), Mtp2State::InService);
    assert_eq!(b.state(), Mtp2State::InService);

    // An MSU handed to A comes out of B intact.
    let sif: Vec<u8> = vec![0x83, 0x01, 0x00, 0x00, 0x09, 0x00, 0x03, 0x05];
    a.submit_msu(0x83, sif.clone()).unwrap();

    let mut got: Option<(u8, Vec<u8>)> = None;
    for _ in 0..12 {
        step(&mut a, &mut b);
        for e in drain(&mut b) {
            if let Event::Msu { sio, sif } = e {
                got = Some((sio, sif));
            }
        }
        if got.is_some() {
            break;
        }
    }
    assert_eq!(got, Some((0x83, sif)));
}

// ── Harness ──────────────────────────────────────────────────────────────────

fn events(link: &mut Mtp2Link) -> Vec<Event> {
    drain(link)
}

fn drain(link: &mut Mtp2Link) -> Vec<Event> {
    let mut out = Vec::new();
    while let Some(e) = link.poll_event() {
        out.push(e);
    }
    out
}

fn collect_msu(link: &mut Mtp2Link, out: &mut Vec<u8>) {
    for e in drain(link) {
        if let Event::Msu { sif, .. } = e {
            out.push(sif[0]);
        }
    }
}

/// One synchronous cycle: each side transmits one SU to the other, then both tick.
fn step(a: &mut Mtp2Link, b: &mut Mtp2Link) {
    let a_su = a.poll_transmit();
    let b_su = b.poll_transmit();
    if let Some(su) = a_su {
        b.handle_su(&su);
    }
    if let Some(su) = b_su {
        a.handle_su(&su);
    }
    a.tick();
    b.tick();
}

fn link(method: RetransmissionMethod) -> Mtp2Link {
    Mtp2Link::new(Mtp2Config {
        method,
        ..fast_config()
    })
}

/// Two links driven through alignment to In Service over an in-memory pipe.
fn in_service_pair(method: RetransmissionMethod) -> (Mtp2Link, Mtp2Link) {
    let mut a = link(method);
    let mut b = link(method);
    a.start();
    b.start();
    for _ in 0..64 {
        step(&mut a, &mut b);
        if a.state() == Mtp2State::InService && b.state() == Mtp2State::InService {
            break;
        }
    }
    assert_eq!(a.state(), Mtp2State::InService, "A reached In Service");
    assert_eq!(b.state(), Mtp2State::InService, "B reached In Service");
    (a, b)
}

fn bring_in_service(link: &mut Mtp2Link) {
    link.start();
    link.handle_su(&peer_lssu(StatusIndication::OutOfAlignment));
    link.handle_su(&peer_lssu(StatusIndication::NormalAlignment));
    for _ in 0..4 {
        link.tick();
    }
    assert_eq!(link.state(), Mtp2State::AlignedReady);
    link.handle_su(&SignalUnit::fisu(hdr(0, true, 0, true)));
    assert_eq!(link.state(), Mtp2State::InService);
}

//! Codec + state-machine micro-benchmarks: signal-unit encode/decode and a full
//! Q.703 alignment sequence.
//!
//! Run with `cargo bench`. All fixtures are built from the public API (or the
//! Q.703 wire layout), so the benches measure exactly the work this crate does -
//! header pack/unpack, body copy, and the state transition table - with no I/O.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use mtp2::{Mtp2Config, Mtp2Link, RetransmissionMethod, SignalUnit, StatusIndication, SuHeader};

/// A representative MTP3 SIF (synthetic: routing label + a short SCCP-ish body).
fn sample_sif() -> Vec<u8> {
    let mut sif = vec![0x01, 0x02, 0x03, 0x04];
    sif.extend_from_slice(&[0x09, 0x00, 0x03, 0x05, 0x0a, 0x0b, 0x0c, 0x0d]);
    sif.extend_from_slice(&[0xAB; 24]);
    sif
}

fn fast_config() -> Mtp2Config {
    Mtp2Config {
        method: RetransmissionMethod::Basic,
        t1_aligned_ready: 10_000,
        t2_not_aligned: 10_000,
        t3_aligned: 10_000,
        t4_proving_normal: 3,
        ..Mtp2Config::default()
    }
}

fn bench_codec(c: &mut Criterion) {
    let hdr = SuHeader::new(5, true, 6, true).expect("valid header");
    let fisu = SignalUnit::fisu(hdr);
    let lssu = SignalUnit::lssu(hdr, StatusIndication::NormalAlignment);
    let msu = SignalUnit::msu(hdr, 0x83, sample_sif()).expect("valid msu");

    let fisu_bytes = fisu.encode().expect("encode fisu");
    let lssu_bytes = lssu.encode().expect("encode lssu");
    let msu_bytes = msu.encode().expect("encode msu");

    let mut g = c.benchmark_group("codec");
    g.throughput(Throughput::Elements(1));

    g.bench_function("fisu/decode", |b| {
        b.iter(|| SignalUnit::decode(&fisu_bytes).unwrap())
    });
    g.bench_function("lssu/decode", |b| {
        b.iter(|| SignalUnit::decode(&lssu_bytes).unwrap())
    });
    g.bench_function("msu/decode", |b| {
        b.iter(|| SignalUnit::decode(&msu_bytes).unwrap())
    });
    g.bench_function("msu/encode", |b| {
        b.iter_batched(
            || msu.clone(),
            |m| m.encode().unwrap(),
            BatchSize::SmallInput,
        )
    });
    g.finish();

    // The link state machine: drive a fresh link through a full alignment.
    let mut sg = c.benchmark_group("state_machine");
    sg.throughput(Throughput::Elements(1));
    sg.bench_function("full_alignment", |b| {
        b.iter(|| {
            let mut link = Mtp2Link::new(fast_config());
            link.start();
            let peer = SuHeader::new(127, true, 127, true).unwrap();
            link.handle_su(&SignalUnit::lssu(peer, StatusIndication::OutOfAlignment));
            link.handle_su(&SignalUnit::lssu(peer, StatusIndication::NormalAlignment));
            for _ in 0..3 {
                link.tick();
            }
            link.handle_su(&SignalUnit::fisu(SuHeader::new(0, true, 0, true).unwrap()));
            link.state()
        })
    });
    sg.finish();
}

criterion_group!(benches, bench_codec);
criterion_main!(benches);

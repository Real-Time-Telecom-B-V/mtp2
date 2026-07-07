//! Memory-leak check.
//!
//! A counting global allocator tracks **live bytes** (allocated − freed) - RSS is
//! too noisy (the OS/allocator retains freed pages), but live bytes are exact, so
//! a real leak shows up as monotonic growth. Two phases:
//!
//!   1. **codec** - encode + decode a FISU, an LSSU and an MSU for many cycles
//!      (the header pack/unpack + body copy path).
//!   2. **state machine** - drive a fresh link through a full alignment to In
//!      Service and pass one MSU, over and over.
//!
//! Each phase asserts live bytes return to a flat baseline. Exits non-zero on a
//! leak. Driven by `scripts/mem_leak_test.sh`.
//!
//! Run: `cargo run --release --example leak_check`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicI64, Ordering};

use mtp2::{
    Mtp2Config, Mtp2Link, Mtp2State, RetransmissionMethod, SignalUnit, StatusIndication, SuHeader,
};

// ── Counting allocator ──────────────────────────────────────────────────────
static LIVE: AtomicI64 = AtomicI64::new(0);

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            LIVE.fetch_add(l.size() as i64, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        LIVE.fetch_sub(l.size() as i64, Ordering::Relaxed);
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(l);
        if !p.is_null() {
            LIVE.fetch_add(l.size() as i64, Ordering::Relaxed);
        }
        p
    }
    unsafe fn realloc(&self, ptr: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let p = System.realloc(ptr, l, new_size);
        if !p.is_null() {
            LIVE.fetch_add(new_size as i64 - l.size() as i64, Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> i64 {
    LIVE.load(Ordering::Relaxed)
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

// ── Phase 1: codec workload ─────────────────────────────────────────────────
fn codec_cycle(iters: usize) {
    let hdr = SuHeader::new(5, true, 6, true).unwrap();
    let fisu = SignalUnit::fisu(hdr);
    let lssu = SignalUnit::lssu(hdr, StatusIndication::NormalAlignment);
    let mut sif = vec![0x01, 0x02, 0x03, 0x04];
    sif.extend_from_slice(&[0xAB; 32]);
    let msu = SignalUnit::msu(hdr, 0x83, sif).unwrap();
    for _ in 0..iters {
        for su in [&fisu, &lssu, &msu] {
            let bytes = su.encode().unwrap();
            std::hint::black_box(SignalUnit::decode(&bytes).unwrap());
        }
    }
}

// ── Phase 2: state-machine churn ────────────────────────────────────────────
fn state_machine_cycle(iters: usize) {
    let peer = SuHeader::new(127, true, 127, true).unwrap();
    for _ in 0..iters {
        let mut link = Mtp2Link::new(fast_config());
        link.start();
        link.handle_su(&SignalUnit::lssu(peer, StatusIndication::OutOfAlignment));
        link.handle_su(&SignalUnit::lssu(peer, StatusIndication::NormalAlignment));
        for _ in 0..3 {
            link.tick();
        }
        link.handle_su(&SignalUnit::fisu(SuHeader::new(0, true, 0, true).unwrap()));
        debug_assert_eq!(link.state(), Mtp2State::InService);
        link.submit_msu(0x83, vec![0x01, 0x02, 0x03]).unwrap();
        let _ = std::hint::black_box(link.poll_transmit());
        while link.poll_event().is_some() {}
    }
}

fn report(phase: &str, base: i64) -> i64 {
    let growth = live() - base;
    println!("  {phase}: live = {} bytes (delta {:+})", live(), growth);
    growth
}

fn main() {
    const ITERS: usize = 200_000;
    const CYCLES: usize = 10;
    const BUDGET: i64 = 64 * 1024;

    // Phase 1: codec.
    println!("[codec] {CYCLES} x {ITERS} encode+decode round-trips (fisu + lssu + msu)");
    codec_cycle(ITERS); // warm up
    let codec_base = live();
    for c in 1..=CYCLES {
        codec_cycle(ITERS);
        report(&format!("cycle {c:>2}/{CYCLES}"), codec_base);
    }
    let codec_growth = live() - codec_base;

    // Phase 2: state machine.
    println!("\n[state machine] {CYCLES} x {ITERS} full alignment + MSU pass");
    state_machine_cycle(ITERS); // warm up
    let sm_base = live();
    for c in 1..=CYCLES {
        state_machine_cycle(ITERS);
        report(&format!("cycle {c:>2}/{CYCLES}"), sm_base);
    }
    let sm_growth = live() - sm_base;

    // Verdict.
    println!();
    let mut ok = true;
    if codec_growth > BUDGET {
        eprintln!("FAIL: codec live bytes grew {codec_growth} (> {BUDGET})");
        ok = false;
    }
    if sm_growth > BUDGET {
        eprintln!("FAIL: state-machine live bytes grew {sm_growth} (> {BUDGET})");
        ok = false;
    }
    if !ok {
        std::process::exit(1);
    }
    println!(
        "PASS: codec delta {codec_growth} <= {BUDGET}; state-machine delta {sm_growth} <= {BUDGET}"
    );
}

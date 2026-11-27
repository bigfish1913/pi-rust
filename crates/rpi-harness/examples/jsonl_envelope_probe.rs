//! Read-only allocation/latency probe for an immutable transcript copy.
//! Run: cargo run -p rpi-harness --example jsonl_envelope_probe --release --locked -- COPY.jsonl
//! Not a storage/replay/startup benchmark. No lease, repair, or payload logging.
use serde_json::Value;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    time::Instant,
};

// Use the actual private helper, not a benchmark-only copy of the optimization.
#[path = "../src/session/jsonl/envelope.rs"]
mod envelope;

struct Audit;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static REQUESTED: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn allocated(size: usize) {
    CALLS.fetch_add(1, Relaxed);
    REQUESTED.fetch_add(size, Relaxed);
    let live = LIVE.fetch_add(size, Relaxed) + size;
    PEAK.fetch_max(live, Relaxed);
}

// Forward the allocator contract unchanged. These non-allocating counters are
// enabled only around complete parse-and-drop loops on this single-thread probe.
unsafe impl GlobalAlloc for Audit {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() && ACTIVE.load(Relaxed) {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() && ACTIVE.load(Relaxed) {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, size) };
        if !new.is_null() && ACTIVE.load(Relaxed) {
            LIVE.fetch_sub(layout.size(), Relaxed);
            allocated(size);
        }
        new
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ACTIVE.load(Relaxed) {
            LIVE.fetch_sub(layout.size(), Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Audit = Audit;

fn decode(line: &str, reserved: bool) -> Value {
    if reserved {
        if let Ok(map) = envelope::parse(line) {
            return Value::Object(map);
        }
    }
    serde_json::from_str(line).expect("invalid JSON in frozen fixture")
}

fn main() {
    let path = std::env::args_os()
        .nth(1)
        .expect("expected frozen COPY.jsonl");
    let transcript = std::fs::read_to_string(path).expect("read frozen fixture");
    // Load input before measuring. Include all physical non-empty mutation rows,
    // without cold-frame filtering or typed-schema validation.
    let lines: Vec<_> = transcript
        .lines()
        .skip(1)
        .filter(|line| !line.is_empty())
        .collect();
    assert!(!lines.is_empty(), "fixture has no mutation rows");
    for (index, line) in lines.iter().enumerate() {
        let expected = decode(line, false);
        let actual = decode(line, true);
        assert!(expected == actual, "value mismatch at mutation row {index}");
        assert!(
            serde_json::to_string(&expected).unwrap() == serde_json::to_string(&actual).unwrap(),
            "order/number mismatch at mutation row {index}"
        );
    }
    for round in 0..7 {
        for reserved in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let start = Instant::now();
            for line in &lines {
                drop(black_box(decode(line, reserved)));
            }
            println!(
                "round={round} reserved={reserved} rows={} elapsed_ms={:.3}",
                lines.len(),
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
    for reserved in [false, true] {
        for counter in [&CALLS, &REQUESTED, &LIVE, &PEAK] {
            counter.store(0, Relaxed);
        }
        ACTIVE.store(true, Relaxed);
        for line in &lines {
            drop(black_box(decode(line, reserved)));
        }
        ACTIVE.store(false, Relaxed);
        assert_eq!(LIVE.load(Relaxed), 0, "probe retained counted allocations");
        // Reallocation counts the new requested size cumulatively. Peak is max
        // outstanding layout bytes, not RSS or malloc's internal transient peak.
        println!("reserved={reserved} rows={} allocations={} requested_bytes={} peak_live_layout_bytes={}", lines.len(), CALLS.load(Relaxed), REQUESTED.load(Relaxed), PEAK.load(Relaxed));
    }
}

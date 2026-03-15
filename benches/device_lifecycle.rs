#![cfg(all(target_os = "linux", feature = "linux-target"))]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tcmu::target::TcmuTarget;

static COUNTER: AtomicU64 = AtomicU64::new(0);
const DEVICE_SIZE: u64 = 1 << 20;

fn next_name() -> String {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("lifecycle-{}-{id}", std::process::id())
}

fn main() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("must run as root");
        std::process::exit(1);
    }

    println!();

    // No loopback: configfs create + UIO wait + reset_ring + configfs remove
    let n = 10;
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let name = next_name();
        let start = Instant::now();
        let target = TcmuTarget::builder()
            .name(&name)
            .size_bytes(DEVICE_SIZE)
            .cmd_time_out(Duration::from_secs(5))
            .build()
            .expect("build failed");
        drop(target);
        samples.push(start.elapsed());
    }

    let mut sorted: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
    let min = sorted[0];
    let max = sorted[sorted.len() - 1];
    let p50 = sorted[sorted.len() / 2];

    println!("create_destroy/no_loopback  n={n}  mean={mean:.1}ms  p50={p50:.1}ms  min={min:.1}ms  max={max:.1}ms");

    // NOTE: Loopback lifecycle benchmarks are not included here because
    // killing the benchmark mid-run (e.g. Ctrl-C) leaves the kernel in
    // an irrecoverable state (D-state kworkers stuck in SCSI probing).
    // Loopback lifecycle testing should be done in a disposable VM.
    // See docs/device-lifecycle.md for recovery procedures.

    println!();
}

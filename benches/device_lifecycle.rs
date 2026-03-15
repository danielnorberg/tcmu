#![cfg(all(target_os = "linux", feature = "linux-target"))]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, SamplingMode, criterion_group, criterion_main};
use tcmu::target::TcmuTarget;
use tcmu::{BlockDevice, TcmuDevice, TcmuDeviceConfig};

static COUNTER: AtomicU64 = AtomicU64::new(0);

const DEVICE_SIZE: u64 = 1 << 20; // 1 MiB — minimal valid size

fn next_name() -> String {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("lifecycle-{}-{id}", std::process::id())
}

fn device_config() -> TcmuDeviceConfig {
    TcmuDeviceConfig {
        vendor_id: *b"BENCH   ",
        product_id: *b"LIFECYCLE BENCH ",
        product_revision: *b"0001",
        device_id_prefix: "lifecycle-bench".to_string(),
    }
}

// ── Minimal BlockDevice ──────────────────────────────────────────────────────

struct NullDevice;

impl BlockDevice for NullDevice {
    fn size_bytes(&self) -> u64 {
        DEVICE_SIZE
    }

    fn read_at(&self, _offset: u64, len: usize) -> anyhow::Result<impl AsRef<[u8]>> {
        Ok(vec![0u8; len])
    }

    fn id_bytes(&self) -> Vec<u8> {
        vec![0; 8]
    }
}

// ── tcm_loop device discovery ────────────────────────────────────────────────

fn tcm_loop_block_devices() -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir("/sys/class/block") else {
        return vec![];
    };
    rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("sd"))
        .filter(|e| {
            fs::canonicalize(e.path().join("device"))
                .map(|p| p.to_string_lossy().contains("tcm_loop"))
                .unwrap_or(false)
        })
        .map(|e| PathBuf::from("/dev").join(e.file_name()))
        .collect()
}

fn wait_for_new_tcm_loop_device(
    before: &[PathBuf],
    timeout: Duration,
) -> anyhow::Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        for dev in tcm_loop_block_devices() {
            if !before.contains(&dev) {
                return Ok(dev);
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "no new tcm_loop device appeared within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ── Lifecycle: create one device with loopback, wait for block device ────────

fn create_one_loopback(
    name: &str,
    before: &[PathBuf],
) -> anyhow::Result<(Arc<TcmuTarget>, std::thread::JoinHandle<anyhow::Result<()>>, PathBuf)> {
    let target = Arc::new(
        TcmuTarget::builder()
            .name(name)
            .size_bytes(DEVICE_SIZE)
            .cmd_time_out(Duration::from_secs(5))
            .with_loopback()
            .build()?,
    );
    let device = Arc::new(TcmuDevice::new(NullDevice, device_config()));
    let target_t = Arc::clone(&target);
    let device_t = Arc::clone(&device);
    let handle = std::thread::spawn(move || target_t.run(&*device_t));

    let dev = wait_for_new_tcm_loop_device(before, Duration::from_secs(10))?;

    Ok((target, handle, dev))
}

fn teardown(target: Arc<TcmuTarget>, handle: std::thread::JoinHandle<anyhow::Result<()>>) {
    target.stop();
    let _ = handle.join();
}

/// Wait until a block device path no longer exists in sysfs.
fn wait_for_device_removal(dev: &std::path::Path, timeout: Duration) {
    let dev_name = dev.file_name().unwrap().to_string_lossy().to_string();
    let sysfs = format!("/sys/block/{dev_name}");
    let deadline = Instant::now() + timeout;
    while std::path::Path::new(&sysfs).exists() {
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ── Benchmarks ───────────────────────────────────────────────────────────────

fn bench_create_destroy(c: &mut Criterion) {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipping device_lifecycle benchmark: must run as root");
        return;
    }

    let mut no_lb = c.benchmark_group("create_destroy");
    no_lb.sample_size(20);
    no_lb.sampling_mode(SamplingMode::Flat);
    no_lb.bench_function("no_loopback", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let name = next_name();
                let start = Instant::now();
                let target = TcmuTarget::builder()
                    .name(&name)
                    .size_bytes(DEVICE_SIZE)
                    .build()
                    .expect("build failed");
                drop(target);
                elapsed += start.elapsed();
            }
            elapsed
        });
    });
    no_lb.finish();

    // With loopback — full lifecycle: build + event loop + block device appearance + teardown.
    // Each iteration waits for the previous device to fully disappear before starting,
    // ensuring we measure one clean device lifecycle at a time.
    let mut with_lb = c.benchmark_group("create_destroy_loopback");
    with_lb.sample_size(10);
    with_lb.sampling_mode(SamplingMode::Flat);
    with_lb.bench_function("sequential", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let name = next_name();
                let before = tcm_loop_block_devices();
                let start = Instant::now();
                let (target, handle, dev) = create_one_loopback(&name, &before)
                    .expect("create failed");
                teardown(target, handle);
                elapsed += start.elapsed();
                // Wait for the block device to vanish so the next iteration
                // starts with a clean slate.
                wait_for_device_removal(&dev, Duration::from_secs(5));
            }
            elapsed
        });
    });
    with_lb.finish();
}

fn bench_concurrent_create(c: &mut Criterion) {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }

    let mut group = c.benchmark_group("concurrent_create");
    group.sample_size(10);
    group.sampling_mode(SamplingMode::Flat);

    // Measure wall-clock time for N devices to all become ready, created in
    // parallel. This is the metric that matters for burst creation.
    for n in [4, 8, 16] {
        group.bench_function(BenchmarkId::new("loopback", n), |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    let before = tcm_loop_block_devices();

                    let start = Instant::now();

                    // Create all N devices concurrently
                    let handles: Vec<_> = (0..n)
                        .map(|_| {
                            let name = next_name();
                            let before = before.clone();
                            std::thread::spawn(move || create_one_loopback(&name, &before))
                        })
                        .collect();

                    let mut targets = Vec::with_capacity(n);
                    for h in handles {
                        let (target, thread, dev) = h.join().unwrap().expect("create failed");
                        targets.push((target, thread, dev));
                    }

                    elapsed += start.elapsed();

                    // Teardown (not timed)
                    let devs: Vec<_> = targets.iter().map(|(_, _, d)| d.clone()).collect();
                    for (target, handle, _) in targets {
                        teardown(target, handle);
                    }
                    for dev in &devs {
                        wait_for_device_removal(dev, Duration::from_secs(5));
                    }
                }
                elapsed
            });
        });
    }

    group.finish();
}

fn bench_burst_sequential(c: &mut Criterion) {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }

    let mut group = c.benchmark_group("burst_sequential");
    group.sample_size(10);
    group.sampling_mode(SamplingMode::Flat);

    for burst_size in [5, 10] {
        group.bench_function(BenchmarkId::new("loopback", burst_size), |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    for _ in 0..burst_size {
                        let name = next_name();
                        let before = tcm_loop_block_devices();
                        let (target, handle, dev) = create_one_loopback(&name, &before)
                            .expect("create failed");
                        teardown(target, handle);
                        wait_for_device_removal(&dev, Duration::from_secs(5));
                    }
                    elapsed += start.elapsed();
                }
                elapsed
            });
        });
    }

    group.finish();
}

criterion_group! {
    name = lifecycle;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1));
    targets = bench_create_destroy, bench_concurrent_create, bench_burst_sequential
}
criterion_main!(lifecycle);

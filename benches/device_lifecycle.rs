#![cfg(all(target_os = "linux", feature = "linux-target"))]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tcmu::target::TcmuTarget;
use tcmu::{BlockDevice, TcmuDevice, TcmuDeviceConfig};

static COUNTER: AtomicU64 = AtomicU64::new(0);
const DEVICE_SIZE: u64 = 1 << 20;

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

fn tcm_loop_block_devices() -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir("/sys/class/block") else { return vec![] };
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

fn wait_for_new_tcm_loop_device(before: &[PathBuf], timeout: Duration) -> anyhow::Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        for dev in tcm_loop_block_devices() {
            if !before.contains(&dev) {
                return Ok(dev);
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "no new tcm_loop device within {timeout:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_device_removal(dev: &std::path::Path, timeout: Duration) {
    let name = dev.file_name().unwrap().to_string_lossy().to_string();
    let sysfs = format!("/sys/block/{name}");
    let deadline = Instant::now() + timeout;
    while std::path::Path::new(&sysfs).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

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

fn main() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("must run as root");
        std::process::exit(1);
    }

    // 1. No loopback
    {
        let name = next_name();
        let start = Instant::now();
        let target = TcmuTarget::builder()
            .name(&name)
            .size_bytes(DEVICE_SIZE)
            .build()
            .expect("build failed");
        drop(target);
        println!("create_destroy/no_loopback          {:>8.1}ms", start.elapsed().as_secs_f64() * 1000.0);
    }

    // 2. With loopback (sequential)
    {
        let name = next_name();
        let before = tcm_loop_block_devices();
        let start = Instant::now();
        let (target, handle, dev) = create_one_loopback(&name, &before).expect("create failed");
        let created = start.elapsed();
        teardown(target, handle);
        let total = start.elapsed();
        println!("create_destroy/loopback             {:>8.1}ms  (create {:>8.1}ms  teardown {:>8.1}ms)",
            total.as_secs_f64() * 1000.0,
            created.as_secs_f64() * 1000.0,
            (total - created).as_secs_f64() * 1000.0);
        wait_for_device_removal(&dev, Duration::from_secs(5));
    }

    // 3. Concurrent (4 devices)
    {
        let before = tcm_loop_block_devices();
        let start = Instant::now();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let name = next_name();
                let before = before.clone();
                std::thread::spawn(move || create_one_loopback(&name, &before))
            })
            .collect();
        let mut targets = Vec::new();
        for h in handles {
            targets.push(h.join().unwrap().expect("create failed"));
        }
        let wall = start.elapsed();
        let devs: Vec<_> = targets.iter().map(|(_, _, d)| d.clone()).collect();
        for (target, handle, _) in targets {
            teardown(target, handle);
        }
        for dev in &devs {
            wait_for_device_removal(dev, Duration::from_secs(5));
        }
        println!("concurrent_create/4                 {:>8.1}ms  ({:.1}ms/device)",
            wall.as_secs_f64() * 1000.0, wall.as_secs_f64() * 1000.0 / 4.0);
    }

    // 4. Concurrent (8 devices)
    {
        let before = tcm_loop_block_devices();
        let start = Instant::now();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let name = next_name();
                let before = before.clone();
                std::thread::spawn(move || create_one_loopback(&name, &before))
            })
            .collect();
        let mut targets = Vec::new();
        for h in handles {
            targets.push(h.join().unwrap().expect("create failed"));
        }
        let wall = start.elapsed();
        let devs: Vec<_> = targets.iter().map(|(_, _, d)| d.clone()).collect();
        for (target, handle, _) in targets {
            teardown(target, handle);
        }
        for dev in &devs {
            wait_for_device_removal(dev, Duration::from_secs(5));
        }
        println!("concurrent_create/8                 {:>8.1}ms  ({:.1}ms/device)",
            wall.as_secs_f64() * 1000.0, wall.as_secs_f64() * 1000.0 / 8.0);
    }
}

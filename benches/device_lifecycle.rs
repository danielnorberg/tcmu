#![cfg(all(target_os = "linux", feature = "linux-target"))]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

fn wait_for_new_tcm_loop_device(
    before: &[PathBuf],
    timeout: Duration,
    stop: &AtomicBool,
) -> anyhow::Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        for dev in tcm_loop_block_devices() {
            if !before.contains(&dev) {
                return Ok(dev);
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "no new tcm_loop device within {timeout:?}");
        anyhow::ensure!(!stop.load(Ordering::Relaxed), "interrupted");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_device_removal(dev: &std::path::Path, timeout: Duration, stop: &AtomicBool) {
    let name = dev.file_name().unwrap().to_string_lossy().to_string();
    let sysfs = format!("/sys/block/{name}");
    let deadline = Instant::now() + timeout;
    while std::path::Path::new(&sysfs).exists()
        && Instant::now() < deadline
        && !stop.load(Ordering::Relaxed)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn create_one_loopback(
    name: &str,
    before: &[PathBuf],
    stop: &AtomicBool,
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
    let dev = wait_for_new_tcm_loop_device(before, Duration::from_secs(10), stop)?;
    Ok((target, handle, dev))
}

fn teardown(target: Arc<TcmuTarget>, handle: std::thread::JoinHandle<anyhow::Result<()>>) {
    target.shutdown();
    let _ = handle.join();
}

fn print_stats(name: &str, samples: &[Duration]) {
    if samples.is_empty() {
        return;
    }
    let mut sorted: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sorted.len();
    let mean = sorted.iter().sum::<f64>() / n as f64;
    let min = sorted[0];
    let max = sorted[n - 1];
    let p50 = sorted[n / 2];
    eprintln!("{name:<40} n={n:<3} mean={mean:>8.1}ms  p50={p50:>8.1}ms  min={min:>8.1}ms  max={max:>8.1}ms");
}

/// Child process: create one loopback device, print creation time, then
/// wait for SIGTERM before tearing down.
fn run_child(name: &str) {
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))
        .expect("register SIGTERM");

    let before = tcm_loop_block_devices();
    let start = Instant::now();
    let (target, handle, _dev) = create_one_loopback(name, &before, &stop)
        .expect("child create failed");
    let created = start.elapsed();

    // Print creation time to stdout so the parent can parse it.
    println!("{}", created.as_nanos());

    // Wait for SIGTERM from parent.
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }

    teardown(target, handle);
}

fn send_sigterm(child: &std::process::Child) {
    // Safety: sending a signal to a known child process we spawned.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
}

fn main() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("must run as root");
        std::process::exit(1);
    }

    let args: Vec<String> = std::env::args().collect();

    // Child process mode: create one device, print timing to stdout, wait for SIGTERM.
    if args.get(1).is_some_and(|a| a == "--child") {
        let name = args.get(2).expect("--child requires a device name");
        run_child(name);
        return;
    }

    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))
        .expect("register SIGINT");
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))
        .expect("register SIGTERM");

    let self_exe = std::env::current_exe().expect("current_exe");

    // Parse --steady-state N: create N background devices before benchmarking.
    let steady_state_n: usize = args.windows(2)
        .find(|w| w[0] == "--steady-state")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(0);

    // Create steady-state background devices using child processes.
    let mut bg_children: Vec<std::process::Child> = Vec::new();
    if steady_state_n > 0 {
        eprintln!("creating {steady_state_n} steady-state devices...");
        let batch = 64;
        let mut created = 0;
        while created < steady_state_n && !stop.load(Ordering::Relaxed) {
            let n = batch.min(steady_state_n - created);
            let children: Vec<_> = (0..n)
                .map(|_| {
                    let name = next_name();
                    std::process::Command::new(&self_exe)
                        .args(["--child", &name])
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .expect("spawn child")
                })
                .collect();
            // Wait for each child to report it's ready (one line on stdout).
            for mut child in children {
                let stdout = child.stdout.as_mut().unwrap();
                let mut line = String::new();
                use std::io::BufRead;
                let _ = std::io::BufReader::new(stdout).read_line(&mut line);
                bg_children.push(child);
            }
            created += n;
            eprintln!("  {created}/{steady_state_n} devices ready");
        }
        eprintln!("steady state: {} devices running", bg_children.len());
        eprintln!();
    }

    eprintln!();

    // 1. No loopback
    {
        let mut samples = Vec::new();
        for _ in 0..10 {
            if stop.load(Ordering::Relaxed) { break; }
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
        print_stats("create_destroy/no_loopback", &samples);
    }

    // 2. Loopback sequential
    if !stop.load(Ordering::Relaxed) {
        let mut samples = Vec::new();
        for i in 0..3 {
            if stop.load(Ordering::Relaxed) { break; }
            let name = next_name();
            let before = tcm_loop_block_devices();
            let start = Instant::now();
            match create_one_loopback(&name, &before, &stop) {
                Ok((target, handle, dev)) => {
                    let created = start.elapsed();
                    teardown(target, handle);
                    let total = start.elapsed();
                    eprintln!("  loopback [{}/3] create={:.0}ms teardown={:.0}ms",
                        i + 1,
                        created.as_secs_f64() * 1000.0,
                        (total - created).as_secs_f64() * 1000.0);
                    samples.push(total);
                    wait_for_device_removal(&dev, Duration::from_secs(5), &stop);
                }
                Err(e) => {
                    eprintln!("  loopback [{}/3] error: {e:#}", i + 1);
                    break;
                }
            }
        }
        print_stats("create_destroy/loopback", &samples);
    }

    // 3. Concurrent
    for n in [4, 8, 16, 32, 64] {
        if stop.load(Ordering::Relaxed) { break; }
        let before = tcm_loop_block_devices();
        let start = Instant::now();
        let handles: Vec<_> = (0..n)
            .map(|_| {
                let name = next_name();
                let before = before.clone();
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || create_one_loopback(&name, &before, &stop))
            })
            .collect();

        let mut targets = Vec::new();
        let mut ok = true;
        for h in handles {
            match h.join().unwrap() {
                Ok(t) => targets.push(t),
                Err(e) => { eprintln!("  concurrent/{n} error: {e:#}"); ok = false; break; }
            }
        }
        let wall = start.elapsed();

        let devs: Vec<_> = targets.iter().map(|(_, _, d)| d.clone()).collect();
        for (target, handle, _) in targets {
            teardown(target, handle);
        }
        for dev in &devs {
            wait_for_device_removal(dev, Duration::from_secs(5), &stop);
        }

        if ok {
            eprintln!("concurrent_create/{n:<23}          wall={:>8.1}ms  per_device={:.1}ms",
                wall.as_secs_f64() * 1000.0,
                wall.as_secs_f64() * 1000.0 / n as f64);
        }
    }

    // 4. Multi-process concurrent creation (simulates separate processes)
    for n in [4, 8, 16, 32] {
        if stop.load(Ordering::Relaxed) { break; }
        let start = Instant::now();

        // Spawn N child processes, each creating one loopback device.
        let mut children: Vec<std::process::Child> = (0..n)
            .map(|_| {
                let name = next_name();
                std::process::Command::new(&self_exe)
                    .args(["--child", &name])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn child")
            })
            .collect();

        // Wait for each child to print its creation time (one line of nanos).
        let mut creation_times = Vec::new();
        for child in &mut children {
            let stdout = child.stdout.as_mut().unwrap();
            let mut line = String::new();
            use std::io::BufRead;
            std::io::BufReader::new(stdout).read_line(&mut line).ok();
            if let Ok(nanos) = line.trim().parse::<u64>() {
                creation_times.push(Duration::from_nanos(nanos));
            }
        }

        let wall = start.elapsed();

        // Teardown: SIGTERM all children and wait
        for child in &mut children {
            send_sigterm(child);
        }
        for child in &mut children {
            let _ = child.wait();
        }

        if creation_times.len() == n {
            let max_create = creation_times.iter().max().unwrap();
            eprintln!("multiprocess_create/{n:<22}          wall={:>8.1}ms  per_device={:.1}ms  slowest_create={:.1}ms",
                wall.as_secs_f64() * 1000.0,
                wall.as_secs_f64() * 1000.0 / n as f64,
                max_create.as_secs_f64() * 1000.0);
        } else {
            eprintln!("multiprocess_create/{n}: only {}/{n} children reported", creation_times.len());
        }
    }

    // 5. Sustained sequential throughput (30 seconds)
    if !stop.load(Ordering::Relaxed) {
        eprintln!("sustained_sequential: running for 30s...");
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut count = 0u64;
        let start = Instant::now();
        while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
            let name = next_name();
            let before = tcm_loop_block_devices();
            match create_one_loopback(&name, &before, &stop) {
                Ok((target, handle, dev)) => {
                    teardown(target, handle);
                    wait_for_device_removal(&dev, Duration::from_secs(5), &stop);
                    count += 1;
                }
                Err(e) => {
                    eprintln!("  sustained error after {count}: {e:#}");
                    break;
                }
            }
        }
        let elapsed = start.elapsed();
        let rate = count as f64 / elapsed.as_secs_f64();
        eprintln!("sustained_sequential                     count={count}  elapsed={:.1}s  rate={rate:.1}/s",
            elapsed.as_secs_f64());
    }

    // Teardown steady-state background devices.
    if !bg_children.is_empty() {
        eprintln!("tearing down {} steady-state devices...", bg_children.len());
        for child in &mut bg_children {
            send_sigterm(child);
        }
        for child in &mut bg_children {
            let _ = child.wait();
        }
        eprintln!("steady-state teardown complete");
    }

    eprintln!();
}

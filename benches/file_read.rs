#![cfg(all(target_os = "linux", feature = "linux-target"))]

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use tcmu::target::TcmuTarget;
use tcmu::{BlockDevice, TcmuDevice, TcmuDeviceConfig};

const IMAGE_SIZE_BYTES: u64 = 5 << 30;
const SMALL_FILE_COUNT: usize = 1024;
const SMALL_FILE_SIZE: usize = 4096;
const LARGE_FILE_SIZE: usize = 4 << 30;

fn main_bench(c: &mut Criterion) {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipping file_read benchmark: must run as root");
        return;
    }

    match BenchEnv::new() {
        Ok(env) => run_benchmarks(c, &env),
        Err(err) => eprintln!("skipping file_read benchmark: {err:#}"),
    }
}

fn run_benchmarks(c: &mut Criterion, env: &BenchEnv) {
    let mut small_files = c.benchmark_group("small_files");
    small_files.measurement_time(Duration::from_secs(10));
    small_files.throughput(Throughput::Bytes(env.small_files_total_bytes()));
    small_files.bench_function(BenchmarkId::new("tcmu", "single_pass"), |b| {
        bench_single_pass(b, env, Transport::Tcmu, read_small_files)
    });
    small_files.bench_function(BenchmarkId::new("loop", "single_pass"), |b| {
        bench_single_pass(b, env, Transport::Loop, read_small_files)
    });
    small_files.finish();

    let mut large_file = c.benchmark_group("large_file");
    large_file.measurement_time(Duration::from_secs(10));
    large_file.sampling_mode(SamplingMode::Flat);
    large_file.throughput(Throughput::Bytes(LARGE_FILE_SIZE as u64));

    large_file.bench_function(BenchmarkId::new("loop", "single_pass"), |b| {
        bench_single_pass(b, env, Transport::Loop, read_large_file)
    });

    for ra_kb in [128, 2048, 8192, 16384] {
        tcmu::target::set_read_ahead_kb(&env.tcmu_block_dev, ra_kb).unwrap();
        large_file.bench_function(
            BenchmarkId::new(format!("tcmu/ra_{ra_kb}k"), "single_pass"),
            |b| bench_single_pass(b, env, Transport::Tcmu, read_large_file),
        );
    }

    large_file.finish();
}

fn bench_single_pass<F>(
    b: &mut criterion::Bencher<'_>,
    env: &BenchEnv,
    transport: Transport,
    workload: F,
) where
    F: Fn(&Path) -> anyhow::Result<u64> + Copy,
{
    b.iter_custom(|iters| {
        let mut elapsed = Duration::ZERO;
        for _ in 0..iters {
            elapsed += time_single_pass(env, transport, workload).unwrap();
        }
        elapsed
    });
}

fn time_single_pass<F>(env: &BenchEnv, transport: Transport, workload: F) -> anyhow::Result<Duration>
where
    F: Fn(&Path) -> anyhow::Result<u64>,
{
    env.mount(transport)?;
    let mountpoint = env.mountpoint(transport);
    let started = Instant::now();
    let result = workload(mountpoint);
    let elapsed = started.elapsed();
    let unmount_result = env.unmount(transport);
    criterion::black_box(result?);
    unmount_result?;
    Ok(elapsed)
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1));
    targets = main_bench
}
criterion_main!(benches);

struct BenchEnv {
    tempdir: tempfile::TempDir,
    image_path: PathBuf,
    tcmu_block_dev: PathBuf,
    loop_block_dev: PathBuf,
    tcmu_mount: PathBuf,
    loop_mount: PathBuf,
    target: Arc<TcmuTarget>,
    loop_thread: Option<JoinHandle<anyhow::Result<()>>>,
}

#[derive(Clone, Copy)]
enum Transport {
    Tcmu,
    Loop,
}

impl BenchEnv {
    fn new() -> anyhow::Result<Self> {
        let tempdir = tempfile::tempdir()?;
        let image_path = tempdir.path().join("bench.img");
        create_ext4_image(&image_path)?;

        let populate_mount = tempdir.path().join("mnt_populate");
        fs::create_dir_all(&populate_mount)?;
        cmd(
            "mount",
            &[
                "-t",
                "ext4",
                "-o",
                "loop",
                image_path.to_str().unwrap(),
                populate_mount.to_str().unwrap(),
            ],
        )?;
        populate_fixture(&populate_mount)?;
        cmd("umount", &[populate_mount.to_str().unwrap()])?;

        let file_device = MmapDevice::open(&image_path)?;
        let size = file_device.size_bytes();
        let target_name = format!("file-read-bench-{}", std::process::id());
        let before_devices = tcm_loop_block_devices();

        let target = Arc::new(
            TcmuTarget::builder()
                .name(&target_name)
                .size_bytes(size)
                .with_loopback()
                .build()?,
        );

        let device = Arc::new(TcmuDevice::new(
            file_device,
            TcmuDeviceConfig {
                vendor_id: *b"BENCH   ",
                product_id: *b"FILE READ BENCH ",
                product_revision: *b"0001",
                device_id_prefix: "file-read-bench".to_string(),
            },
        ));

        let target_t = Arc::clone(&target);
        let device_t = Arc::clone(&device);
        let loop_thread = std::thread::spawn(move || target_t.run(&*device_t));

        let tcmu_block_dev = wait_for_new_tcm_loop_device(&before_devices, Duration::from_secs(5))?;

        // Tune the block device queue for throughput.
        tcmu::target::set_scheduler(&tcmu_block_dev, "none")?;
        tcmu::target::set_max_sectors_kb(&tcmu_block_dev, 16384)?;

        let loop_block_dev = attach_loop_device(&image_path)?;

        let tcmu_mount = tempdir.path().join("mnt_tcmu");
        let loop_mount = tempdir.path().join("mnt_loop");
        fs::create_dir_all(&tcmu_mount)?;
        fs::create_dir_all(&loop_mount)?;

        Ok(Self {
            tempdir,
            image_path,
            tcmu_block_dev,
            loop_block_dev,
            tcmu_mount,
            loop_mount,
            target,
            loop_thread: Some(loop_thread),
        })
    }

    fn small_files_total_bytes(&self) -> u64 {
        SMALL_FILE_COUNT as u64 * SMALL_FILE_SIZE as u64
    }

    fn device(&self, transport: Transport) -> &Path {
        match transport {
            Transport::Tcmu => &self.tcmu_block_dev,
            Transport::Loop => &self.loop_block_dev,
        }
    }

    fn mountpoint(&self, transport: Transport) -> &Path {
        match transport {
            Transport::Tcmu => &self.tcmu_mount,
            Transport::Loop => &self.loop_mount,
        }
    }

    fn mount(&self, transport: Transport) -> anyhow::Result<()> {
        cmd(
            "mount",
            &[
                "-t",
                "ext4",
                "-o",
                "ro",
                self.device(transport).to_str().unwrap(),
                self.mountpoint(transport).to_str().unwrap(),
            ],
        )
    }

    fn unmount(&self, transport: Transport) -> anyhow::Result<()> {
        cmd("umount", &[self.mountpoint(transport).to_str().unwrap()])
    }
}

impl Drop for BenchEnv {
    fn drop(&mut self) {
        if is_mounted(&self.tcmu_mount) {
            let _ = self.unmount(Transport::Tcmu);
        }
        if is_mounted(&self.loop_mount) {
            let _ = self.unmount(Transport::Loop);
        }
        let _ = detach_loop_device(&self.loop_block_dev);
        self.target.stop();
        if let Some(handle) = self.loop_thread.take() {
            let _ = handle.join();
        }
        let _ = (&self.tempdir, &self.image_path);
    }
}

struct MmapDevice {
    ptr: *const u8,
    size: u64,
    id: [u8; 8],
}

unsafe impl Send for MmapDevice {}
unsafe impl Sync for MmapDevice {}

impl MmapDevice {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        let size = file.metadata()?.len();
        anyhow::ensure!(size > 0 && size % 512 == 0, "image size {size} unusable");
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as libc::size_t,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_POPULATE,
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                0,
            )
        };
        anyhow::ensure!(ptr != libc::MAP_FAILED, "mmap failed");
        unsafe { libc::madvise(ptr, size as libc::size_t, libc::MADV_HUGEPAGE) };
        unsafe { libc::madvise(ptr, size as libc::size_t, libc::MADV_SEQUENTIAL) };
        let id = size.to_be_bytes();
        Ok(Self {
            ptr: ptr as *const u8,
            size,
            id,
        })
    }
}

impl Drop for MmapDevice {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.size as libc::size_t) };
    }
}

impl BlockDevice for MmapDevice {
    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, len: usize) -> anyhow::Result<impl AsRef<[u8]>> {
        let src = unsafe { std::slice::from_raw_parts(self.ptr.add(offset as usize), len) };
        Ok(src.to_vec())
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let src = unsafe { std::slice::from_raw_parts(self.ptr.add(offset as usize), buf.len()) };
        buf.copy_from_slice(src);
        Ok(())
    }

    fn read_exact_vectored_at(&self, offset: u64, bufs: &mut [&mut [u8]]) -> anyhow::Result<()> {
        let mut off = offset as usize;
        for buf in bufs {
            let src = unsafe { std::slice::from_raw_parts(self.ptr.add(off), buf.len()) };
            buf.copy_from_slice(src);
            off += buf.len();
        }
        Ok(())
    }

    fn id_bytes(&self) -> Vec<u8> {
        self.id.to_vec()
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

fn create_ext4_image(image_path: &Path) -> anyhow::Result<()> {
    let file = fs::File::create(image_path)?;
    file.set_len(IMAGE_SIZE_BYTES)?;
    cmd("mkfs.ext4", &["-F", image_path.to_str().unwrap()])?;
    Ok(())
}

fn populate_fixture(mountpoint: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let small_dir = mountpoint.join("small");
    fs::create_dir_all(&small_dir)?;

    let mut small_file_paths = Vec::with_capacity(SMALL_FILE_COUNT);
    let mut payload = vec![0u8; SMALL_FILE_SIZE];
    for (idx, byte) in payload.iter_mut().enumerate() {
        *byte = (idx % 251) as u8;
    }

    for idx in 0..SMALL_FILE_COUNT {
        let rel = PathBuf::from("small").join(format!("{idx:04}.bin"));
        fs::write(mountpoint.join(&rel), &payload)?;
        small_file_paths.push(rel);
    }

    let mut large = fs::File::create(mountpoint.join("large.bin"))?;
    let mut chunk = vec![0u8; 1 << 20];
    for (idx, byte) in chunk.iter_mut().enumerate() {
        *byte = (idx % 239) as u8;
    }
    for _ in 0..(LARGE_FILE_SIZE / chunk.len()) {
        large.write_all(&chunk)?;
    }
    large.sync_all()?;

    Ok(small_file_paths)
}

fn read_small_files(mountpoint: &Path) -> anyhow::Result<u64> {
    let mut total = 0u64;
    for idx in 0..SMALL_FILE_COUNT {
        let path = mountpoint.join("small").join(format!("{idx:04}.bin"));
        let data = fs::read(path)?;
        total = total.wrapping_add(data.iter().map(|&b| u64::from(b)).sum::<u64>());
    }
    Ok(total)
}

fn read_large_file(mountpoint: &Path) -> anyhow::Result<u64> {
    let mut file = fs::File::open(mountpoint.join("large.bin"))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total = total.wrapping_add(buf[..n].iter().map(|&b| u64::from(b)).sum::<u64>());
    }
    Ok(total)
}

fn attach_loop_device(image_path: &Path) -> anyhow::Result<PathBuf> {
    let output = Command::new("losetup")
        .args(["--find", "--show", "--read-only", image_path.to_str().unwrap()])
        .output()
        .map_err(|e| anyhow::anyhow!("losetup: {e}"))?;
    anyhow::ensure!(
        output.status.success(),
        "losetup --find --show --read-only {:?} failed: {}",
        image_path,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let path = String::from_utf8(output.stdout)?.trim().to_owned();
    anyhow::ensure!(!path.is_empty(), "losetup returned no loop device");
    Ok(PathBuf::from(path))
}

fn detach_loop_device(loop_dev: &Path) -> anyhow::Result<()> {
    cmd("losetup", &["-d", loop_dev.to_str().unwrap()])
}

fn is_mounted(path: &Path) -> bool {
    Command::new("mountpoint")
        .args(["-q", path.to_str().unwrap()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn cmd(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| anyhow::anyhow!("{program}: {e}"))?;
    anyhow::ensure!(status.success(), "{program} {:?} failed: {status}", args);
    Ok(())
}

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

fn wait_for_new_tcm_loop_device(before: &[PathBuf], timeout: Duration) -> anyhow::Result<PathBuf> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        for dev in tcm_loop_block_devices() {
            if !before.contains(&dev) {
                return Ok(dev);
            }
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "no new tcm_loop block device appeared within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

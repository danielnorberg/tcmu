//! Integration test: create an ext4 filesystem, serve it over TCMU, write
//! files through the loopback block device, then verify them via a normal
//! loop mount.
//!
//! # Prerequisites
//!
//! - Must run as root
//! - `target_core_mod`, `target_core_user`, and `tcm_loop` kernel modules loaded
//! - configfs mounted at `/sys/kernel/config`
//! - `mkfs.ext4` available on PATH
//!
//! # Running
//!
//! ```sh
//! TCMU_INTEGRATION_TESTS=1 sudo -E cargo test --features linux-target --test ext4_roundtrip -- --nocapture
//! ```

#[cfg(all(target_os = "linux", feature = "linux-target"))]
mod test {
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::FileExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tcmu::target::TcmuTarget;
    use tcmu::{BlockDevice, TcmuDevice, TcmuDeviceConfig};

    // ── Read-write file-backed block device ───────────────────────────────────

    struct RwFileDevice {
        file: std::fs::File,
        size: u64,
        id: [u8; 8],
    }

    impl RwFileDevice {
        fn open(path: &Path) -> anyhow::Result<Self> {
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            let size = file.metadata()?.len();
            anyhow::ensure!(size > 0 && size % 512 == 0, "image size {size} unusable");
            let id = size.to_be_bytes();
            Ok(Self { file, size, id })
        }
    }

    impl BlockDevice for RwFileDevice {
        fn size_bytes(&self) -> u64 {
            self.size
        }

        fn read_at(&self, offset: u64, len: usize) -> anyhow::Result<impl AsRef<[u8]>> {
            let mut buf = vec![0u8; len];
            self.file.read_exact_at(&mut buf, offset)?;
            Ok(buf)
        }

        fn id_bytes(&self) -> Vec<u8> {
            self.id.to_vec()
        }

        fn is_read_only(&self) -> bool {
            false
        }

        fn write_at(&self, offset: u64, data: &[u8]) -> anyhow::Result<()> {
            let mut off = offset;
            let mut remaining = data;
            while !remaining.is_empty() {
                match self.file.write_at(remaining, off) {
                    Ok(0) => anyhow::bail!("write_at returned 0"),
                    Ok(n) => {
                        remaining = &remaining[n..];
                        off += n as u64;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(())
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn cmd(program: &str, args: &[&str]) -> anyhow::Result<()> {
        let status = Command::new(program)
            .args(args)
            .status()
            .map_err(|e| anyhow::anyhow!("{program}: {e}"))?;
        anyhow::ensure!(status.success(), "{program} {:?} failed: {status}", args);
        Ok(())
    }

    /// Snapshot the set of `/dev/sd*` devices whose sysfs path passes through
    /// a `tcm_loop` adapter.
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

    /// Wait until a `/dev/sd*` device backed by `tcm_loop` appears that was
    /// not present in `before`.
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
                "no new tcm_loop block device appeared within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    // ── Test ─────────────────────────────────────────────────────────────────

    #[test]
    fn ext4_roundtrip() -> anyhow::Result<()> {
        // Skip unless the caller explicitly opts in and has root.
        if std::env::var("TCMU_INTEGRATION_TESTS").is_err() {
            eprintln!("skipping ext4_roundtrip: set TCMU_INTEGRATION_TESTS=1 to enable");
            return Ok(());
        }
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipping ext4_roundtrip: must run as root");
            return Ok(());
        }

        let tmpdir = tempfile::tempdir()?;

        // ── 1. Create a 64 MiB ext4 image ────────────────────────────────────
        let img_path = tmpdir.path().join("test.img");
        {
            let f = fs::File::create(&img_path)?;
            f.set_len(64 << 20)?;
        }
        cmd("mkfs.ext4", &["-F", img_path.to_str().unwrap()])?;

        // ── 2. Wrap in a read-write block device ──────────────────────────────
        let file_device = RwFileDevice::open(&img_path)?;
        let size = file_device.size_bytes();

        // ── 3. Set up TcmuTarget with loopback ────────────────────────────────
        let before_devices = tcm_loop_block_devices();

        let target = Arc::new(
            TcmuTarget::builder()
                .name("ext4-roundtrip")
                .size_bytes(size)
                .with_loopback()
                .build()?,
        );
        eprintln!("UIO device: {}", target.uio_path().display());

        // ── 4. Start the SCSI event loop in a background thread ───────────────
        let device = Arc::new(TcmuDevice::new(
            file_device,
            TcmuDeviceConfig {
                vendor_id: *b"TEST    ",
                product_id: *b"EXT4 ROUNDTRIP  ",
                product_revision: *b"0001",
                device_id_prefix: "ext4-roundtrip".to_string(),
            },
        ));

        let target_t = Arc::clone(&target);
        let device_t = Arc::clone(&device);
        let loop_thread = std::thread::spawn(move || target_t.run(&*device_t));

        // ── 5. Wait for the loopback block device ─────────────────────────────
        let block_dev = wait_for_new_tcm_loop_device(&before_devices, Duration::from_secs(15))?;
        eprintln!("Block device: {}", block_dev.display());

        // ── 6. Mount the block device ─────────────────────────────────────────
        let mnt_tcmu = tmpdir.path().join("mnt_tcmu");
        fs::create_dir_all(&mnt_tcmu)?;
        cmd(
            "mount",
            &[
                "-t",
                "ext4",
                block_dev.to_str().unwrap(),
                mnt_tcmu.to_str().unwrap(),
            ],
        )?;

        // ── 7. Make changes to the filesystem ─────────────────────────────────
        fs::write(mnt_tcmu.join("hello.txt"), b"Hello from TCMU!\n")?;
        fs::create_dir_all(mnt_tcmu.join("subdir"))?;
        fs::write(
            mnt_tcmu.join("subdir").join("data.bin"),
            &[0xDE, 0xAD, 0xBE, 0xEF],
        )?;
        eprintln!("Files written through TCMU block device");

        // ── 8. Sync and unmount ───────────────────────────────────────────────
        // umount flushes dirty pages to the block device before returning.
        cmd("umount", &[mnt_tcmu.to_str().unwrap()])?;
        eprintln!("Unmounted TCMU block device");

        // ── 9. Stop the event loop ────────────────────────────────────────────
        target.stop();
        loop_thread.join().expect("event loop thread panicked")??;
        eprintln!("TCMU event loop stopped");

        // ── 10. Mount via normal Linux loop device ────────────────────────────
        let mnt_loop = tmpdir.path().join("mnt_loop");
        fs::create_dir_all(&mnt_loop)?;
        cmd(
            "mount",
            &[
                "-t",
                "ext4",
                "-o",
                "loop",
                img_path.to_str().unwrap(),
                mnt_loop.to_str().unwrap(),
            ],
        )?;
        eprintln!("Image mounted via loop device");

        // ── 11. Verify the filesystem changes ─────────────────────────────────
        let hello = fs::read(mnt_loop.join("hello.txt"))?;
        assert_eq!(hello, b"Hello from TCMU!\n", "hello.txt mismatch");

        let data = fs::read(mnt_loop.join("subdir").join("data.bin"))?;
        assert_eq!(data, &[0xDE, 0xAD, 0xBE, 0xEF], "data.bin mismatch");

        eprintln!("All filesystem checks passed");

        // ── 12. Cleanup ────────────────────────────────────────────────────────
        cmd("umount", &[mnt_loop.to_str().unwrap()])?;

        Ok(())
    }
}

// Compile successfully on non-Linux or without the linux-target feature.
#[cfg(not(all(target_os = "linux", feature = "linux-target")))]
#[test]
fn ext4_roundtrip_not_supported() {}

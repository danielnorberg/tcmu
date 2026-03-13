//! Serves a filesystem image file as a read-only SCSI block device via TCMU.
//!
//! Uses [`tcmu::target::TcmuTarget`] to handle configfs setup, UIO discovery,
//! and the SCSI command loop automatically.
//!
//! # Prerequisites  (run as root)
//!
//! 1. Build a test image:
//!    ```sh
//!    truncate -s 64M /tmp/ext4.img && mkfs.ext4 /tmp/ext4.img
//!    ```
//!
//! 2. Load modules:
//!    ```sh
//!    modprobe target_core_mod target_core_user tcm_loop
//!    mount -t configfs configfs /sys/kernel/config   # if not already mounted
//!    ```
//!
//! 3. Run:
//!    ```sh
//!    sudo cargo run --example loopback --features linux-target -- /tmp/ext4.img
//!    ```
//!    A new block device (e.g. `/dev/sdb`) will appear. Mount it in another
//!    terminal:
//!    ```sh
//!    mount -o ro /dev/sdb /mnt && ls /mnt
//!    ```
//!
//! 4. Press Ctrl-C to stop. The target and loopback fabric are cleaned up
//!    automatically on exit.

#[cfg(not(all(target_os = "linux", feature = "linux-target")))]
fn main() {
    eprintln!("This example requires Linux and the `linux-target` feature.");
    eprintln!("Run with: cargo run --example loopback --features linux-target -- <image>");
    std::process::exit(1);
}

#[cfg(all(target_os = "linux", feature = "linux-target"))]
fn main() -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{FileExt, MetadataExt};
    use std::path::Path;

    use tcmu::target::TcmuTarget;
    use tcmu::{BlockDevice, TcmuDevice, TcmuDeviceConfig};

    // ── FileDevice: a read-only block device backed by a file ────────────────

    struct FileDevice {
        file: std::fs::File,
        size: u64,
        id: [u8; 8],
    }

    impl FileDevice {
        fn open(path: &Path) -> anyhow::Result<Self> {
            let file = OpenOptions::new().read(true).open(path)?;
            let meta = file.metadata()?;
            let size = meta.len();
            anyhow::ensure!(
                size % 512 == 0,
                "image size {size} is not a multiple of 512"
            );
            let mut id = [0u8; 8];
            id.copy_from_slice(&meta.ino().to_be_bytes());
            Ok(Self { file, size, id })
        }
    }

    impl BlockDevice for FileDevice {
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
    }

    // ── main ─────────────────────────────────────────────────────────────────

    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <image-file>", args[0]);
        std::process::exit(1);
    }
    let image_path = Path::new(&args[1]);

    let file_device = FileDevice::open(image_path)?;
    let size = file_device.size_bytes();
    eprintln!("Image: {} ({size} bytes)", image_path.display());

    // Set up the TCMU target and loopback fabric in one call.
    let target = TcmuTarget::builder()
        .name("loopback")
        .size_bytes(size)
        .with_loopback()
        .build()?;

    eprintln!("UIO device:  {}", target.uio_path().display());
    eprintln!("A block device (/dev/sdX) should now be visible — mount it read-only.");
    eprintln!("Press Ctrl-C to stop and clean up.");

    let device = TcmuDevice::new(
        file_device,
        TcmuDeviceConfig {
            vendor_id: *b"TCMU    ",
            product_id: *b"FILE DEVICE     ",
            product_revision: *b"0001",
            device_id_prefix: "loopback".to_string(),
        },
    );

    // Blocks until I/O error or signal; drops TcmuTarget on exit which
    // cleans up configfs and the loopback fabric.
    target.run(&device)
}

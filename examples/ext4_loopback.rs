//! Serves a filesystem image file as a read-only SCSI block device via the
//! Linux kernel TCMU (Target Core Module Userspace) interface.
//!
//! This is the user-space equivalent of a loopback block device: instead of
//! the kernel reading the file directly, this process handles every SCSI
//! READ command that arrives through TCMU and answers it from the file.
//!
//! # How TCMU works
//!
//! TCMU is a Linux kernel facility that lets user space implement a SCSI
//! target handler.  The kernel and user space share a lock-free ring buffer
//! (mapped via UIO) to exchange SCSI commands and responses:
//!
//! ```text
//!  Initiator (e.g. kernel's own SCSI layer via tcm_loop)
//!       │ SCSI CDB
//!       ▼
//!  LIO target core ──► ring buffer (shared mem, /dev/uioN) ──► this process
//!       ▲                                                           │
//!       └───────────── response (status + data) ───────────────────┘
//! ```
//!
//! # Prerequisites  (run as root)
//!
//! 1. Build a test ext4 image:
//!    ```sh
//!    truncate -s 64M /tmp/ext4.img
//!    mkfs.ext4 /tmp/ext4.img
//!    ```
//!
//! 2. Load kernel modules:
//!    ```sh
//!    modprobe target_core_mod target_core_user tcm_loop
//!    mount -t configfs configfs /sys/kernel/config  # if not already mounted
//!    ```
//!
//! 3. Create the TCMU target via configfs:
//!    ```sh
//!    DEV=/sys/kernel/config/target/core/user_0/ext4disk
//!    mkdir -p $DEV
//!    echo 67108864 > $DEV/dev_size   # 64 MiB — must match your image
//!    echo 1        > $DEV/enable
//!    # The kernel now creates a UIO device; note its number:
//!    ls /sys/class/uio/
//!    ```
//!
//! 4. Expose it as a block device using the loopback fabric:
//!    ```sh
//!    WWN=naa.600000000001
//!    LUN=/sys/kernel/config/target/loopback/$WWN/tpgt_1/lun/lun_0
//!    mkdir -p $LUN
//!    ln -s /sys/kernel/config/target/core/user_0/ext4disk $LUN/ext4disk
//!    echo 1 > /sys/kernel/config/target/loopback/$WWN/tpgt_1/enable
//!    # A new SCSI disk (e.g. /dev/sdb) now appears in the system.
//!    ```
//!
//! 5. Run this example:
//!    ```sh
//!    sudo cargo run --example ext4_loopback -- /tmp/ext4.img /dev/uio0
//!    ```
//!
//! 6. In another terminal, mount the SCSI disk read-only:
//!    ```sh
//!    mount -o ro /dev/sdb /mnt && ls /mnt
//!    ```
//!
//! 7. Teardown:
//!    ```sh
//!    umount /mnt
//!    # Ctrl-C the example process, then:
//!    echo 0 > /sys/kernel/config/target/loopback/$WWN/tpgt_1/enable
//!    rm $LUN/ext4disk
//!    rmdir $LUN /sys/kernel/config/target/loopback/$WWN/tpgt_1
//!    rmdir /sys/kernel/config/target/loopback/$WWN
//!    echo 0 > /sys/kernel/config/target/core/user_0/ext4disk/enable
//!    rmdir /sys/kernel/config/target/core/user_0/ext4disk
//!    ```

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("This example requires Linux.");
    std::process::exit(1);
}

// ─── Linux implementation ─────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::unix::{fs::FileExt, io::AsRawFd};
    use std::path::Path;
    use std::sync::atomic::{fence, Ordering};

    use tcmu::{BlockDevice, TcmuDevice, TcmuDeviceConfig};

    // ── FileDevice ────────────────────────────────────────────────────────────

    /// A read-only block device backed by a regular file.
    struct FileDevice {
        file: File,
        size: u64,
        id: [u8; 8],
    }

    impl FileDevice {
        fn open(path: &Path) -> anyhow::Result<Self> {
            let file = OpenOptions::new().read(true).open(path)?;
            let meta = file.metadata()?;
            let size = meta.len();
            anyhow::ensure!(size % 512 == 0, "image size {size} is not a multiple of 512");

            // Use the file's inode number as a stable device identifier.
            use std::os::unix::fs::MetadataExt;
            let ino = meta.ino();
            let mut id = [0u8; 8];
            id.copy_from_slice(&ino.to_be_bytes());
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

    // ── TCMU ring buffer ──────────────────────────────────────────────────────
    //
    // Shared memory layout (from <linux/target_core_user.h>):
    //
    //  ┌──────────────┐  offset 0
    //  │   mailbox    │  version(u16), flags(u16), cmdr_off(u32), cmdr_size(u32),
    //  │              │  cmd_head(u32) at offset 12,
    //  │              │  cmd_tail(u32) at offset 64  (__aligned(64))
    //  ├──────────────┤  offset cmdr_off
    //  │  cmd ring    │  ring of tcmu_cmd_entry records, size cmdr_size
    //  ├──────────────┤  offset cmdr_off + cmdr_size
    //  │  data area   │  CDB bytes and scatter-gather buffers live here
    //  └──────────────┘
    //
    // tcmu_cmd_entry layout (__packed):
    //   offset  0: u32 len_op     — bits[1:0]=opcode, bits[31:2]=entry length
    //   offset  4: u16 cmd_id
    //   offset  6: u8  kflags
    //   offset  7: u8  uflags
    //   --- request fields (union) ---
    //   offset  8: u32 iov_cnt
    //   offset 12: u32 iov_bidi_cnt
    //   offset 16: u32 iov_dif_cnt
    //   offset 20: u64 cdb_off    — byte offset from mmap base to CDB
    //   offset 28: u64 pad1
    //   offset 36: u64 pad2
    //   offset 44: iovec[iov_cnt] — each iovec: u64 base_offset, u64 len
    //   --- response fields (same union, written by user space) ---
    //   offset  8: i8  scsi_status
    //   offset  9: i8[3] pad
    //   offset 12: u32 pad
    //   offset 16: u32 sense_buffer_len
    //   offset 20: u8[96] sense_buffer
    //
    // iovec.iov_base is a byte offset from the mmap base, NOT a real pointer.

    const TCMU_OP_PAD: u32 = 0;
    const TCMU_OP_CMD: u32 = 1;
    const TCMU_SENSE_BUFFERSIZE: usize = 96;

    // Mailbox field byte offsets.
    const MB_OFF_CMDR_OFF: usize = 4;
    const MB_OFF_CMDR_SIZE: usize = 8;
    const MB_OFF_CMD_HEAD: usize = 12;
    const MB_OFF_CMD_TAIL: usize = 64; // __aligned(64)

    // Command entry field byte offsets.
    const ENTRY_OFF_LEN_OP: usize = 0;
    const ENTRY_OFF_CMD_ID: usize = 4;
    const ENTRY_OFF_IOV_CNT: usize = 8;
    const ENTRY_OFF_CDB_OFF: usize = 20; // u64, unaligned (__packed)
    const ENTRY_OFF_IOVS: usize = 44; // iovec array starts here
    const ENTRY_OFF_RSP_STATUS: usize = 8;
    const ENTRY_OFF_RSP_SENSE_LEN: usize = 16;
    const ENTRY_OFF_RSP_SENSE_BUF: usize = 20;

    // iovec field offsets (each iovec is 16 bytes).
    const IOV_OFF_BASE: usize = 0;
    const IOV_OFF_LEN: usize = 8;
    const IOV_STRIDE: usize = 16;

    /// Read a u32 from an arbitrary (potentially unaligned) offset in a byte slice.
    #[inline]
    unsafe fn read_u32(base: *const u8, off: usize) -> u32 {
        unsafe { (base.add(off) as *const u32).read_unaligned() }
    }

    /// Read a u64 from an arbitrary (potentially unaligned) offset in a byte slice.
    #[inline]
    unsafe fn read_u64(base: *const u8, off: usize) -> u64 {
        unsafe { (base.add(off) as *const u64).read_unaligned() }
    }

    /// Read a volatile u32 (for mailbox head/tail which are written by other parties).
    #[inline]
    unsafe fn read_volatile_u32(base: *const u8, off: usize) -> u32 {
        unsafe { (base.add(off) as *const u32).read_volatile() }
    }

    /// Write a volatile u32 followed by a full memory fence.
    #[inline]
    unsafe fn write_volatile_u32(base: *mut u8, off: usize, val: u32) {
        unsafe { (base.add(off) as *mut u32).write_volatile(val) };
        fence(Ordering::SeqCst);
    }

    // ── mmap size helper ──────────────────────────────────────────────────────

    /// Read the UIO memory map size from sysfs.
    ///
    /// For `/dev/uio0` this reads `/sys/class/uio/uio0/maps/map0/size`.
    fn uio_mmap_size(uio_path: &Path) -> anyhow::Result<usize> {
        let name = uio_path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid uio device path"))?;
        let sysfs = format!("/sys/class/uio/{name}/maps/map0/size");
        let contents = std::fs::read_to_string(&sysfs)
            .map_err(|e| anyhow::anyhow!("reading {sysfs}: {e}"))?;
        let hex = contents.trim().trim_start_matches("0x");
        let size = usize::from_str_radix(hex, 16)?;
        Ok(size)
    }

    // ── TCMU event loop ───────────────────────────────────────────────────────

    pub fn run() -> anyhow::Result<()> {
        let args: Vec<String> = std::env::args().collect();
        if args.len() != 3 {
            eprintln!("Usage: {} <image-file> <uio-device>", args[0]);
            eprintln!("  e.g.: {} /tmp/ext4.img /dev/uio0", args[0]);
            std::process::exit(1);
        }
        let image_path = Path::new(&args[1]);
        let uio_path = Path::new(&args[2]);

        let file_device = FileDevice::open(image_path)?;
        eprintln!(
            "Image: {} ({} bytes, {} 512-byte sectors)",
            image_path.display(),
            file_device.size_bytes(),
            file_device.size_bytes() / 512,
        );

        let device = TcmuDevice::new(
            file_device,
            TcmuDeviceConfig {
                vendor_id: *b"TCMU    ",
                product_id: *b"FILE DEVICE     ",
                product_revision: *b"0001",
                device_id_prefix: "file-device".to_string(),
            },
        );

        // Open the UIO character device.
        let mut uio = OpenOptions::new()
            .read(true)
            .write(true)
            .open(uio_path)
            .map_err(|e| anyhow::anyhow!("opening {}: {e}", uio_path.display()))?;
        let fd = uio.as_raw_fd();

        // Determine shared memory size from sysfs, then mmap it.
        let mmap_size = uio_mmap_size(uio_path)?;
        eprintln!("UIO mmap size: {mmap_size} bytes");

        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mmap_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        anyhow::ensure!(
            base != libc::MAP_FAILED,
            "mmap failed: {}",
            std::io::Error::last_os_error()
        );
        let base = base as *mut u8;

        // Read mailbox header fields (written once by the kernel at startup).
        let cmdr_off = unsafe { read_u32(base, MB_OFF_CMDR_OFF) } as usize;
        let cmdr_size = unsafe { read_u32(base, MB_OFF_CMDR_SIZE) } as usize;
        let cmdr = unsafe { base.add(cmdr_off) };
        eprintln!("Ring buffer: offset={cmdr_off}, size={cmdr_size}");

        eprintln!("Ready — waiting for SCSI commands (Ctrl-C to stop)");

        loop {
            // Block until the kernel signals at least one new command.
            // The kernel writes an event count to the UIO fd; we read it.
            let mut notify = [0u8; 4];
            uio.read_exact(&mut notify)?;

            // Drain every pending entry from the ring buffer.
            loop {
                // Acquire-load cmd_head (kernel-written) and cmd_tail (our pointer).
                let head = unsafe { read_volatile_u32(base, MB_OFF_CMD_HEAD) } as usize;
                let tail = unsafe { read_volatile_u32(base, MB_OFF_CMD_TAIL) } as usize;
                if head == tail {
                    break; // Ring buffer is empty.
                }

                let entry = unsafe { cmdr.add(tail) };
                let len_op = unsafe { read_u32(entry, ENTRY_OFF_LEN_OP) };
                let opcode = len_op & 0x3;
                let entry_len = (len_op >> 2) as usize;

                // A PAD entry means the ring wrapped; skip to the beginning.
                if opcode == TCMU_OP_PAD {
                    unsafe { write_volatile_u32(base, MB_OFF_CMD_TAIL, 0) };
                    continue;
                }

                if opcode != TCMU_OP_CMD || entry_len == 0 {
                    // Unknown opcode or malformed entry — skip it.
                    let new_tail = (tail + entry_len.max(8)) % cmdr_size;
                    unsafe { write_volatile_u32(base, MB_OFF_CMD_TAIL, new_tail as u32) };
                    continue;
                }

                let cmd_id = unsafe { read_u32(entry, ENTRY_OFF_CMD_ID) } as u16;
                let iov_cnt = unsafe { read_u32(entry, ENTRY_OFF_IOV_CNT) } as usize;
                let cdb_off = unsafe { read_u64(entry, ENTRY_OFF_CDB_OFF) } as usize;

                // The CDB lives in the data area at `cdb_off` from the mmap base.
                // We read up to 16 bytes (the maximum standard CDB length).
                let cdb = unsafe { std::slice::from_raw_parts(base.add(cdb_off), 16) };

                // For write commands (which we reject) the IOVs contain the
                // initiator data.  For read commands the IOVs are output
                // buffers we fill in.  Gather data_out for completeness.
                let iov_array = unsafe { entry.add(ENTRY_OFF_IOVS) };
                let mut data_out = Vec::new();
                for i in 0..iov_cnt {
                    let iov = unsafe { iov_array.add(i * IOV_STRIDE) };
                    let iov_base_off = unsafe { read_u64(iov, IOV_OFF_BASE) } as usize;
                    let iov_len = unsafe { read_u64(iov, IOV_OFF_LEN) } as usize;
                    let buf =
                        unsafe { std::slice::from_raw_parts(base.add(iov_base_off), iov_len) };
                    data_out.extend_from_slice(buf);
                }

                // Execute the SCSI command.
                let response = device.execute(cdb, &data_out);

                // For successful reads, scatter response.data into the IOV buffers.
                if response.status == 0x00 && !response.data.is_empty() {
                    let mut remaining = response.data.as_slice();
                    for i in 0..iov_cnt {
                        if remaining.is_empty() {
                            break;
                        }
                        let iov = unsafe { iov_array.add(i * IOV_STRIDE) };
                        let iov_base_off = unsafe { read_u64(iov, IOV_OFF_BASE) } as usize;
                        let iov_len = unsafe { read_u64(iov, IOV_OFF_LEN) } as usize;
                        let copy_len = iov_len.min(remaining.len());
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                remaining.as_ptr(),
                                base.add(iov_base_off),
                                copy_len,
                            );
                        }
                        remaining = &remaining[copy_len..];
                    }
                }

                // Write the SCSI status and sense data back into the entry.
                unsafe {
                    // scsi_status
                    entry.add(ENTRY_OFF_RSP_STATUS).write(response.status);
                    // sense_buffer_len
                    let sense_len = response.sense.len().min(TCMU_SENSE_BUFFERSIZE) as u32;
                    (entry.add(ENTRY_OFF_RSP_SENSE_LEN) as *mut u32).write_unaligned(sense_len);
                    // sense_buffer
                    std::ptr::copy_nonoverlapping(
                        response.sense.as_ptr(),
                        entry.add(ENTRY_OFF_RSP_SENSE_BUF),
                        sense_len as usize,
                    );
                }

                // Advance cmd_tail past this entry to signal completion.
                let new_tail = (tail + entry_len) % cmdr_size;
                unsafe { write_volatile_u32(base, MB_OFF_CMD_TAIL, new_tail as u32) };

                eprintln!(
                    "cmd_id={cmd_id:#06x} opcode={:#04x} status={:#04x}",
                    cdb[0], response.status
                );
            }

            // Notify the kernel that we have advanced cmd_tail.
            // Writing any 4-byte value to the UIO fd triggers the kernel's
            // IRQ control handler, which wakes up any waiters.
            uio.write_all(&1u32.to_ne_bytes())?;
        }
    }
}

//! Linux configfs lifecycle management and UIO event loop for TCMU targets.
//!
//! # Typical usage
//!
//! ```no_run
//! use tcmu::{BlockDevice, TcmuDevice, TcmuDeviceConfig};
//! use tcmu::target::TcmuTarget;
//! # struct MyDevice;
//! # impl BlockDevice for MyDevice {
//! #     fn size_bytes(&self) -> u64 { 4096 }
//! #     fn read_at(&self, _: u64, _: usize) -> anyhow::Result<impl AsRef<[u8]>> { Ok(vec![0u8]) }
//! #     fn id_bytes(&self) -> Vec<u8> { vec![] }
//! # }
//!
//! let target = TcmuTarget::builder()
//!     .name("mydev")
//!     .size_bytes(64 << 20)
//!     .with_loopback()        // also sets up a tcm_loop LUN → /dev/sdX
//!     .build()?;
//!
//! eprintln!("UIO device: {}", target.uio_path().display());
//!
//! let device = TcmuDevice::new(MyDevice, TcmuDeviceConfig {
//!     vendor_id:        *b"VENDOR  ",
//!     product_id:       *b"PRODUCT         ",
//!     product_revision: *b"0001",
//!     device_id_prefix: "mydev".to_string(),
//! });
//!
//! // Blocks until I/O error or signal; cleans up configfs on drop.
//! target.run(&device)?;
//! # anyhow::Ok(())
//! ```
//!
//! # Kernel modules required
//!
//! ```sh
//! modprobe target_core_mod target_core_user
//! modprobe tcm_loop  # only if using .with_loopback()
//! mount -t configfs configfs /sys/kernel/config  # if not already mounted
//! ```

use std::fs;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering, fence};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::{BlockDevice, TcmuDevice};

const CONFIGFS: &str = "/sys/kernel/config/target";

// ─── Builder ──────────────────────────────────────────────────────────────────

/// Builder for a [`TcmuTarget`].
#[derive(Default)]
pub struct TcmuTargetBuilder {
    name: String,
    size_bytes: u64,
    hba_index: u32,
    loopback: bool,
    wwn: Option<String>,
    read_ahead_kb: Option<u32>,
    hw_max_sectors: Option<u32>,
    cmd_time_out: Option<Duration>,
}

impl TcmuTargetBuilder {
    /// Device name as it will appear in configfs (e.g. `"mydev"`).
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Total device size in bytes (must be a multiple of 512).
    pub fn size_bytes(mut self, size: u64) -> Self {
        self.size_bytes = size;
        self
    }

    /// HBA index for the `user_N` configfs slot (default: `0`).
    pub fn hba_index(mut self, idx: u32) -> Self {
        self.hba_index = idx;
        self
    }

    /// Also create a `tcm_loop` fabric so a SCSI block device (`/dev/sdX`)
    /// appears in the system. Requires the `tcm_loop` kernel module.
    ///
    /// The loopback fabric is created lazily inside [`run`](TcmuTarget::run),
    /// after the UIO file is open and ready to service SCSI commands. This
    /// avoids the kernel probing a LUN whose user-space handler is not yet
    /// running.
    pub fn with_loopback(mut self) -> Self {
        self.loopback = true;
        self
    }

    /// Override the auto-generated WWN used for the loopback fabric.
    ///
    /// If not set, a deterministic WWN is derived from the device name.
    pub fn wwn(mut self, wwn: impl Into<String>) -> Self {
        self.wwn = Some(wwn.into());
        self
    }

    /// Set `read_ahead_kb` on the block device queue after the loopback
    /// device appears. Only meaningful with [`with_loopback`](Self::with_loopback).
    ///
    /// If not set, the kernel default (typically 128 KiB) is used.
    pub fn read_ahead_kb(mut self, kb: u32) -> Self {
        self.read_ahead_kb = Some(kb);
        self
    }

    /// Maximum transfer size in 512-byte sectors for a single SCSI command.
    ///
    /// Written to the TCMU `control` file (as `hw_max_sectors=N`) before the
    /// device is enabled. The kernel default is 128 sectors (64 KiB). Higher
    /// values allow the kernel to send larger SCSI READ/WRITE commands,
    /// reducing per-command overhead.
    ///
    /// The practical upper bound is constrained by the TCMU data area size
    /// (`max_data_area_mb`, default 1024 MiB).
    pub fn hw_max_sectors(mut self, sectors: u32) -> Self {
        self.hw_max_sectors = Some(sectors);
        self
    }

    /// SCSI command timeout for the TCMU device.
    ///
    /// Written to the `control` file before the device is enabled. When a
    /// handler crashes (UIO fd closes), in-flight commands time out after
    /// this duration and the kernel completes them with CHECK CONDITION,
    /// allowing cleanup to proceed.
    ///
    /// **Cannot be changed after LUN exports exist.** Defaults to 10 seconds.
    /// Setting to zero disables timeouts entirely — handler crashes become
    /// unrecoverable without `reset_ring`.
    pub fn cmd_time_out(mut self, timeout: Duration) -> Self {
        self.cmd_time_out = Some(timeout);
        self
    }

    /// Create the TCMU target.
    ///
    /// This writes to configfs, which requires root and the `target_core_user`
    /// kernel module to be loaded.
    pub fn build(self) -> anyhow::Result<TcmuTarget> {
        anyhow::ensure!(!self.name.is_empty(), "device name must not be empty");
        anyhow::ensure!(self.size_bytes > 0, "size_bytes must be > 0");
        anyhow::ensure!(
            self.size_bytes.is_multiple_of(512),
            "size_bytes must be a multiple of 512"
        );
        TcmuTarget::create(self)
    }
}

// ─── TcmuTarget ───────────────────────────────────────────────────────────────

/// A TCMU block device target registered with the Linux kernel.
///
/// Cleans up the configfs entries on [`Drop`].
pub struct TcmuTarget {
    device_configfs: PathBuf,
    uio_path: PathBuf,
    loopback_cfg: Option<LoopbackConfig>,
    loopback: Arc<Mutex<Option<LoopbackPaths>>>,
    stop: Arc<AtomicBool>,
}

#[derive(Clone)]
struct LoopbackConfig {
    wwn: String,
    name: String,
}

struct LoopbackStarter {
    device_configfs: PathBuf,
    cfg: LoopbackConfig,
    loopback: Arc<Mutex<Option<LoopbackPaths>>>,
}

struct LoopbackPaths {
    wwn_dir: PathBuf,
    tpgt_dir: PathBuf,
    lun_symlink: PathBuf,
    /// SCSI address from `tpgt_1/address`, e.g. `"0:0:1"` → host0, channel 0,
    /// target 1. The LUN is always 0 (we create exactly one LUN per tpgt).
    scsi_address: Option<String>,
}

impl TcmuTarget {
    /// Create a [`TcmuTargetBuilder`].
    pub fn builder() -> TcmuTargetBuilder {
        TcmuTargetBuilder::default()
    }

    fn create(cfg: TcmuTargetBuilder) -> anyhow::Result<Self> {
        // 1. Create the TCMU configfs device and enable it.
        let device_configfs = PathBuf::from(CONFIGFS)
            .join("core")
            .join(format!("user_{}", cfg.hba_index))
            .join(&cfg.name);

        fs::create_dir_all(&device_configfs)
            .with_context(|| format!("creating {}", device_configfs.display()))?;

        fs::write(
            device_configfs.join("attrib").join("dev_size"),
            cfg.size_bytes.to_string(),
        )
        .context("writing dev_size")?;

        if let Some(sectors) = cfg.hw_max_sectors {
            fs::write(
                device_configfs.join("control"),
                format!("hw_max_sectors={sectors}"),
            )
            .context("writing hw_max_sectors to control")?;
        }

        let timeout = cfg.cmd_time_out.unwrap_or(Duration::from_secs(10));
        fs::write(
            device_configfs.join("control"),
            format!("cmd_time_out={}", timeout.as_secs()),
        )
        .context("writing cmd_time_out to control")?;

        fs::write(device_configfs.join("enable"), "1").context("enabling device")?;

        // 2. Find the UIO device the kernel created (appears under /sys/class/uio/).
        let uio_name = format!("tcm-user/{}/{}", cfg.hba_index, cfg.name);
        let uio_path = wait_for_uio(&uio_name, Duration::from_secs(5))
            .context("waiting for UIO device to appear")?;

        // 3. Optionally remember loopback config to be activated by run().
        let loopback_cfg = if cfg.loopback {
            Some(LoopbackConfig {
                wwn: cfg.wwn.unwrap_or_else(|| derive_wwn(&cfg.name)),
                name: cfg.name,
            })
        } else {
            None
        };

        Ok(Self {
            device_configfs,
            uio_path,
            loopback_cfg,
            loopback: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Path to the UIO character device (e.g. `/dev/uio0`).
    pub fn uio_path(&self) -> &Path {
        &self.uio_path
    }

    /// Gracefully shut down the target.
    ///
    /// If a loopback fabric is active, it is torn down first while the event
    /// loop is still running, so the kernel's SCSI teardown commands can be
    /// serviced normally. Then the event loop is signaled to stop.
    ///
    /// Safe to call from any thread. After calling this, join the event loop
    /// thread (if you spawned one) and then drop the target.
    pub fn stop(&self) {
        if let Ok(mut loopback) = self.loopback.lock()
            && let Some(lb) = loopback.take()
        {
            // Delete the SCSI device first — this is O(1) and removes the
            // block device immediately, rather than relying on the host-wide
            // removal path triggered by configfs teardown (which is slow when
            // many other devices exist).
            if let Some(addr) = &lb.scsi_address {
                delete_scsi_device(addr);
            }
            let _ = fs::write(lb.tpgt_dir.join("enable"), "0");
            let _ = fs::remove_file(&lb.lun_symlink);
            let _ = fs::remove_dir(lb.tpgt_dir.join("lun").join("lun_0"));
            let _ = fs::remove_dir(&lb.tpgt_dir);
            let _ = fs::remove_dir(&lb.wwn_dir);
        }
        self.stop.store(true, Ordering::Release);
    }

    /// Run the blocking SCSI command loop.
    ///
    /// Returns only on I/O error (e.g. the UIO device was destroyed). The
    /// configfs target remains active until this [`TcmuTarget`] is dropped.
    pub fn run<D: BlockDevice>(&self, device: &TcmuDevice<D>) -> anyhow::Result<()> {
        let loopback_starter = self.loopback_cfg.clone().map(|cfg| LoopbackStarter {
            device_configfs: self.device_configfs.clone(),
            cfg,
            loopback: Arc::clone(&self.loopback),
        });
        run_event_loop(
            &self.uio_path,
            device,
            Arc::clone(&self.stop),
            loopback_starter,
        )
    }
}

impl Drop for TcmuTarget {
    fn drop(&mut self) {
        // Best-effort — ignore individual errors so drop never panics.
        //
        // Teardown order matters:
        //
        // 1. reset_ring — force-complete any in-flight TCMU commands so the
        //    kernel's lun_ref count can drain. Without this, the LUN unlink
        //    in step 2 blocks forever if the event loop has already exited.
        //
        // 2. Remove loopback fabric — the kernel sends SCSI commands during
        //    removal (INQUIRY, etc.). If the event loop is still running it
        //    services them; if not, reset_ring already cleared the decks.
        //
        // 3. Remove the TCMU device configfs directory.
        let _ = fs::write(
            self.device_configfs.join("action").join("reset_ring"),
            "2",
        );

        if let Ok(loopback) = self.loopback.lock()
            && let Some(lb) = loopback.as_ref()
        {
            let _ = fs::write(lb.tpgt_dir.join("enable"), "0");
            let _ = fs::remove_file(&lb.lun_symlink);
            let _ = fs::remove_dir(lb.tpgt_dir.join("lun").join("lun_0"));
            let _ = fs::remove_dir(&lb.tpgt_dir);
            let _ = fs::remove_dir(&lb.wwn_dir);
        }

        let _ = fs::remove_dir(&self.device_configfs);
    }
}

impl LoopbackStarter {
    fn run(self, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
        // Let the event loop arm UIO interrupts and enter poll() first.
        std::thread::sleep(Duration::from_millis(50));
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }

        let mut loopback = self
            .loopback
            .lock()
            .map_err(|_| anyhow::anyhow!("loopback state lock poisoned"))?;
        if loopback.is_some() {
            return Ok(());
        }

        let paths = create_loopback(&self.cfg.wwn, &self.device_configfs, &self.cfg.name)
            .context("creating loopback fabric")?;

        // Trigger a targeted SCSI scan on just the host and target assigned to
        // this device, instead of rescanning every tcm_loop host (which is
        // O(N) in existing devices).
        if let Some(addr) = &paths.scsi_address {
            scan_scsi_address(addr);
        }

        *loopback = Some(paths);
        drop(loopback);
        Ok(())
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn create_loopback(wwn: &str, device_configfs: &Path, name: &str) -> anyhow::Result<LoopbackPaths> {
    let wwn_dir = PathBuf::from(CONFIGFS).join("loopback").join(wwn);
    let tpgt_dir = wwn_dir.join("tpgt_1");
    let lun_dir = tpgt_dir.join("lun").join("lun_0");
    let lun_symlink = lun_dir.join(name);

    fs::create_dir_all(&lun_dir)
        .with_context(|| format!("creating LUN directory {}", lun_dir.display()))?;

    // Set the initiator port (nexus) so the kernel can associate an initiator
    // with this target portal group and trigger SCSI host creation.
    fs::write(tpgt_dir.join("nexus"), wwn).context("writing loopback nexus")?;

    std::os::unix::fs::symlink(device_configfs, &lun_symlink).with_context(|| {
        format!(
            "creating LUN symlink {} -> {}",
            lun_symlink.display(),
            device_configfs.display()
        )
    })?;

    // Some kernels require enabling the tpgt explicitly; ignore errors on
    // kernels where this attribute doesn't exist (the tpgt is auto-active).
    let _ = fs::write(tpgt_dir.join("enable"), "1");

    // Read the SCSI address assigned by the kernel (e.g. "0:0:1" for
    // host0, channel 0, target 1). Used for targeted scan and deletion.
    let scsi_address = fs::read_to_string(tpgt_dir.join("address"))
        .ok()
        .map(|s| s.trim().to_string());

    Ok(LoopbackPaths {
        wwn_dir,
        tpgt_dir,
        lun_symlink,
        scsi_address,
    })
}

/// Trigger a targeted SCSI scan for a specific host:channel:target address.
///
/// `address` is the `H:C:T` string from `tpgt_1/address` (e.g. `"0:0:1"`).
/// Writes `"C T 0"` to `/sys/class/scsi_host/host{H}/scan` so only this
/// one LUN is probed — O(1) instead of the O(N) full-host rescan.
fn scan_scsi_address(address: &str) {
    let parts: Vec<&str> = address.split(':').collect();
    if parts.len() != 3 {
        return;
    }
    let (host, channel, target) = (parts[0], parts[1], parts[2]);
    let scan_path = format!("/sys/class/scsi_host/host{host}/scan");
    let _ = fs::write(&scan_path, format!("{channel} {target} 0"));
}

/// Delete a specific SCSI device by its H:C:T address (LUN 0).
///
/// Writes `1` to `/sys/class/scsi_device/H:C:T:0/device/delete`. This is
/// much faster than relying on the host-wide removal path during loopback
/// teardown, especially when many other devices exist on the system.
fn delete_scsi_device(address: &str) {
    let delete_path = format!("/sys/class/scsi_device/{address}:0/device/delete");
    let _ = fs::write(&delete_path, "1");
}


/// Poll `/sys/class/uio/` until a device whose `name` file matches appears.
fn wait_for_uio(expected_name: &str, timeout: Duration) -> anyhow::Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        for entry in fs::read_dir("/sys/class/uio").context("reading /sys/class/uio")? {
            let entry = entry?;
            if let Ok(name) = fs::read_to_string(entry.path().join("name"))
                && name.trim() == expected_name
            {
                return Ok(PathBuf::from("/dev").join(entry.file_name()));
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "UIO device '{expected_name}' did not appear within {}s",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Derive a deterministic NAA WWN from a device name so the loopback target
/// gets a stable identity across restarts.
fn derive_wwn(name: &str) -> String {
    let mut hash: u64 = 5381;
    for byte in name.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(u64::from(byte));
    }
    format!("naa.6{hash:015x}")
}

// ─── UIO event loop ───────────────────────────────────────────────────────────
//
// Shared memory layout (from <linux/target_core_user.h>, all __packed):
//
//  Mailbox (at mmap base):
//    offset  0: u16 version
//    offset  2: u16 flags
//    offset  4: u32 cmdr_off   — byte offset of ring buffer from mmap base
//    offset  8: u32 cmdr_size  — ring buffer size in bytes
//    offset 12: u32 cmd_head   — written by kernel, read by user space
//    offset 64: u32 cmd_tail   — written by user space (__aligned(64))
//
//  Command entry (at cmdr + tail):
//    offset  0: u32 len_op     — bits[1:0]=opcode, bits[31:2]=entry length
//    offset  4: u16 cmd_id
//    offset  6: u8  kflags / u8 uflags
//    --- request (union) ---
//    offset  8: u32 iov_cnt
//    offset 12: u32 iov_bidi_cnt
//    offset 16: u32 iov_dif_cnt
//    offset 20: u64 cdb_off    — offset from mmap base to CDB bytes
//    offset 28: u64 pad × 2
//    offset 44: iovec[iov_cnt] — each: u64 base_offset, u64 len
//    --- response (same union, written by user space) ---
//    offset  8: u8  scsi_status
//    offset 16: u32 sense_buffer_len
//    offset 20: u8[96] sense_buffer

const TCMU_OP_PAD: u32 = 0;
const TCMU_OP_CMD: u32 = 1;
const TCMU_SENSE_BUFFERSIZE: usize = 96;

const MB_CMDR_OFF: usize = 4;
const MB_CMDR_SIZE: usize = 8;
const MB_CMD_HEAD: usize = 12;
const MB_CMD_TAIL: usize = 64;

const ENTRY_LEN_OP: usize = 0;
const ENTRY_IOV_CNT: usize = 8;
// The inner anonymous struct within the union is NOT __packed__, so the
// compiler inserts 4 bytes of alignment padding before the u64 cdb_off field:
//   hdr(8) | iov_cnt(4) | iov_bidi_cnt(4) | iov_dif_cnt(4) | pad(4) | cdb_off(8)
const ENTRY_CDB_OFF: usize = 24;
// iov[] follows __pad1(8) and __pad2(8) after cdb_off, so starts at offset 48.
const ENTRY_IOVS: usize = 48;
const ENTRY_RSP_STATUS: usize = 8;
// In kernel ≥6.x, sense_buffer starts immediately after the 8-byte status/pad
// header (scsi_status(1) + pad(1) + pad(2) + read_len(4) → offset 16).
const ENTRY_RSP_SENSE_BUF: usize = 16;

const IOV_BASE: usize = 0;
const IOV_LEN: usize = 8;
const IOV_STRIDE: usize = 16;

fn command_has_data_out(opcode: u8) -> bool {
    matches!(
        opcode,
        crate::MODE_SELECT_6
            | crate::MODE_SELECT_10
            | crate::WRITE_6
            | crate::WRITE_10
            | crate::WRITE_12
            | crate::WRITE_16
            | crate::WRITE_SAME_10
            | crate::WRITE_SAME_16
    )
}

fn command_has_data_in(opcode: u8) -> bool {
    matches!(
        opcode,
        crate::READ_6 | crate::READ_10 | crate::READ_12 | crate::READ_16
    )
}

/// Set `read_ahead_kb` on a block device queue via sysfs.
///
/// `block_dev` is the device path (e.g. `/dev/sda`). The device must
/// already exist.
pub fn set_read_ahead_kb(block_dev: &Path, kb: u32) -> anyhow::Result<()> {
    set_block_queue_param(block_dev, "read_ahead_kb", &kb.to_string())
}

/// Set `max_sectors_kb` on a block device queue via sysfs.
pub fn set_max_sectors_kb(block_dev: &Path, kb: u32) -> anyhow::Result<()> {
    set_block_queue_param(block_dev, "max_sectors_kb", &kb.to_string())
}

/// Set the I/O scheduler on a block device via sysfs.
///
/// Common values: `"none"`, `"mq-deadline"`, `"kyber"`, `"bfq"`.
pub fn set_scheduler(block_dev: &Path, scheduler: &str) -> anyhow::Result<()> {
    set_block_queue_param(block_dev, "scheduler", scheduler)
}

fn set_block_queue_param(block_dev: &Path, param: &str, value: &str) -> anyhow::Result<()> {
    let dev_name = block_dev
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid block device path: {}", block_dev.display()))?;
    let path = format!("/sys/block/{dev_name}/queue/{param}");
    fs::write(&path, value).with_context(|| format!("writing {value} to {path}"))?;
    Ok(())
}

#[inline]
unsafe fn ru32(base: *const u8, off: usize) -> u32 {
    unsafe { (base.add(off) as *const u32).read_unaligned() }
}

#[inline]
unsafe fn ru64(base: *const u8, off: usize) -> u64 {
    unsafe { (base.add(off) as *const u64).read_unaligned() }
}

#[inline]
unsafe fn rv32(base: *const u8, off: usize) -> u32 {
    unsafe { (base.add(off) as *const u32).read_volatile() }
}

#[inline]
unsafe fn wv32(base: *mut u8, off: usize, val: u32) {
    // Release fence: all prior stores (response data, sense buffer) must be
    // visible before the tail pointer update becomes visible to the kernel.
    fence(Ordering::Release);
    unsafe { (base.add(off) as *mut u32).write_volatile(val) };
}

fn uio_mmap_size(uio_path: &Path) -> anyhow::Result<usize> {
    let name = uio_path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid UIO path"))?;
    let sysfs = format!("/sys/class/uio/{name}/maps/map0/size");
    let raw = fs::read_to_string(&sysfs).with_context(|| format!("reading {sysfs}"))?;
    let hex = raw.trim().trim_start_matches("0x");
    Ok(usize::from_str_radix(hex, 16)?)
}

fn run_event_loop<D: BlockDevice>(
    uio_path: &Path,
    device: &TcmuDevice<D>,
    stop: Arc<AtomicBool>,
    loopback_starter: Option<LoopbackStarter>,
) -> anyhow::Result<()> {
    let mut uio = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(uio_path)
        .with_context(|| format!("opening {}", uio_path.display()))?;

    let mmap_size = uio_mmap_size(uio_path)?;

    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            mmap_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            uio.as_raw_fd(),
            0,
        )
    };
    anyhow::ensure!(
        base != libc::MAP_FAILED,
        "mmap failed: {}",
        std::io::Error::last_os_error()
    );
    let base = base as *mut u8;

    let cmdr_off = unsafe { ru32(base, MB_CMDR_OFF) } as usize;
    let cmdr_size = unsafe { ru32(base, MB_CMDR_SIZE) } as usize;
    let cmdr = unsafe { base.add(cmdr_off) };
    // Arm the UIO fd before creating loopback fabric so the initial SCSI
    // discovery can interrupt user space immediately.
    uio.write_all(&1u32.to_ne_bytes())?;
    let mut loopback_rx = None;
    let mut loopback_handle = None;
    if let Some(starter) = loopback_starter {
        let (tx, rx) = mpsc::sync_channel(1);
        let stop_t = Arc::clone(&stop);
        loopback_handle = Some(std::thread::spawn(move || {
            let _ = tx.send(starter.run(stop_t));
        }));
        loopback_rx = Some(rx);
    }

    let result = 'outer: loop {
        if let Some(rx) = loopback_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(())) => loopback_rx = None,
                Ok(Err(err)) => {
                    stop.store(true, Ordering::Release);
                    break 'outer Err(err);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    stop.store(true, Ordering::Release);
                    break 'outer Err(anyhow::anyhow!("loopback setup thread disconnected"));
                }
            }
        }

        // Poll with a 100 ms timeout so the stop flag is checked promptly.
        let mut pfd = libc::pollfd {
            fd: uio.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, 100) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                if stop.load(Ordering::Acquire) {
                    break 'outer Ok(());
                }
                continue;
            }
            stop.store(true, Ordering::Release);
            break 'outer Err(err.into());
        }
        if stop.load(Ordering::Acquire) {
            break 'outer Ok(());
        }
        if ret > 0 {
            // POLLIN: consume the interrupt counter so the next poll starts fresh.
            let mut buf = [0u8; 4];
            if let Err(err) = uio.read_exact(&mut buf) {
                stop.store(true, Ordering::Release);
                break 'outer Err(err.into());
            }
        }
        // Drain all pending entries (including commands queued before the UIO
        // file was opened, which don't generate a new POLLIN event).
        loop {
            let head = unsafe { rv32(base, MB_CMD_HEAD) } as usize;
            let tail = unsafe { rv32(base, MB_CMD_TAIL) } as usize;
            if head == tail {
                break;
            }

            let entry = unsafe { cmdr.add(tail) };
            let len_op = unsafe { ru32(entry, ENTRY_LEN_OP) };
            // Low 3 bits are the opcode; remaining bits (already byte-aligned
            // to TCMU_OP_ALIGN_SIZE=8) are the entry length in bytes.
            let opcode = len_op & 0x7;
            let entry_len = (len_op & !0x7) as usize;

            // PAD entry: ring wrapped around, restart from the beginning.
            if opcode == TCMU_OP_PAD {
                unsafe { wv32(base, MB_CMD_TAIL, 0) };
                continue;
            }

            if opcode != TCMU_OP_CMD || entry_len == 0 {
                let new_tail = (tail + entry_len.max(8)) % cmdr_size;
                unsafe { wv32(base, MB_CMD_TAIL, new_tail as u32) };
                continue;
            }

            let iov_cnt = unsafe { ru32(entry, ENTRY_IOV_CNT) } as usize;
            let cdb_off = unsafe { ru64(entry, ENTRY_CDB_OFF) } as usize;
            let cdb = unsafe { std::slice::from_raw_parts(base.add(cdb_off), 16) };

            // Only DATA OUT commands carry initiator payload in their IOVs.
            // Reads use the same IOV array for response buffers; copying those
            // pages into a temporary Vec just burns memory bandwidth.
            let iov_arr = unsafe { entry.add(ENTRY_IOVS) };
            let data_out = if command_has_data_out(cdb[0]) {
                let mut data_out = Vec::new();
                for i in 0..iov_cnt {
                    let iov = unsafe { iov_arr.add(i * IOV_STRIDE) };
                    let off = unsafe { ru64(iov, IOV_BASE) } as usize;
                    let len = unsafe { ru64(iov, IOV_LEN) } as usize;
                    data_out.extend_from_slice(unsafe {
                        std::slice::from_raw_parts(base.add(off), len)
                    });
                }
                data_out
            } else {
                Vec::new()
            };

            let response = if command_has_data_in(cdb[0]) {
                let mut data_in = Vec::with_capacity(iov_cnt);
                for i in 0..iov_cnt {
                    let iov = unsafe { iov_arr.add(i * IOV_STRIDE) };
                    let off = unsafe { ru64(iov, IOV_BASE) } as usize;
                    let len = unsafe { ru64(iov, IOV_LEN) } as usize;
                    data_in.push(unsafe { std::slice::from_raw_parts_mut(base.add(off), len) });
                }
                device.execute_into(cdb, &data_out, &mut data_in)
            } else {
                device.execute(cdb, &data_out)
            };

            // For successful reads, scatter response data into the IOV buffers.
            if response.status == 0x00 && !response.data.is_empty() {
                let mut src = response.data.as_slice();
                for i in 0..iov_cnt {
                    if src.is_empty() {
                        break;
                    }
                    let iov = unsafe { iov_arr.add(i * IOV_STRIDE) };
                    let off = unsafe { ru64(iov, IOV_BASE) } as usize;
                    let len = (unsafe { ru64(iov, IOV_LEN) } as usize).min(src.len());
                    unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), base.add(off), len) };
                    src = &src[len..];
                }
            }

            // Write status and sense data back into the entry.
            unsafe {
                entry.add(ENTRY_RSP_STATUS).write(response.status);
                let sense_len = response.sense.len().min(TCMU_SENSE_BUFFERSIZE);
                std::ptr::copy_nonoverlapping(
                    response.sense.as_ptr(),
                    entry.add(ENTRY_RSP_SENSE_BUF),
                    sense_len,
                );
            }

            // Advance cmd_tail to signal completion to the kernel.
            let new_tail = (tail + entry_len) % cmdr_size;
            unsafe { wv32(base, MB_CMD_TAIL, new_tail as u32) };
        }

        // Notify the kernel that cmd_tail has moved.
        if let Err(err) = uio.write_all(&1u32.to_ne_bytes()) {
            stop.store(true, Ordering::Release);
            break 'outer Err(err.into());
        }
    };

    if let Some(handle) = loopback_handle.take() {
        if result.is_err() {
            let _ = handle.join();
        } else if handle.join().is_err() {
            return Err(anyhow::anyhow!("loopback setup thread panicked"));
        }
    }

    result
}

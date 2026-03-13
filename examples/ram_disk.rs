//! A minimal in-memory block device exposed via the SCSI/TCMU command processor.
//!
//! This example shows how to implement [`BlockDevice`] for a simple byte buffer
//! and exercise the SCSI command layer directly, without a real TCMU kernel
//! interface. In a production integration you would forward the CDB bytes from
//! the kernel (e.g. via `/dev/tcmu-*` or tcmu-runner) to `TcmuDevice::execute`
//! and write the response back.

use tcmu::{BlockDevice, TcmuDevice, TcmuDeviceConfig};

/// A flat, read-only in-memory block device.
struct RamDisk {
    /// Device contents. Must be a multiple of 512 bytes.
    data: Vec<u8>,
    /// Stable identifier — used to derive the SCSI serial number.
    id: [u8; 8],
}

impl RamDisk {
    fn new(data: Vec<u8>) -> Self {
        assert!(
            data.len() % 512 == 0,
            "RamDisk size must be a multiple of 512 bytes"
        );
        // Use the first 8 bytes of the data as a simple stable id.
        let mut id = [0u8; 8];
        let copy_len = data.len().min(8);
        id[..copy_len].copy_from_slice(&data[..copy_len]);
        Self { data, id }
    }
}

impl BlockDevice for RamDisk {
    fn size_bytes(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&self, offset: u64, len: usize) -> anyhow::Result<impl AsRef<[u8]>> {
        let start = offset as usize;
        let end = start + len;
        anyhow::ensure!(end <= self.data.len(), "read out of range");
        Ok(self.data[start..end].to_vec())
    }

    fn id_bytes(&self) -> Vec<u8> {
        self.id.to_vec()
    }
}

fn main() {
    // Build a 4-sector (2 KiB) disk filled with a recognizable pattern.
    let mut contents = Vec::with_capacity(4 * 512);
    for sector in 0u8..4 {
        contents.extend(std::iter::repeat(sector).take(512));
    }

    let device = TcmuDevice::new(
        RamDisk::new(contents),
        TcmuDeviceConfig {
            vendor_id: *b"EXAMPLE ",
            product_id: *b"RAM DISK        ",
            product_revision: *b"0001",
            device_id_prefix: "ram-disk".to_string(),
        },
    );

    println!(
        "Device: {} blocks × {} bytes/block",
        device.logical_block_count(),
        device.logical_block_size(),
    );

    // --- INQUIRY (standard) ---
    let resp = device.execute(&[0x12, 0x00, 0x00, 0x00, 36, 0x00], &[]);
    assert_eq!(resp.status, 0x00, "INQUIRY failed");
    println!(
        "Vendor:   {:?}",
        std::str::from_utf8(&resp.data[8..16]).unwrap().trim()
    );
    println!(
        "Product:  {:?}",
        std::str::from_utf8(&resp.data[16..32]).unwrap().trim()
    );
    println!(
        "Revision: {:?}",
        std::str::from_utf8(&resp.data[32..36]).unwrap().trim()
    );

    // --- READ CAPACITY(10) ---
    let resp = device.execute(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], &[]);
    assert_eq!(resp.status, 0x00, "READ CAPACITY failed");
    let last_lba = u32::from_be_bytes(resp.data[0..4].try_into().unwrap());
    let block_size = u32::from_be_bytes(resp.data[4..8].try_into().unwrap());
    println!("Capacity: last LBA={last_lba}, block size={block_size}");

    // --- READ(10): read each sector and verify the fill pattern ---
    for lba in 0u32..4 {
        let mut cdb = [0u8; 10];
        cdb[0] = 0x28; // READ(10)
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&1u16.to_be_bytes()); // 1 block
        let resp = device.execute(&cdb, &[]);
        assert_eq!(resp.status, 0x00, "READ failed for LBA {lba}");
        assert!(
            resp.data.iter().all(|&b| b == lba as u8),
            "unexpected data in sector {lba}"
        );
        println!("Sector {lba}: all bytes = 0x{:02x} ✓", lba);
    }

    // --- WRITE(10): must be rejected with WRITE PROTECTED ---
    let resp = device.execute(&[0x2a, 0, 0, 0, 0, 0, 0, 0, 1, 0], &[]);
    assert_eq!(resp.status, 0x02, "expected CHECK CONDITION for write");
    assert_eq!(resp.sense[2], 0x07, "expected SENSE KEY = DATA PROTECT");
    assert_eq!(resp.sense[12], 0x27, "expected ASC = WRITE PROTECTED");
    println!("WRITE(10): correctly rejected with WRITE PROTECTED ✓");
}

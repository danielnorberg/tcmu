//! Generic SCSI/TCMU command processor for read-only user-space block devices.
//!
//! Implement [`BlockDevice`] for your backing store, then wrap it in
//! [`TcmuDevice`] to get a SCSI command executor suitable for use with TCMU.
//!
//! # Optional: Linux target management
//!
//! Enable the `linux-target` feature for [`target`], which handles the
//! configfs lifecycle (creating/destroying the TCMU device in the kernel) and
//! the UIO event loop, so you don't have to write that plumbing yourself.

#[cfg(all(target_os = "linux", feature = "linux-target"))]
pub mod target;

const LOGICAL_BLOCK_SIZE: u32 = 512;
const INQUIRY: u8 = 0x12;
const REQUEST_SENSE: u8 = 0x03;
const TEST_UNIT_READY: u8 = 0x00;
const READ_CAPACITY_10: u8 = 0x25;
const SERVICE_ACTION_IN_16: u8 = 0x9e;
const READ_6: u8 = 0x08;
const READ_10: u8 = 0x28;
const READ_12: u8 = 0xa8;
const READ_16: u8 = 0x88;
const WRITE_6: u8 = 0x0a;
const WRITE_10: u8 = 0x2a;
const WRITE_12: u8 = 0xaa;
const WRITE_16: u8 = 0x8a;
const WRITE_SAME_10: u8 = 0x41;
const WRITE_SAME_16: u8 = 0x93;
const SYNCHRONIZE_CACHE_10: u8 = 0x35;
const SYNCHRONIZE_CACHE_16: u8 = 0x91;
const MODE_SENSE_6: u8 = 0x1a;
const MODE_SENSE_10: u8 = 0x5a;

const SAM_STAT_GOOD: u8 = 0x00;
const SAM_STAT_CHECK_CONDITION: u8 = 0x02;
const INQUIRY_STANDARD: u8 = 0x00;
const INQUIRY_VPD_SUPPORTED_PAGES: u8 = 0x00;
const INQUIRY_VPD_UNIT_SERIAL: u8 = 0x80;
const INQUIRY_VPD_DEVICE_ID: u8 = 0x83;
const SENSE_FIXED_CURRENT: u8 = 0x70;
const SENSE_KEY_NO_SENSE: u8 = 0x00;
const SENSE_KEY_ILLEGAL_REQUEST: u8 = 0x05;
const SENSE_KEY_DATA_PROTECT: u8 = 0x07;
const ASC_INVALID_OPCODE: u8 = 0x20;
const ASCQ_NONE: u8 = 0x00;
const ASC_LBA_OUT_OF_RANGE: u8 = 0x21;
const ASC_WRITE_PROTECTED: u8 = 0x27;
const SERVICE_ACTION_READ_CAPACITY_16: u8 = 0x10;
const MODE_SENSE_PAGE_CODE_ALL: u8 = 0x3f;
const MODE_SENSE_PAGE_CODE_CACHING: u8 = 0x08;

/// A read-only user-space block device that can be exposed via TCMU.
pub trait BlockDevice {
    /// Total size of the device in bytes.
    fn size_bytes(&self) -> u64;

    /// Read `len` bytes starting at `offset`. Returns an error if the range
    /// is out of bounds or the read fails.
    fn read_at(&self, offset: u64, len: usize) -> anyhow::Result<impl AsRef<[u8]>>;

    /// Opaque identifier bytes used to derive the SCSI serial number and
    /// device identification VPD page. The bytes may be any length;
    /// they are hex-encoded to produce a human-readable serial string.
    fn id_bytes(&self) -> Vec<u8>;
}

/// Identity strings reported to the SCSI initiator via INQUIRY responses.
pub struct TcmuDeviceConfig {
    /// 8-byte vendor identification field (padded with spaces).
    pub vendor_id: [u8; 8],
    /// 16-byte product identification field (padded with spaces).
    pub product_id: [u8; 16],
    /// 4-byte product revision level.
    pub product_revision: [u8; 4],
    /// Prefix prepended to the hex serial in the VPD device identification
    /// page, e.g. `"mydevice"` produces `"mydevice:<hex-serial>"`.
    pub device_id_prefix: String,
}

/// Read-only TCMU-facing SCSI command processor backed by a [`BlockDevice`].
pub struct TcmuDevice<D> {
    device: D,
    config: TcmuDeviceConfig,
}

/// Result of executing one SCSI command against a [`TcmuDevice`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TcmuResponse {
    pub status: u8,
    pub data: Vec<u8>,
    pub sense: Vec<u8>,
}

impl<D: BlockDevice> TcmuDevice<D> {
    /// Wrap a block device with the given SCSI identity configuration.
    pub fn new(device: D, config: TcmuDeviceConfig) -> Self {
        Self { device, config }
    }

    /// Execute a single SCSI CDB and return the resulting status, payload, and
    /// sense data.
    pub fn execute(&self, cdb: &[u8], _data_out: &[u8]) -> TcmuResponse {
        if cdb.is_empty() {
            return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
        }

        match cdb[0] {
            TEST_UNIT_READY => good(Vec::new()),
            REQUEST_SENSE => self.request_sense(cdb),
            INQUIRY => self.inquiry(cdb),
            READ_CAPACITY_10 => self.read_capacity_10(cdb),
            SERVICE_ACTION_IN_16 => self.service_action_in_16(cdb),
            MODE_SENSE_6 => self.mode_sense_6(cdb),
            MODE_SENSE_10 => self.mode_sense_10(cdb),
            READ_6 => self.read_6(cdb),
            READ_10 => self.read_10(cdb),
            READ_12 => self.read_12(cdb),
            READ_16 => self.read_16(cdb),
            SYNCHRONIZE_CACHE_10 | SYNCHRONIZE_CACHE_16 => good(Vec::new()),
            WRITE_6 | WRITE_10 | WRITE_12 | WRITE_16 | WRITE_SAME_10 | WRITE_SAME_16 => {
                check_condition(SENSE_KEY_DATA_PROTECT, ASC_WRITE_PROTECTED, ASCQ_NONE)
            }
            _ => check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE),
        }
    }

    /// SCSI logical block size reported to TCMU consumers.
    pub fn logical_block_size(&self) -> u32 {
        LOGICAL_BLOCK_SIZE
    }

    /// Number of logical blocks reported to TCMU consumers.
    pub fn logical_block_count(&self) -> u64 {
        self.device.size_bytes() / u64::from(self.logical_block_size())
    }

    fn inquiry(&self, cdb: &[u8]) -> TcmuResponse {
        if cdb.len() < 6 {
            return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
        }
        let evpd = cdb[1] & 0x01 != 0;
        let page_code = cdb[2];
        let alloc_len = usize::from(read_be_u16(&cdb[3..5]));
        let data = if !evpd {
            if page_code != INQUIRY_STANDARD {
                return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
            }
            let mut buf = vec![0_u8; 36];
            buf[0] = 0x00;
            buf[2] = 0x06;
            buf[3] = 0x02;
            buf[4] = (buf.len() - 5) as u8;
            buf[7] = 0x02;
            buf[8..16].copy_from_slice(&self.config.vendor_id);
            buf[16..32].copy_from_slice(&self.config.product_id);
            buf[32..36].copy_from_slice(&self.config.product_revision);
            buf
        } else {
            self.vpd_page(page_code)
        };
        good(truncate_to_alloc_len(data, alloc_len))
    }

    fn vpd_page(&self, page_code: u8) -> Vec<u8> {
        match page_code {
            INQUIRY_VPD_SUPPORTED_PAGES => {
                let pages = [
                    INQUIRY_VPD_SUPPORTED_PAGES,
                    INQUIRY_VPD_UNIT_SERIAL,
                    INQUIRY_VPD_DEVICE_ID,
                ];
                let mut buf = vec![0_u8; 4 + pages.len()];
                buf[1] = INQUIRY_VPD_SUPPORTED_PAGES;
                put_be_u16(&mut buf[2..4], pages.len() as u16);
                buf[4..].copy_from_slice(&pages);
                buf
            }
            INQUIRY_VPD_UNIT_SERIAL => {
                let serial = self.hex_serial();
                let mut buf = vec![0_u8; 4 + serial.len()];
                buf[1] = INQUIRY_VPD_UNIT_SERIAL;
                put_be_u16(&mut buf[2..4], serial.len() as u16);
                buf[4..].copy_from_slice(serial.as_bytes());
                buf
            }
            INQUIRY_VPD_DEVICE_ID => {
                let ident = format!(
                    "{prefix}:{serial}",
                    prefix = self.config.device_id_prefix,
                    serial = self.hex_serial()
                );
                let ident_bytes = ident.as_bytes();
                let descriptor_len = ident_bytes.len() + 4;
                let mut buf = vec![0_u8; 4 + descriptor_len];
                buf[1] = INQUIRY_VPD_DEVICE_ID;
                put_be_u16(&mut buf[2..4], descriptor_len as u16);
                buf[4] = 0x02;
                buf[5] = 0x08;
                buf[7] = ident_bytes.len() as u8;
                buf[8..8 + ident_bytes.len()].copy_from_slice(ident_bytes);
                buf
            }
            _ => Vec::new(),
        }
    }

    fn request_sense(&self, cdb: &[u8]) -> TcmuResponse {
        let alloc_len = cdb.get(4).copied().unwrap_or(0) as usize;
        let sense = sense_data(SENSE_KEY_NO_SENSE, 0, 0);
        good(truncate_to_alloc_len(sense, alloc_len))
    }

    fn read_capacity_10(&self, cdb: &[u8]) -> TcmuResponse {
        if cdb.len() < 10 {
            return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
        }
        let mut buf = vec![0_u8; 8];
        let blocks = self.logical_block_count();
        let last_lba = blocks.saturating_sub(1).min(u32::MAX as u64) as u32;
        put_be_u32(&mut buf[0..4], last_lba);
        put_be_u32(&mut buf[4..8], self.logical_block_size());
        good(buf)
    }

    fn service_action_in_16(&self, cdb: &[u8]) -> TcmuResponse {
        if cdb.len() < 16 || (cdb[1] & 0x1f) != SERVICE_ACTION_READ_CAPACITY_16 {
            return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
        }
        let alloc_len = read_be_u32(&cdb[10..14]) as usize;
        let mut buf = vec![0_u8; 32];
        put_be_u64(&mut buf[0..8], self.logical_block_count().saturating_sub(1));
        put_be_u32(&mut buf[8..12], self.logical_block_size());
        good(truncate_to_alloc_len(buf, alloc_len))
    }

    fn mode_sense_6(&self, cdb: &[u8]) -> TcmuResponse {
        if cdb.len() < 6 {
            return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
        }
        let page_code = cdb[2] & 0x3f;
        let alloc_len = cdb[4] as usize;
        let page = match self.mode_sense_page(page_code) {
            Some(page) => page,
            None => {
                return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
            }
        };
        let mut buf = vec![0_u8; 4 + page.len()];
        buf[0] = (buf.len() - 1) as u8;
        buf[2] = 0x80;
        buf[4..].copy_from_slice(&page);
        good(truncate_to_alloc_len(buf, alloc_len))
    }

    fn mode_sense_10(&self, cdb: &[u8]) -> TcmuResponse {
        if cdb.len() < 10 {
            return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
        }
        let page_code = cdb[2] & 0x3f;
        let alloc_len = read_be_u16(&cdb[7..9]) as usize;
        let page = match self.mode_sense_page(page_code) {
            Some(page) => page,
            None => {
                return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
            }
        };
        let mut buf = vec![0_u8; 8 + page.len()];
        let mode_data_len = (buf.len() - 2) as u16;
        put_be_u16(&mut buf[0..2], mode_data_len);
        buf[3] = 0x80;
        buf[8..].copy_from_slice(&page);
        good(truncate_to_alloc_len(buf, alloc_len))
    }

    fn mode_sense_page(&self, page_code: u8) -> Option<Vec<u8>> {
        match page_code {
            MODE_SENSE_PAGE_CODE_CACHING | MODE_SENSE_PAGE_CODE_ALL => {
                let mut page = vec![0_u8; 20];
                page[0] = MODE_SENSE_PAGE_CODE_CACHING;
                page[1] = 18;
                Some(page)
            }
            _ => None,
        }
    }

    fn read_6(&self, cdb: &[u8]) -> TcmuResponse {
        if cdb.len() < 6 {
            return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
        }
        let lba = u32::from(cdb[1] & 0x1f) << 16 | u32::from(cdb[2]) << 8 | u32::from(cdb[3]);
        let transfer = if cdb[4] == 0 { 256 } else { u32::from(cdb[4]) };
        self.read_blocks(u64::from(lba), transfer)
    }

    fn read_10(&self, cdb: &[u8]) -> TcmuResponse {
        if cdb.len() < 10 {
            return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
        }
        let lba = u64::from(read_be_u32(&cdb[2..6]));
        let transfer = u32::from(read_be_u16(&cdb[7..9]));
        self.read_blocks(lba, transfer)
    }

    fn read_12(&self, cdb: &[u8]) -> TcmuResponse {
        if cdb.len() < 12 {
            return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
        }
        let lba = u64::from(read_be_u32(&cdb[2..6]));
        let transfer = read_be_u32(&cdb[6..10]);
        self.read_blocks(lba, transfer)
    }

    fn read_16(&self, cdb: &[u8]) -> TcmuResponse {
        if cdb.len() < 16 {
            return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE, ASCQ_NONE);
        }
        let lba = read_be_u64(&cdb[2..10]);
        let transfer = read_be_u32(&cdb[10..14]);
        self.read_blocks(lba, transfer)
    }

    fn read_blocks(&self, lba: u64, transfer_blocks: u32) -> TcmuResponse {
        let byte_len = u64::from(transfer_blocks) * u64::from(LOGICAL_BLOCK_SIZE);
        let offset = lba * u64::from(LOGICAL_BLOCK_SIZE);
        if offset
            .checked_add(byte_len)
            .is_none_or(|end| end > self.device.size_bytes())
        {
            return check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_LBA_OUT_OF_RANGE, ASCQ_NONE);
        }

        match self.device.read_at(offset, byte_len as usize) {
            Ok(bytes) => good(bytes.as_ref().to_vec()),
            Err(_) => check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_LBA_OUT_OF_RANGE, ASCQ_NONE),
        }
    }

    fn hex_serial(&self) -> String {
        self.device
            .id_bytes()
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

fn good(data: Vec<u8>) -> TcmuResponse {
    TcmuResponse {
        status: SAM_STAT_GOOD,
        data,
        sense: Vec::new(),
    }
}

fn check_condition(sense_key: u8, asc: u8, ascq: u8) -> TcmuResponse {
    TcmuResponse {
        status: SAM_STAT_CHECK_CONDITION,
        data: Vec::new(),
        sense: sense_data(sense_key, asc, ascq),
    }
}

fn sense_data(sense_key: u8, asc: u8, ascq: u8) -> Vec<u8> {
    let mut sense = vec![0_u8; 18];
    sense[0] = SENSE_FIXED_CURRENT;
    sense[2] = sense_key;
    sense[7] = 10;
    sense[12] = asc;
    sense[13] = ascq;
    sense
}

fn truncate_to_alloc_len(mut data: Vec<u8>, alloc_len: usize) -> Vec<u8> {
    data.truncate(alloc_len.min(data.len()));
    data
}

fn read_be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes(bytes.try_into().expect("slice has exact length"))
}

fn read_be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("slice has exact length"))
}

fn read_be_u64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().expect("slice has exact length"))
}

fn put_be_u16(dst: &mut [u8], value: u16) {
    dst.copy_from_slice(&value.to_be_bytes());
}

fn put_be_u32(dst: &mut [u8], value: u32) {
    dst.copy_from_slice(&value.to_be_bytes());
}

fn put_be_u64(dst: &mut [u8], value: u64) {
    dst.copy_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FlatDevice {
        data: Vec<u8>,
        id: [u8; 4],
    }

    impl FlatDevice {
        fn new(data: Vec<u8>) -> Self {
            let len = data.len() as u32;
            Self {
                data,
                id: len.to_be_bytes(),
            }
        }
    }

    impl BlockDevice for FlatDevice {
        fn size_bytes(&self) -> u64 {
            self.data.len() as u64
        }

        fn read_at(&self, offset: u64, len: usize) -> anyhow::Result<impl AsRef<[u8]>> {
            Ok(self.data[offset as usize..offset as usize + len].to_vec())
        }

        fn id_bytes(&self) -> Vec<u8> {
            self.id.to_vec()
        }
    }

    fn test_config() -> TcmuDeviceConfig {
        TcmuDeviceConfig {
            vendor_id: *b"TESTVEN ",
            product_id: *b"TESTPROD        ",
            product_revision: *b"0001",
            device_id_prefix: "test-device".to_string(),
        }
    }

    fn make_device(size_blocks: usize) -> TcmuDevice<FlatDevice> {
        let data = vec![0xAB_u8; size_blocks * LOGICAL_BLOCK_SIZE as usize];
        TcmuDevice::new(FlatDevice::new(data), test_config())
    }

    #[test]
    fn inquiry_reports_configured_vendor_and_product() {
        let dev = make_device(4);
        let resp = dev.execute(&[INQUIRY, 0, 0, 0, 36, 0], &[]);
        assert_eq!(resp.status, SAM_STAT_GOOD);
        assert_eq!(resp.data[0], 0x00);
        assert_eq!(&resp.data[8..16], b"TESTVEN ");
        assert_eq!(&resp.data[16..32], b"TESTPROD        ");
    }

    #[test]
    fn read_capacity_matches_block_count() {
        let dev = make_device(8);
        let resp = dev.execute(&[READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0], &[]);
        assert_eq!(resp.status, SAM_STAT_GOOD);
        let last_lba = read_be_u32(&resp.data[0..4]) as u64;
        let block_size = read_be_u32(&resp.data[4..8]);
        assert_eq!(block_size, LOGICAL_BLOCK_SIZE);
        assert_eq!(last_lba + 1, dev.logical_block_count());
    }

    #[test]
    fn read_10_returns_correct_data() {
        let dev = make_device(4);
        let mut cdb = [0_u8; 10];
        cdb[0] = READ_10;
        put_be_u32(&mut cdb[2..6], 0);
        put_be_u16(&mut cdb[7..9], 1);
        let resp = dev.execute(&cdb, &[]);
        assert_eq!(resp.status, SAM_STAT_GOOD);
        assert_eq!(resp.data.len(), LOGICAL_BLOCK_SIZE as usize);
        assert!(resp.data.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn write_commands_are_rejected() {
        let dev = make_device(4);
        let resp = dev.execute(&[WRITE_10, 0, 0, 0, 0, 0, 0, 0, 1, 0], &[]);
        assert_eq!(resp.status, SAM_STAT_CHECK_CONDITION);
        assert_eq!(resp.sense[2], SENSE_KEY_DATA_PROTECT);
        assert_eq!(resp.sense[12], ASC_WRITE_PROTECTED);
    }

    #[test]
    fn out_of_range_read_returns_check_condition() {
        let dev = make_device(2);
        let mut cdb = [0_u8; 10];
        cdb[0] = READ_10;
        put_be_u32(&mut cdb[2..6], 100);
        put_be_u16(&mut cdb[7..9], 1);
        let resp = dev.execute(&cdb, &[]);
        assert_eq!(resp.status, SAM_STAT_CHECK_CONDITION);
        assert_eq!(resp.sense[2], SENSE_KEY_ILLEGAL_REQUEST);
        assert_eq!(resp.sense[12], ASC_LBA_OUT_OF_RANGE);
    }
}

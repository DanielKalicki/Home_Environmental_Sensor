//! Keeping a small blob in the on-board flash across reboots.
//!
//! The firmware has one thing worth remembering between runs: the gas baseline
//! BSEC spends hours learning. This module stores it, and is deliberately not
//! specific to it — it is a single named slot holding an opaque byte string.
//!
//! Two things make writing to the flash a device-level operation rather than a
//! file operation:
//!
//! - The flash is also where the running code lives. Erasing or writing it
//!   requires the instruction cache to be turned off, so no code fetched from
//!   flash can run during the operation. [`esp_storage`] handles this by
//!   placing the routines in RAM and taking a critical section around them, so
//!   the write blocks every task and interrupt for as long as it takes.
//! - The flash erases in whole 4096-byte sectors and can only clear bits when
//!   it does. Rewriting one byte means erasing the sector around it, so a
//!   255-byte record still costs a sector erase, and the sector is briefly
//!   blank in the middle of it.
//!
//! That second point is why the record carries a checksum. Losing power during
//! a save leaves a half-written sector, and a partial BSEC state handed back to
//! the library would be read as a valid one. A record that does not check out
//! is reported the same way as an empty slot, so the caller simply starts over.
//!
//! Wear is not a concern at the rate this is used. Flash sectors tolerate on
//! the order of 100000 erase cycles, and the caller saves a handful of times a
//! day.

use embedded_storage::{ReadStorage, Storage};
use esp_storage::{FlashStorage, FlashStorageError};

/// Where the ESP32-S3 bootloader expects the partition table.
///
/// Fixed by the bootloader, not by us: it reads this address to find out where
/// the application is, so nothing else can be placed here.
const PARTITION_TABLE_OFFSET: u32 = 0x8000;

/// The partition table is a plain array of these, one after another.
const PARTITION_ENTRY_SIZE: usize = 32;

/// How many entries the table can hold, which bounds the scan.
const MAX_PARTITION_ENTRIES: usize = 95;

/// First two bytes of a partition entry.
///
/// The table has no length field. It ends where this stops matching, which in
/// practice is the checksum entry the tooling appends.
const PARTITION_MAGIC: [u8; 2] = [0xAA, 0x50];

/// Offsets within a partition entry, as the bootloader lays it out.
const PARTITION_OFFSET_FIELD: usize = 4;
const PARTITION_SIZE_FIELD: usize = 8;
const PARTITION_LABEL_FIELD: usize = 12;
const PARTITION_LABEL_LENGTH: usize = 16;

/// First four bytes of a stored record, distinguishing it from erased flash.
///
/// An erased sector reads as all `0xFF`, and a sector that was never written
/// could hold anything, so a record has to identify itself.
const RECORD_MAGIC: [u8; 4] = *b"BSEC";

/// Format version of the record, so a later change can be recognised rather
/// than misread. A record written by a different version is discarded.
const RECORD_VERSION: u8 = 1;

/// Bytes of bookkeeping in front of the payload: magic, version, padding,
/// payload length and checksum.
const RECORD_HEADER_SIZE: usize = 12;

/// Largest payload this module will store.
///
/// The limit exists so a whole record fits in one stack buffer and can be
/// written in a single flash operation. A BSEC state is at most 255 bytes, so
/// there is room to spare.
pub const MAX_PAYLOAD_SIZE: usize = 512;

/// What can go wrong reaching the flash.
#[derive(Debug)]
pub enum Error {
    /// The flash itself refused a read, erase or write.
    Flash(FlashStorageError),
    /// No partition with the requested label exists. The device was flashed
    /// without the project's partition table.
    PartitionMissing,
    /// The partition is too small to hold the record, so saving would run past
    /// its end and into whatever follows.
    PayloadTooLarge { length: usize, capacity: usize },
}

impl From<FlashStorageError> for Error {
    fn from(error: FlashStorageError) -> Self {
        Error::Flash(error)
    }
}

/// A named region of flash holding one record.
pub struct FlashStore {
    flash: FlashStorage,
    /// Byte address of the partition in the flash.
    offset: u32,
    /// Length of the partition in bytes.
    size: u32,
}

impl FlashStore {
    /// Find the partition named `label` and open it.
    ///
    /// The address is looked up in the partition table rather than compiled in,
    /// because the two would otherwise have to be kept in step by hand. A stale
    /// constant would not fail cleanly: it would point somewhere else in the
    /// flash, and the first save would erase a sector of whatever lives there,
    /// which may well be the running firmware.
    pub fn open(label: &str) -> Result<Self, Error> {
        let mut flash = FlashStorage::new();
        let mut entry = [0u8; PARTITION_ENTRY_SIZE];

        for index in 0..MAX_PARTITION_ENTRIES {
            let entry_offset = PARTITION_TABLE_OFFSET + (index * PARTITION_ENTRY_SIZE) as u32;
            flash.read(entry_offset, &mut entry)?;

            if entry[..PARTITION_MAGIC.len()] != PARTITION_MAGIC {
                break;
            }

            if entry_label(&entry) == label.as_bytes() {
                return Ok(FlashStore {
                    offset: u32::from_le_bytes(
                        entry[PARTITION_OFFSET_FIELD..][..4].try_into().unwrap(),
                    ),
                    size: u32::from_le_bytes(
                        entry[PARTITION_SIZE_FIELD..][..4].try_into().unwrap(),
                    ),
                    flash,
                });
            }
        }

        Err(Error::PartitionMissing)
    }

    /// How many payload bytes fit in the partition.
    pub fn capacity(&self) -> usize {
        (self.size as usize)
            .saturating_sub(RECORD_HEADER_SIZE)
            .min(MAX_PAYLOAD_SIZE)
    }

    /// Read the stored record into `buffer`, returning the part of it that was
    /// filled.
    ///
    /// `Ok(None)` means there is nothing to restore, which covers every way
    /// that can happen and which the caller treats alike: the slot was never
    /// written, it was written by an older format, or a save was interrupted.
    /// None of those is an error; they are all just a first run.
    pub fn load<'a>(&mut self, buffer: &'a mut [u8]) -> Result<Option<&'a [u8]>, Error> {
        let mut header = [0u8; RECORD_HEADER_SIZE];
        self.flash.read(self.offset, &mut header)?;

        if header[..4] != RECORD_MAGIC || header[4] != RECORD_VERSION {
            return Ok(None);
        }

        let length = u16::from_le_bytes([header[6], header[7]]) as usize;
        let checksum = u32::from_le_bytes(header[8..12].try_into().unwrap());

        // A length past the end of the partition or of the caller's buffer
        // means the header is not what it claims to be, whatever the magic
        // says.
        if length > self.capacity() || length > buffer.len() {
            return Ok(None);
        }

        let payload = &mut buffer[..length];
        self.flash
            .read(self.offset + RECORD_HEADER_SIZE as u32, payload)?;

        if crc32(payload) != checksum {
            return Ok(None);
        }

        Ok(Some(payload))
    }

    /// Replace the stored record with `payload`.
    ///
    /// Header and payload go out as one write, so the sector is erased once and
    /// the record is never on the flash in a state the header contradicts. The
    /// erase and write take a few milliseconds, during which no other task or
    /// interrupt runs, so this belongs between measurements rather than in the
    /// middle of one.
    pub fn save(&mut self, payload: &[u8]) -> Result<(), Error> {
        if payload.len() > self.capacity() {
            return Err(Error::PayloadTooLarge {
                length: payload.len(),
                capacity: self.capacity(),
            });
        }

        let mut record = [0u8; RECORD_HEADER_SIZE + MAX_PAYLOAD_SIZE];
        record[..4].copy_from_slice(&RECORD_MAGIC);
        record[4] = RECORD_VERSION;
        record[6..8].copy_from_slice(&(payload.len() as u16).to_le_bytes());
        record[8..12].copy_from_slice(&crc32(payload).to_le_bytes());
        record[RECORD_HEADER_SIZE..][..payload.len()].copy_from_slice(payload);

        self.flash
            .write(self.offset, &record[..RECORD_HEADER_SIZE + payload.len()])?;

        Ok(())
    }
}

/// The label of a partition entry, without the padding that follows it.
fn entry_label(entry: &[u8; PARTITION_ENTRY_SIZE]) -> &[u8] {
    let label = &entry[PARTITION_LABEL_FIELD..][..PARTITION_LABEL_LENGTH];
    let end = label.iter().position(|&byte| byte == 0).unwrap_or(label.len());
    &label[..end]
}

/// CRC-32 as used by Ethernet, PNG and zip.
///
/// Computed a bit at a time. A table would be faster, but this runs over a few
/// hundred bytes a handful of times a day, and the table would cost a kilobyte
/// of flash to save time nothing is waiting on.
fn crc32(data: &[u8]) -> u32 {
    /// Reversed form of the standard polynomial, matching the bit order below.
    const POLYNOMIAL: u32 = 0xEDB8_8320;

    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let carry = crc & 1;
            crc >>= 1;
            if carry != 0 {
                crc ^= POLYNOMIAL;
            }
        }
    }
    !crc
}

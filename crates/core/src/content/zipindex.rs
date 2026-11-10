//! Minimal, budgeted zip reader (std + `flate2`).
//!
//! Only what a modpack import needs: read the central directory, list entries,
//! extract a stored/deflated entry, and **verify** one without keeping its bytes in
//! memory ([`verify_entry`] streams the payload through CRC-32 and SHA-256 under the
//! byte budget). Length and CRC are always checked when a payload is read, so a
//! truncated or corrupted override is rejected rather than silently accepted.

use std::io::Read;

use sha2::{Digest, Sha256};

use super::error::ContentError;
use super::paths::{MAX_ARCHIVE_ENTRIES, MAX_ENTRY_UNCOMPRESSED, MAX_TOTAL_UNCOMPRESSED};

const EOCD_SIGNATURE: u32 = 0x0605_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const ZIP64_MARKER: u32 = 0xffff_ffff;
const FLAG_ENCRYPTED: u16 = 0x0001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZipBudget {
    pub max_entries: usize,
    pub max_total_uncompressed: u64,
    pub max_entry_uncompressed: u64,
}

impl Default for ZipBudget {
    fn default() -> Self {
        Self {
            max_entries: MAX_ARCHIVE_ENTRIES,
            max_total_uncompressed: MAX_TOTAL_UNCOMPRESSED,
            max_entry_uncompressed: MAX_ENTRY_UNCOMPRESSED,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    pub name: String,
    /// 0 = stored, 8 = deflate.
    pub method: u16,
    pub crc32: u32,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub local_header_offset: u64,
}

impl ZipEntry {
    pub fn is_directory(&self) -> bool {
        self.name.ends_with('/')
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipIndex {
    pub entries: Vec<ZipEntry>,
    pub total_uncompressed: u64,
}

impl ZipIndex {
    pub fn find(&self, name: &str) -> Option<&ZipEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.name.as_str())
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn at(bytes: &'a [u8], offset: usize) -> Option<Self> {
        if offset > bytes.len() {
            return None;
        }
        Some(Self { bytes, offset })
    }

    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let end = self.offset.checked_add(length)?;
        let slice = self.bytes.get(self.offset..end)?;
        self.offset = end;
        Some(slice)
    }

    fn u16(&mut self) -> Option<u16> {
        let slice = self.take(2)?;
        Some(u16::from_le_bytes([slice[0], slice[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        let slice = self.take(4)?;
        Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }
}

fn unsupported(detail: impl Into<String>) -> ContentError {
    ContentError::UnsupportedArchive {
        detail: detail.into(),
    }
}

/// Locate the end-of-central-directory record and read every central entry.
pub fn read_index(bytes: &[u8], budget: &ZipBudget) -> Result<ZipIndex, ContentError> {
    let eocd_offset = find_eocd(bytes)?;
    let mut cursor =
        Cursor::at(bytes, eocd_offset).ok_or_else(|| unsupported("truncated archive"))?;
    cursor.u32().ok_or_else(|| unsupported("truncated eocd"))?;
    cursor.u16().ok_or_else(|| unsupported("truncated eocd"))?;
    cursor.u16().ok_or_else(|| unsupported("truncated eocd"))?;
    cursor.u16().ok_or_else(|| unsupported("truncated eocd"))?;
    let declared_entries = cursor.u16().ok_or_else(|| unsupported("truncated eocd"))? as u64;
    let directory_size = cursor.u32().ok_or_else(|| unsupported("truncated eocd"))? as u64;
    let directory_offset = cursor.u32().ok_or_else(|| unsupported("truncated eocd"))? as u64;

    if declared_entries == 0xffff || directory_offset == ZIP64_MARKER as u64 {
        return Err(unsupported("zip64 archives are not supported"));
    }
    if declared_entries as usize > budget.max_entries {
        return Err(ContentError::BudgetExceeded {
            budget: "archive_entries",
            limit: budget.max_entries as u64,
            observed: declared_entries,
        });
    }
    let directory_end = directory_offset
        .checked_add(directory_size)
        .ok_or_else(|| unsupported("central directory overflows"))?;
    if directory_end > bytes.len() as u64 {
        return Err(unsupported("central directory is outside the archive"));
    }

    let mut entries = Vec::with_capacity(declared_entries as usize);
    let mut total_uncompressed = 0u64;
    let mut offset = directory_offset as usize;
    for _ in 0..declared_entries {
        let mut header =
            Cursor::at(bytes, offset).ok_or_else(|| unsupported("truncated directory"))?;
        let signature = header
            .u32()
            .ok_or_else(|| unsupported("truncated directory"))?;
        if signature != CENTRAL_SIGNATURE {
            return Err(unsupported("bad central directory signature"));
        }
        header
            .take(4)
            .ok_or_else(|| unsupported("truncated directory"))?; // versions
        let flags = header
            .u16()
            .ok_or_else(|| unsupported("truncated directory"))?;
        let method = header
            .u16()
            .ok_or_else(|| unsupported("truncated directory"))?;
        header
            .take(4)
            .ok_or_else(|| unsupported("truncated directory"))?; // time/date
        let crc = header
            .u32()
            .ok_or_else(|| unsupported("truncated directory"))?;
        let compressed = header
            .u32()
            .ok_or_else(|| unsupported("truncated directory"))? as u64;
        let uncompressed = header
            .u32()
            .ok_or_else(|| unsupported("truncated directory"))? as u64;
        let name_length = header
            .u16()
            .ok_or_else(|| unsupported("truncated directory"))? as usize;
        let extra_length = header
            .u16()
            .ok_or_else(|| unsupported("truncated directory"))? as usize;
        let comment_length = header
            .u16()
            .ok_or_else(|| unsupported("truncated directory"))?
            as usize;
        header
            .take(8)
            .ok_or_else(|| unsupported("truncated directory"))?; // disk + attrs
        let local_offset = header
            .u32()
            .ok_or_else(|| unsupported("truncated directory"))? as u64;
        let name_bytes = header
            .take(name_length)
            .ok_or_else(|| unsupported("truncated entry name"))?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| unsupported("entry name is not utf-8"))?
            .to_string();

        if flags & FLAG_ENCRYPTED != 0 {
            return Err(unsupported(format!("entry {name} is encrypted")));
        }
        if compressed == ZIP64_MARKER as u64 || uncompressed == ZIP64_MARKER as u64 {
            return Err(unsupported(format!("entry {name} uses zip64 sizes")));
        }
        if !matches!(method, 0 | 8) {
            return Err(unsupported(format!(
                "entry {name} uses compression method {method}"
            )));
        }
        if uncompressed > budget.max_entry_uncompressed {
            return Err(ContentError::BudgetExceeded {
                budget: "entry_uncompressed_bytes",
                limit: budget.max_entry_uncompressed,
                observed: uncompressed,
            });
        }
        total_uncompressed = total_uncompressed.saturating_add(uncompressed);
        if total_uncompressed > budget.max_total_uncompressed {
            return Err(ContentError::BudgetExceeded {
                budget: "total_uncompressed_bytes",
                limit: budget.max_total_uncompressed,
                observed: total_uncompressed,
            });
        }

        entries.push(ZipEntry {
            name,
            method,
            crc32: crc,
            compressed_size: compressed,
            uncompressed_size: uncompressed,
            local_header_offset: local_offset,
        });

        offset = offset
            .checked_add(46 + name_length + extra_length + comment_length)
            .ok_or_else(|| unsupported("directory offset overflow"))?;
    }

    Ok(ZipIndex {
        entries,
        total_uncompressed,
    })
}

fn find_eocd(bytes: &[u8]) -> Result<usize, ContentError> {
    if bytes.len() < 22 {
        return Err(unsupported("archive is shorter than an eocd record"));
    }
    let lowest = bytes.len().saturating_sub(22 + 0xffff);
    let mut offset = bytes.len() - 22;
    loop {
        if let Some(window) = bytes.get(offset..offset + 4) {
            if u32::from_le_bytes([window[0], window[1], window[2], window[3]]) == EOCD_SIGNATURE {
                return Ok(offset);
            }
        }
        if offset == lowest {
            break;
        }
        offset -= 1;
    }
    Err(unsupported("no end-of-central-directory record"))
}

/// Locate the raw (still compressed) payload of one entry.
fn entry_payload<'a>(bytes: &'a [u8], entry: &ZipEntry) -> Result<&'a [u8], ContentError> {
    if !matches!(entry.method, 0 | 8) {
        return Err(unsupported(format!(
            "entry {} uses compression method {}",
            entry.name, entry.method
        )));
    }
    let offset = entry.local_header_offset as usize;
    let mut header =
        Cursor::at(bytes, offset).ok_or_else(|| unsupported("truncated local header"))?;
    let signature = header
        .u32()
        .ok_or_else(|| unsupported("truncated local header"))?;
    if signature != LOCAL_SIGNATURE {
        return Err(unsupported("bad local header signature"));
    }
    header
        .take(22)
        .ok_or_else(|| unsupported("truncated local header"))?;
    let name_length = header
        .u16()
        .ok_or_else(|| unsupported("truncated local header"))? as usize;
    let extra_length = header
        .u16()
        .ok_or_else(|| unsupported("truncated local header"))? as usize;
    let data_offset = offset + 30 + name_length + extra_length;
    let data_end = data_offset
        .checked_add(entry.compressed_size as usize)
        .ok_or_else(|| unsupported("entry data overflows"))?;
    bytes
        .get(data_offset..data_end)
        .ok_or_else(|| unsupported("entry data is outside the archive"))
}

fn check_entry_budget(entry: &ZipEntry, budget: &ZipBudget) -> Result<(), ContentError> {
    if entry.uncompressed_size > budget.max_entry_uncompressed {
        return Err(ContentError::BudgetExceeded {
            budget: "entry_uncompressed_bytes",
            limit: budget.max_entry_uncompressed,
            observed: entry.uncompressed_size,
        });
    }
    Ok(())
}

/// Walk the uncompressed payload of an entry in bounded chunks, feeding both CRC-32
/// and SHA-256, and verify the declared length and CRC.
///
/// `sink` receives the bytes only when the caller actually needs them, so a pack
/// reader can verify every override without holding the whole archive in memory.
fn walk_entry(
    bytes: &[u8],
    entry: &ZipEntry,
    budget: &ZipBudget,
    mut sink: Option<&mut Vec<u8>>,
) -> Result<EntryDigest, ContentError> {
    check_entry_budget(entry, budget)?;
    let payload = entry_payload(bytes, entry)?;

    let mut crc = 0xffff_ffffu32;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let chunk_limit = budget.max_entry_uncompressed + 1;
    let mut buffer = [0u8; 64 * 1024];

    let mut consume = |chunk: &[u8]| -> Result<(), ContentError> {
        total += chunk.len() as u64;
        if total > chunk_limit {
            return Err(ContentError::BudgetExceeded {
                budget: "entry_uncompressed_bytes",
                limit: budget.max_entry_uncompressed,
                observed: total,
            });
        }
        crc = crc32_update(crc, chunk);
        hasher.update(chunk);
        if let Some(output) = sink.as_deref_mut() {
            output.extend_from_slice(chunk);
        }
        Ok(())
    };

    if entry.method == 0 {
        if payload.len() as u64 != entry.uncompressed_size {
            return Err(ContentError::ChecksumMismatch {
                path: entry.name.clone(),
            });
        }
        consume(payload)?;
    } else {
        let mut decoder = flate2::read::DeflateDecoder::new(payload);
        loop {
            let read = decoder
                .read(&mut buffer)
                .map_err(|_| unsupported(format!("entry {} failed to inflate", entry.name)))?;
            if read == 0 {
                break;
            }
            consume(&buffer[..read])?;
        }
    }

    if total != entry.uncompressed_size {
        return Err(ContentError::ChecksumMismatch {
            path: entry.name.clone(),
        });
    }
    let crc = crc ^ 0xffff_ffff;
    if crc != entry.crc32 {
        return Err(ContentError::ChecksumMismatch {
            path: entry.name.clone(),
        });
    }

    Ok(EntryDigest {
        uncompressed: total,
        crc32: crc,
        sha256: hex_lower(&hasher.finalize()),
    })
}

/// Verified identity of one entry payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryDigest {
    pub uncompressed: u64,
    pub crc32: u32,
    /// Lowercase hex SHA-256 of the *uncompressed* payload.
    pub sha256: String,
}

/// Verify one entry without keeping its payload (CRC, length and SHA-256).
pub fn verify_entry(
    bytes: &[u8],
    entry: &ZipEntry,
    budget: &ZipBudget,
) -> Result<EntryDigest, ContentError> {
    walk_entry(bytes, entry, budget, None)
}

/// Extract one entry. Stored (0) and deflate (8) are supported; the payload is
/// verified (length + CRC) before it is returned.
pub fn read_entry(
    bytes: &[u8],
    entry: &ZipEntry,
    budget: &ZipBudget,
) -> Result<Vec<u8>, ContentError> {
    let mut out = Vec::new();
    walk_entry(bytes, entry, budget, Some(&mut out))?;
    Ok(out)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut rendered = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        rendered.push_str(&format!("{byte:02x}"));
    }
    rendered
}

fn crc32_update(crc: u32, bytes: &[u8]) -> u32 {
    let mut value = crc;
    for byte in bytes {
        let index = ((value ^ u32::from(*byte)) & 0xff) as usize;
        value = CRC_TABLE[index] ^ (value >> 8);
    }
    value
}

static CRC_TABLE: [u32; 256] = build_crc_table();

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 1 {
                0xedb8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

/// IEEE 802.3 CRC-32, used by zip for entry verification.
pub fn crc32(bytes: &[u8]) -> u32 {
    crc32_update(0xffff_ffff, bytes) ^ 0xffff_ffff
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the smallest possible stored zip so the reader is exercised against
    /// real bytes without a third-party writer.
    pub(crate) fn build_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut output: Vec<u8> = Vec::new();
        let mut central: Vec<u8> = Vec::new();
        for (name, data) in entries {
            let offset = output.len() as u32;
            let crc = crc32(data);
            output.extend_from_slice(&LOCAL_SIGNATURE.to_le_bytes());
            output.extend_from_slice(&20u16.to_le_bytes()); // version needed
            output.extend_from_slice(&0u16.to_le_bytes()); // flags
            output.extend_from_slice(&0u16.to_le_bytes()); // method stored
            output.extend_from_slice(&0u16.to_le_bytes()); // time
            output.extend_from_slice(&0u16.to_le_bytes()); // date
            output.extend_from_slice(&crc.to_le_bytes());
            output.extend_from_slice(&(data.len() as u32).to_le_bytes());
            output.extend_from_slice(&(data.len() as u32).to_le_bytes());
            output.extend_from_slice(&(name.len() as u16).to_le_bytes());
            output.extend_from_slice(&0u16.to_le_bytes()); // extra
            output.extend_from_slice(name.as_bytes());
            output.extend_from_slice(data);

            central.extend_from_slice(&CENTRAL_SIGNATURE.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes()); // version made by
            central.extend_from_slice(&20u16.to_le_bytes()); // version needed
            central.extend_from_slice(&0u16.to_le_bytes()); // flags
            central.extend_from_slice(&0u16.to_le_bytes()); // method
            central.extend_from_slice(&0u16.to_le_bytes()); // time
            central.extend_from_slice(&0u16.to_le_bytes()); // date
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let directory_offset = output.len() as u32;
        let directory_size = central.len() as u32;
        output.extend_from_slice(&central);
        output.extend_from_slice(&EOCD_SIGNATURE.to_le_bytes());
        output.extend_from_slice(&0u16.to_le_bytes()); // disk
        output.extend_from_slice(&0u16.to_le_bytes()); // directory disk
        output.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        output.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        output.extend_from_slice(&directory_size.to_le_bytes());
        output.extend_from_slice(&directory_offset.to_le_bytes());
        output.extend_from_slice(&0u16.to_le_bytes()); // comment length
        output
    }

    #[test]
    fn reads_central_directory_and_stored_entries() {
        let archive = build_stored_zip(&[
            ("modrinth.index.json", b"{\"formatVersion\":1}"),
            ("overrides/config/a.txt", b"hello"),
        ]);
        let index = read_index(&archive, &ZipBudget::default()).expect("index");
        assert_eq!(index.entries.len(), 2);
        let entry = index.find("overrides/config/a.txt").expect("entry");
        let data = read_entry(&archive, entry, &ZipBudget::default()).expect("read");
        assert_eq!(data, b"hello");
        assert!(index.find("missing").is_none());
    }

    #[test]
    fn rejects_entry_count_over_budget() {
        let archive = build_stored_zip(&[("a.txt", b"a"), ("b.txt", b"b")]);
        let budget = ZipBudget {
            max_entries: 1,
            ..ZipBudget::default()
        };
        assert!(matches!(
            read_index(&archive, &budget),
            Err(ContentError::BudgetExceeded { .. })
        ));
    }

    #[test]
    fn rejects_archives_without_an_eocd_record() {
        assert!(matches!(
            read_index(&[0u8; 8], &ZipBudget::default()),
            Err(ContentError::UnsupportedArchive { .. })
        ));
    }
}

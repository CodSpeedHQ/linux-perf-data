use linux_perf_event_reader::{Endianness, RawData};
use std::io::{Read, Seek};

use super::buffered_reader::BufferedReader;
use super::error::JitDumpError;
use super::header::JitDumpHeader;
use super::read_exact::ReadExactOrUntilEof;
use super::record::{JitDumpRawRecord, JitDumpRecordHeader, JitDumpRecordType};

/// Parses a jitdump file and allows iterating over records.
///
/// This reader works with complete jitdump files as well as with partial files
/// which are still being written to. This makes it useful in live-profiling
/// settings.
///
/// The records refer to memory owned by the reader, to minimize copies.
#[derive(Debug, Clone)]
pub struct JitDumpReader<R: Read> {
    reader: BufferedReader<R>,
    header: JitDumpHeader,
    endian: Endianness,
    /// None if the reader is currently pointing to the start of the header of the next entry.
    /// Some() if the reader is currently pointing after the header but before the body of an entry.
    pending_record_header: Option<JitDumpRecordHeader>,
    current_record_start_offset: u64,
    /// When set, `next_record_header` skips past byte ranges that don't look
    /// like valid record headers. Enable for jitdumps produced by writers
    /// whose userspace buffer can leak stray bytes (e.g. CPython under
    /// fork-heavy workloads).
    recover_from_corruption: bool,
}

impl<R: Read> JitDumpReader<R> {
    /// Create a new `JitDumpReader`. `JitDumpReader` does its own buffering so
    /// there is no need to wrap a [`File`](std::fs::File) into a `BufReader`.
    pub fn new(reader: R) -> Result<Self, JitDumpError> {
        Self::new_with_buffer_size(reader, 4 * 1024)
    }

    /// Create a new `JitDumpReader`, with a manually-specified buffer chunk size.
    pub fn new_with_buffer_size(mut reader: R, buffer_size: usize) -> Result<Self, JitDumpError> {
        let mut buf = vec![0; buffer_size];
        let first_data_len = reader
            .read_exact_or_until_eof(&mut buf)
            .map_err(JitDumpError::Io)?;

        let first_data = &buf[..first_data_len];
        let header = JitDumpHeader::parse(RawData::Single(first_data))?;
        let total_header_size = header.total_size;
        let endian = match &header.magic {
            b"DTiJ" => Endianness::LittleEndian,
            b"JiTD" => Endianness::BigEndian,
            _ => panic!(),
        };

        Ok(Self {
            reader: BufferedReader::new_with_partially_read_buffer(
                reader,
                buf,
                total_header_size as usize,
                first_data_len,
            ),
            header,
            endian,
            pending_record_header: None,
            current_record_start_offset: total_header_size as u64,
            recover_from_corruption: false,
        })
    }

    /// Enable best-effort resync when the next 16 bytes don't decode to a
    /// recognised record header (unknown `record_type` or a `total_size`
    /// smaller than the header itself). The reader will scan forward
    /// byte-by-byte for a header that looks valid AND whose claimed next
    /// header also looks valid — the chained check is what keeps random
    /// bytes from being mistaken for a record boundary. Off by default;
    /// callers that consume jitdumps from misbehaving writers (notably
    /// CPython's perf-trampoline, which can interleave stray bytes when
    /// `fork()` races with its buffered stream) should enable it.
    pub fn recover_from_corruption(mut self, enabled: bool) -> Self {
        self.recover_from_corruption = enabled;
        self
    }

    /// The file header.
    pub fn header(&self) -> &JitDumpHeader {
        &self.header
    }

    /// The file endian.
    pub fn endian(&self) -> Endianness {
        self.endian
    }

    /// Returns the header of the next record.
    pub fn next_record_header(&mut self) -> Result<Option<JitDumpRecordHeader>, std::io::Error> {
        if self.pending_record_header.is_some() {
            return Ok(self.pending_record_header.clone());
        }

        if self.recover_from_corruption {
            // Peek-and-validate: only commit to the 16 bytes if they decode
            // to a recognised record header. Otherwise, scan forward for a
            // valid one before consuming.
            if !self.peek_next_header_is_plausible()? {
                self.resync_to_next_valid_header()?;
            }
        }

        if let Some(record_header_bytes) = self.reader.consume_data(JitDumpRecordHeader::SIZE)? {
            self.pending_record_header =
                Some(JitDumpRecordHeader::parse(self.endian, record_header_bytes).unwrap());
        }
        Ok(self.pending_record_header.clone())
    }

    /// Returns `Ok(true)` if the next 16 bytes decode to a known record
    /// header AND the position they claim for the next header
    /// (`current + total_size`) also decodes to a known header. The chained
    /// check is what makes this useful for recovery: a single header check
    /// is too easy to satisfy with random bytes (any zero byte runs as a
    /// valid `JIT_CODE_LOAD` rid). Returns `Ok(true)` at EOF — the caller's
    /// `consume_data` will then return `Ok(None)` cleanly.
    fn peek_next_header_is_plausible(&mut self) -> Result<bool, std::io::Error> {
        self.peek_chain_valid_header_at(0)
    }

    /// Like `is_known_record_header` but operating on the stream at
    /// `byte_offset` from the current read position, and additionally
    /// requiring the next claimed header (at `byte_offset + total_size`) to
    /// also decode cleanly — or to land exactly at end of stream, which
    /// covers a clean trailing record on a partial file.
    fn peek_chain_valid_header_at(&mut self, byte_offset: usize) -> Result<bool, std::io::Error> {
        let endian = self.endian;
        let needed = byte_offset + JitDumpRecordHeader::SIZE;
        self.reader.ensure_available(needed)?;
        let window = match self.reader.peek_data(needed) {
            Some(w) => w,
            None => return Ok(true), // EOF mid-header — treat as clean end.
        };
        let window_bytes = window.as_slice();
        let candidate_bytes =
            RawData::Single(&window_bytes[byte_offset..byte_offset + JitDumpRecordHeader::SIZE]);
        if !is_known_record_header(endian, candidate_bytes) {
            return Ok(false);
        }
        let candidate = JitDumpRecordHeader::parse(
            endian,
            RawData::Single(&window_bytes[byte_offset..byte_offset + JitDumpRecordHeader::SIZE]),
        )
        .unwrap();
        let chained_start = byte_offset + candidate.total_size as usize;
        let chained_end = chained_start + JitDumpRecordHeader::SIZE;

        self.reader.ensure_available(chained_end)?;
        let extended = match self.reader.peek_data(chained_end) {
            Some(w) => w,
            None => {
                // The chain reaches past EOF. Accept iff the candidate's
                // claimed end lands exactly at EOF (clean trailing record).
                let avail = self.reader.ensure_available(chained_start)?;
                return Ok(avail == chained_start);
            }
        };
        let extended_bytes = extended.as_slice();
        let chained_bytes = RawData::Single(
            &extended_bytes[chained_start..chained_start + JitDumpRecordHeader::SIZE],
        );
        Ok(is_known_record_header(endian, chained_bytes))
    }

    /// Scan forward byte-by-byte for the next position that passes
    /// `peek_chain_valid_header_at`. Advances the reader to the resync point
    /// on success. On failure (no valid header before EOF), leaves the
    /// reader past EOF, so the next `consume_data` returns `Ok(None)`.
    fn resync_to_next_valid_header(&mut self) -> Result<(), std::io::Error> {
        loop {
            // The current position has already been rejected by the caller.
            // Step past it before re-checking.
            self.reader.advance(1);
            self.current_record_start_offset += 1;

            // Stop when we can no longer pull a full header — there's
            // nothing to anchor on.
            let avail = self.reader.ensure_available(JitDumpRecordHeader::SIZE)?;
            if avail < JitDumpRecordHeader::SIZE {
                return Ok(());
            }

            if self.peek_chain_valid_header_at(0)? {
                return Ok(());
            }
        }
    }

    /// Returns the timestamp of the next record.
    ///
    /// When operating on partial files, `None` means that not enough bytes for the header
    /// of the next record are available. `Some` means that we have enough bytes for the
    /// header but we may not have enough bytes to get the entire record.
    ///
    /// If `next_record_timestamp` returns `Ok(Some(...))`, the next call to `next_record()`
    /// can still return `None`!
    pub fn next_record_timestamp(&mut self) -> Result<Option<u64>, std::io::Error> {
        Ok(self.next_record_header()?.map(|r| r.timestamp))
    }

    /// Returns the record type of the next record.
    pub fn next_record_type(&mut self) -> Result<Option<JitDumpRecordType>, std::io::Error> {
        Ok(self.next_record_header()?.map(|r| r.record_type))
    }

    /// Returns the file offset at which the next record (specifically its record header) starts.
    pub fn next_record_offset(&self) -> u64 {
        self.current_record_start_offset
    }

    /// Returns the next record.
    ///
    /// When operating on partial files, this will return `Ok(None)` if the entire record is
    /// not available yet. Future calls to `next_record` may return `Ok(Some)` if the
    /// data has become available in the meantime, because they will call `read` on `R` again.
    pub fn next_record(&mut self) -> Result<Option<JitDumpRawRecord<'_>>, std::io::Error> {
        let record_size = match self.next_record_header()? {
            Some(header) => header.total_size,
            None => return Ok(None),
        };
        let body_size = record_size as usize - JitDumpRecordHeader::SIZE;

        match self.reader.consume_data(body_size)? {
            Some(record_body_data) => {
                let record_header = self.pending_record_header.take().unwrap();
                let start_offset = self.current_record_start_offset;
                self.current_record_start_offset += record_size as u64;
                Ok(Some(JitDumpRawRecord {
                    endian: self.endian,
                    start_offset,
                    record_size,
                    record_type: record_header.record_type,
                    timestamp: record_header.timestamp,
                    body: record_body_data,
                }))
            }
            None => Ok(None),
        }
    }
}

impl<R: Read + Seek> JitDumpReader<R> {
    /// Skip the upcoming record. If this returns true, the record has been skipped.
    /// If `false` is returned, it means the file could not be seeked far enough to
    /// skip the entire record (for example because this is a partial file which has
    /// not been fully written), and the next record remains unchanged from before the
    /// call to `skip_next_record`.
    ///
    /// You may want to call this if you've called `next_record_type` and have
    /// determined that you're not interested in the upcoming record. It saves having
    /// to read the full record into a contiguous slice of memory.
    pub fn skip_next_record(&mut self) -> Result<bool, std::io::Error> {
        let record_size = match self.next_record_header()? {
            Some(record_header) => record_header.total_size,
            None => return Ok(false),
        };
        let body_size = record_size as usize - JitDumpRecordHeader::SIZE; // TODO: Handle underflow

        self.reader.skip_bytes(body_size)?;
        self.pending_record_header.take();
        self.current_record_start_offset += record_size as u64;
        Ok(true)
    }
}

/// Decode the 16 bytes of `header_bytes` with the library's own header
/// parser and check that the result is internally consistent: known
/// `record_type` and a `total_size` of at least the header size itself.
/// Callers pass exactly `JitDumpRecordHeader::SIZE` bytes.
fn is_known_record_header(endian: Endianness, header_bytes: RawData<'_>) -> bool {
    let header = match JitDumpRecordHeader::parse(endian, header_bytes) {
        Ok(h) => h,
        Err(_) => return false,
    };
    let known_rid = matches!(
        header.record_type,
        JitDumpRecordType::JIT_CODE_LOAD
            | JitDumpRecordType::JIT_CODE_MOVE
            | JitDumpRecordType::JIT_CODE_DEBUG_INFO
            | JitDumpRecordType::JIT_CODE_CLOSE
            | JitDumpRecordType::JIT_CODE_UNWINDING_INFO
    );
    known_rid && (header.total_size as usize) >= JitDumpRecordHeader::SIZE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jitdump::JitDumpRecord;
    use std::io::Cursor;

    const HEADER_SIZE: usize = 40;

    fn make_header(le: bool) -> Vec<u8> {
        let mut h = Vec::with_capacity(HEADER_SIZE);
        if le {
            h.extend_from_slice(b"DTiJ");
        } else {
            h.extend_from_slice(b"JiTD");
        }
        let total_size = HEADER_SIZE as u32;
        if le {
            h.extend_from_slice(&1u32.to_le_bytes());
            h.extend_from_slice(&total_size.to_le_bytes());
            h.extend_from_slice(&62u32.to_le_bytes());
            h.extend_from_slice(&0u32.to_le_bytes());
            h.extend_from_slice(&12345u32.to_le_bytes());
            h.extend_from_slice(&0u64.to_le_bytes());
            h.extend_from_slice(&0u64.to_le_bytes());
        } else {
            h.extend_from_slice(&1u32.to_be_bytes());
            h.extend_from_slice(&total_size.to_be_bytes());
            h.extend_from_slice(&62u32.to_be_bytes());
            h.extend_from_slice(&0u32.to_be_bytes());
            h.extend_from_slice(&12345u32.to_be_bytes());
            h.extend_from_slice(&0u64.to_be_bytes());
            h.extend_from_slice(&0u64.to_be_bytes());
        }
        assert_eq!(h.len(), HEADER_SIZE);
        h
    }

    fn make_code_load(name: &str, vma: u64, code: &[u8], ts: u64) -> Vec<u8> {
        // body: pid(4) + tid(4) + vma(8) + code_addr(8) + code_size(8) + code_index(8)
        let body_fixed = 4 + 4 + 8 + 8 + 8 + 8;
        let body_len = body_fixed + name.len() + 1 + code.len();
        let total = JitDumpRecordHeader::SIZE + body_len;
        let mut r = Vec::with_capacity(total);
        r.extend_from_slice(&JitDumpRecordType::JIT_CODE_LOAD.0.to_le_bytes());
        r.extend_from_slice(&(total as u32).to_le_bytes());
        r.extend_from_slice(&ts.to_le_bytes());
        r.extend_from_slice(&12345u32.to_le_bytes());
        r.extend_from_slice(&12345u32.to_le_bytes());
        r.extend_from_slice(&vma.to_le_bytes());
        r.extend_from_slice(&vma.to_le_bytes());
        r.extend_from_slice(&(code.len() as u64).to_le_bytes());
        r.extend_from_slice(&0u64.to_le_bytes());
        r.extend_from_slice(name.as_bytes());
        r.push(0);
        r.extend_from_slice(code);
        assert_eq!(r.len(), total);
        r
    }

    fn collect_code_load_names<R: Read>(mut reader: JitDumpReader<R>) -> Vec<String> {
        let mut names = Vec::new();
        while let Some(raw) = reader.next_record().unwrap() {
            if let JitDumpRecord::CodeLoad(record) = raw.parse().unwrap() {
                names.push(String::from_utf8_lossy(&record.function_name.as_slice()).into_owned());
            }
        }
        names
    }

    #[test]
    fn recovers_past_inline_corruption() {
        // Mimics the CPython fork-flush race: two stray bytes get
        // interleaved between otherwise-valid records.
        let mut data = make_header(true);
        data.extend(make_code_load("first", 0x1000, &[0x90; 8], 100));
        data.extend_from_slice(&[0u8, 0u8]);
        data.extend(make_code_load("second", 0x2000, &[0x90; 8], 200));
        data.extend(make_code_load("third", 0x3000, &[0x90; 8], 300));

        let reader = JitDumpReader::new(Cursor::new(data))
            .unwrap()
            .recover_from_corruption(true);
        let names = collect_code_load_names(reader);
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn strict_mode_stops_at_corruption() {
        // Without recovery enabled, the reader should give up at the
        // corruption boundary (returns Ok(None) once a body can't be
        // consumed). We expect to see only the records before the gap.
        let mut data = make_header(true);
        data.extend(make_code_load("first", 0x1000, &[0x90; 8], 100));
        data.extend_from_slice(&[0u8, 0u8]);
        data.extend(make_code_load("second", 0x2000, &[0x90; 8], 200));

        let reader = JitDumpReader::new(Cursor::new(data)).unwrap();
        let names = collect_code_load_names(reader);
        assert_eq!(names, vec!["first"]);
    }

    #[test]
    fn clean_file_unaffected_by_recovery_flag() {
        let mut data = make_header(true);
        data.extend(make_code_load("first", 0x1000, &[0x90; 8], 100));
        data.extend(make_code_load("second", 0x2000, &[0x90; 8], 200));

        let reader = JitDumpReader::new(Cursor::new(data))
            .unwrap()
            .recover_from_corruption(true);
        let names = collect_code_load_names(reader);
        assert_eq!(names, vec!["first", "second"]);
    }
}

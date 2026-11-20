//! Parsing raw NTFS master file table records.
//!
//! `FSCTL_ENUM_USN_DATA` reports one name per file, so a file with several hard links shows
//! up in one directory only. The MFT itself keeps every name: each link is a `$FILE_NAME`
//! attribute of the file's record. This module reads them from the raw record bytes.
//!
//! Everything here is pure byte parsing and never panics on bad input. A record that
//! doesn't parse is an error the caller counts and skips. Reading the records off the
//! volume is [`crate::Volume::read_mft`].
//!
//! Layout references: the NTFS documentation of the Linux-NTFS project
//! (`FILE_RECORD_SEGMENT_HEADER`, `ATTRIBUTE_RECORD_HEADER`, `FILE_NAME`, `ATTRIBUTE_LIST_ENTRY`).

/// Attribute types mtseek reads.
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

/// Record header flag: the record describes a live file.
const RECORD_IN_USE: u16 = 0x0001;
/// `$FILE_NAME` namespace of an 8.3 alias. The file's real name is in another attribute.
const NAMESPACE_DOS: u8 = 2;
/// `$FILE_NAME` flag set on directories (`FILE_NAME_INDEX_PRESENT`).
const NAME_IS_DIRECTORY: u32 = 0x1000_0000;
/// Fixups protect each 512-byte stride of a record, whatever the sector size.
const FIXUP_STRIDE: usize = 512;
/// Mask for the record-number part of a file reference.
const RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// One name of one file: a file with two hard links yields two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName {
    /// The file's reference number (record number + sequence number of its base record).
    pub file_ref: u64,
    /// The reference number of the directory this name is in.
    pub parent_ref: u64,
    /// The name, with unpaired surrogates replaced by U+FFFD.
    pub name: String,
    /// Whether the file is a directory.
    pub is_dir: bool,
}

/// A run of clusters: virtual cluster `vcn` of the stream is logical cluster `lcn` of the
/// volume, for `clusters` clusters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Extent {
    /// First cluster within the stream.
    pub vcn: u64,
    /// First cluster on the volume.
    pub lcn: u64,
    /// Length in clusters.
    pub clusters: u64,
}

/// Why a record could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MftError {
    /// The update sequence (fixup) array is malformed, or a stride's check value doesn't
    /// match: the record was torn or is corrupt.
    #[error("record fails its update sequence check")]
    BadFixup,
    /// An attribute's length or offsets point outside the record.
    #[error("attribute at byte {offset} is malformed")]
    BadAttribute {
        /// Offset of the attribute in the record.
        offset: usize,
    },
    /// A non-resident attribute's run list is malformed.
    #[error("malformed run list")]
    BadRunList,
    /// Something mtseek does not read (e.g. a non-resident attribute list on `$MFT`).
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

/// Checks and undoes the update sequence (fixup) array of a raw record in place.
///
/// NTFS stamps the last two bytes of every 512-byte stride with a check value and keeps
/// the real bytes in an array in the header. A stride whose stamp doesn't match was not
/// written completely.
pub fn apply_fixups(record: &mut [u8]) -> Result<(), MftError> {
    if record.len() < 48 || &record[0..4] != b"FILE" {
        return Err(MftError::BadFixup);
    }
    let usa_offset = u16_at(record, 4) as usize;
    let usa_count = u16_at(record, 6) as usize;
    let strides = usa_count.checked_sub(1).ok_or(MftError::BadFixup)?;
    if strides == 0
        || strides * FIXUP_STRIDE > record.len()
        || usa_offset + usa_count * 2 > record.len()
    {
        return Err(MftError::BadFixup);
    }
    let check = [record[usa_offset], record[usa_offset + 1]];
    for i in 1..=strides {
        let end = i * FIXUP_STRIDE;
        if record[end - 2..end] != check {
            return Err(MftError::BadFixup);
        }
        let real = usa_offset + i * 2;
        record.copy_within(real..real + 2, end - 2);
    }
    Ok(())
}

/// Parses one raw record (fixups not yet applied) into `out`.
///
/// `number` is the record's position in the MFT. Free records and records that were never
/// used are skipped. Names from an extension record are attributed to its base record's
/// file, so they land under the right file reference. 8.3 aliases are skipped.
pub fn parse_record(
    record: &mut [u8],
    number: u64,
    out: &mut Vec<FileName>,
) -> Result<(), MftError> {
    if record.len() < 4 || &record[0..4] != b"FILE" {
        return Ok(()); // never used, or zeroed
    }
    apply_fixups(record)?;
    let header = Header::read(record);
    if header.flags & RECORD_IN_USE == 0 {
        return Ok(());
    }
    let file_ref = if header.base_ref & RECORD_NUMBER_MASK == 0 {
        (u64::from(header.sequence) << 48) | (number & RECORD_NUMBER_MASK)
    } else {
        header.base_ref
    };
    for attribute in Attributes::new(record, &header) {
        let attribute = attribute?;
        if attribute.kind != ATTR_FILE_NAME || attribute.non_resident {
            continue; // $FILE_NAME is always resident
        }
        let value = attribute.resident_value()?;
        if value.len() < 66 {
            return Err(MftError::BadAttribute {
                offset: attribute.offset,
            });
        }
        let name_chars = value[64] as usize;
        let namespace = value[65];
        let Some(name_bytes) = value.get(66..66 + name_chars * 2) else {
            return Err(MftError::BadAttribute {
                offset: attribute.offset,
            });
        };
        if namespace == NAMESPACE_DOS {
            continue;
        }
        let (pairs, _) = name_bytes.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|&p| u16::from_le_bytes(p)).collect();
        out.push(FileName {
            file_ref,
            parent_ref: u64_at(value, 0),
            name: String::from_utf16_lossy(&units),
            is_dir: u32_at(value, 56) & NAME_IS_DIRECTORY != 0,
        });
    }
    Ok(())
}

/// The extents of the unnamed `$DATA` stream described in a record with fixups applied.
/// For `$MFT`'s own record (number 0) that is where the whole MFT lives on the volume.
///
/// A stream whose description continues in other records lists only its part here; see
/// [`data_records_in_attribute_list`].
pub fn data_extents(record: &[u8]) -> Result<Vec<Extent>, MftError> {
    let header = Header::read(record);
    let mut extents = Vec::new();
    for attribute in Attributes::new(record, &header) {
        let attribute = attribute?;
        if attribute.kind == ATTR_DATA && attribute.non_resident && attribute.name_len == 0 {
            extents.extend(attribute.runs()?);
        }
    }
    Ok(extents)
}

/// Other records holding pieces of this record's unnamed `$DATA` stream, from its
/// `$ATTRIBUTE_LIST`. Empty when the stream is described entirely in this record.
///
/// A very fragmented `$MFT` needs this: its run list outgrows record 0.
pub fn data_records_in_attribute_list(record: &[u8], own: u64) -> Result<Vec<u64>, MftError> {
    let header = Header::read(record);
    let mut records = Vec::new();
    for attribute in Attributes::new(record, &header) {
        let attribute = attribute?;
        if attribute.kind != ATTR_ATTRIBUTE_LIST {
            continue;
        }
        if attribute.non_resident {
            return Err(MftError::Unsupported("non-resident attribute list"));
        }
        let list = attribute.resident_value()?;
        let mut at = 0;
        while at + 26 <= list.len() {
            let entry_len = u16_at(list, at + 4) as usize;
            if entry_len < 26 || at + entry_len > list.len() {
                return Err(MftError::BadAttribute {
                    offset: attribute.offset,
                });
            }
            let kind = u32_at(list, at);
            let name_len = list[at + 6];
            let record_number = u64_at(list, at + 16) & RECORD_NUMBER_MASK;
            if kind == ATTR_DATA
                && name_len == 0
                && record_number != own
                && !records.contains(&record_number)
            {
                records.push(record_number);
            }
            at += entry_len;
        }
    }
    Ok(records)
}

/// The fixed part of a record header that mtseek uses.
struct Header {
    sequence: u16,
    attrs_offset: usize,
    flags: u16,
    bytes_in_use: usize,
    base_ref: u64,
}

impl Header {
    /// Callers have checked the record holds at least 48 bytes (`apply_fixups`).
    fn read(record: &[u8]) -> Self {
        if record.len() < 48 {
            return Self {
                sequence: 0,
                attrs_offset: 0,
                flags: 0,
                bytes_in_use: 0,
                base_ref: 0,
            };
        }
        Self {
            sequence: u16_at(record, 0x10),
            attrs_offset: u16_at(record, 0x14) as usize,
            flags: u16_at(record, 0x16),
            bytes_in_use: (u32_at(record, 0x18) as usize).min(record.len()),
            base_ref: u64_at(record, 0x20),
        }
    }
}

/// One attribute, borrowed from its record.
struct Attribute<'a> {
    kind: u32,
    non_resident: bool,
    name_len: u8,
    /// The whole attribute, header included.
    bytes: &'a [u8],
    /// Its offset in the record, for errors.
    offset: usize,
}

impl<'a> Attribute<'a> {
    fn bad(&self) -> MftError {
        MftError::BadAttribute {
            offset: self.offset,
        }
    }

    fn resident_value(&self) -> Result<&'a [u8], MftError> {
        if self.bytes.len() < 24 {
            return Err(self.bad());
        }
        let len = u32_at(self.bytes, 16) as usize;
        let at = u16_at(self.bytes, 20) as usize;
        self.bytes
            .get(at..at.saturating_add(len))
            .ok_or_else(|| self.bad())
    }

    /// Decodes a non-resident attribute's run list. Sparse runs (no clusters on disk)
    /// are left out.
    fn runs(&self) -> Result<Vec<Extent>, MftError> {
        if self.bytes.len() < 64 {
            return Err(self.bad());
        }
        let mut vcn = u64_at(self.bytes, 16);
        let mut at = u16_at(self.bytes, 32) as usize;
        let mut lcn: i64 = 0;
        let mut extents = Vec::new();
        loop {
            let &header = self.bytes.get(at).ok_or(MftError::BadRunList)?;
            if header == 0 {
                return Ok(extents);
            }
            let len_size = (header & 0x0F) as usize;
            let off_size = (header >> 4) as usize;
            if !(1..=8).contains(&len_size) || off_size > 8 {
                return Err(MftError::BadRunList);
            }
            let fields = self
                .bytes
                .get(at + 1..at + 1 + len_size + off_size)
                .ok_or(MftError::BadRunList)?;
            let clusters = le_unsigned(&fields[..len_size]);
            if clusters == 0 {
                return Err(MftError::BadRunList);
            }
            if off_size > 0 {
                lcn = lcn
                    .checked_add(le_signed(&fields[len_size..]))
                    .filter(|l| *l >= 0)
                    .ok_or(MftError::BadRunList)?;
                extents.push(Extent {
                    vcn,
                    lcn: lcn.unsigned_abs(),
                    clusters,
                });
            }
            vcn = vcn.checked_add(clusters).ok_or(MftError::BadRunList)?;
            at += 1 + len_size + off_size;
        }
    }
}

/// Iterates a record's attributes. Stops after the first malformed one.
struct Attributes<'a> {
    record: &'a [u8],
    end: usize,
    at: usize,
    done: bool,
}

impl<'a> Attributes<'a> {
    fn new(record: &'a [u8], header: &Header) -> Self {
        Self {
            record,
            end: header.bytes_in_use,
            at: header.attrs_offset,
            done: false,
        }
    }
}

impl<'a> Iterator for Attributes<'a> {
    type Item = Result<Attribute<'a>, MftError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let at = self.at;
        let bad = MftError::BadAttribute { offset: at };
        let Some(head) = self.record.get(at..at + 4).filter(|_| at + 4 <= self.end) else {
            self.done = true;
            return Some(Err(bad));
        };
        let kind = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
        if kind == ATTR_END {
            self.done = true;
            return None;
        }
        let len = match self.record.get(at + 4..at + 8) {
            Some(l) => u32::from_le_bytes([l[0], l[1], l[2], l[3]]) as usize,
            None => 0,
        };
        // Attributes are 8-byte aligned and at least as long as the 16-byte common header.
        if len < 16 || len % 8 != 0 || at + len > self.end {
            self.done = true;
            return Some(Err(bad));
        }
        let bytes = &self.record[at..at + len];
        self.at = at + len;
        Some(Ok(Attribute {
            kind,
            non_resident: bytes[8] != 0,
            name_len: bytes[9],
            bytes,
            offset: at,
        }))
    }
}

fn le_unsigned(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .rev()
        .fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
}

fn le_signed(bytes: &[u8]) -> i64 {
    if bytes.is_empty() {
        return 0;
    }
    let mut buf = if bytes[bytes.len() - 1] & 0x80 != 0 {
        [0xFF; 8]
    } else {
        [0; 8]
    };
    buf[..bytes.len()].copy_from_slice(bytes);
    i64::from_le_bytes(buf)
}

// Callers check bounds before reading fixed fields.
fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut w = [0; 8];
    w.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(w)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use proptest::prelude::*;

    const SIZE: usize = 1024;

    /// Builds raw MFT records the way NTFS writes them, fixups included.
    pub(crate) struct RecordBuilder {
        sequence: u16,
        flags: u16,
        base_ref: u64,
        attributes: Vec<Vec<u8>>,
    }

    impl RecordBuilder {
        pub(crate) fn new(sequence: u16) -> Self {
            Self {
                sequence,
                flags: RECORD_IN_USE,
                base_ref: 0,
                attributes: Vec::new(),
            }
        }

        pub(crate) fn free(mut self) -> Self {
            self.flags = 0;
            self
        }

        pub(crate) fn extension_of(mut self, base_ref: u64) -> Self {
            self.base_ref = base_ref;
            self
        }

        pub(crate) fn name(mut self, parent: u64, name: &str, namespace: u8, dir: bool) -> Self {
            let units: Vec<u16> = name.encode_utf16().collect();
            let mut value = vec![0u8; 66];
            value[0..8].copy_from_slice(&parent.to_le_bytes());
            let flags = if dir { NAME_IS_DIRECTORY } else { 0x20 };
            value[56..60].copy_from_slice(&flags.to_le_bytes());
            value[64] = u8::try_from(units.len()).unwrap();
            value[65] = namespace;
            value.extend(units.iter().flat_map(|u| u.to_le_bytes()));
            self.attributes.push(resident(ATTR_FILE_NAME, &value));
            self
        }

        pub(crate) fn data_runs(mut self, start_vcn: u64, runs: &[u8]) -> Self {
            let mut a = vec![0u8; 64];
            a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
            a[8] = 1; // non-resident
            a[16..24].copy_from_slice(&start_vcn.to_le_bytes());
            a[32..34].copy_from_slice(&64u16.to_le_bytes());
            a.extend_from_slice(runs);
            a.push(0);
            pad(&mut a);
            let len = u32::try_from(a.len()).unwrap();
            a[4..8].copy_from_slice(&len.to_le_bytes());
            self.attributes.push(a);
            self
        }

        pub(crate) fn attribute_list(mut self, data_records: &[u64]) -> Self {
            let mut list = Vec::new();
            for &record in data_records {
                let mut e = vec![0u8; 32];
                e[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
                e[4..6].copy_from_slice(&32u16.to_le_bytes());
                e[16..24].copy_from_slice(&record.to_le_bytes());
                list.extend(e);
            }
            self.attributes.push(resident(ATTR_ATTRIBUTE_LIST, &list));
            self
        }

        /// The record as it sits on disk: fixups applied over the check value 0x0BAD.
        pub(crate) fn build(&self) -> Vec<u8> {
            let mut r = vec![0u8; SIZE];
            r[0..4].copy_from_slice(b"FILE");
            let usa_offset = 0x30u16;
            let usa_count = u16::try_from(SIZE / FIXUP_STRIDE + 1).unwrap();
            r[4..6].copy_from_slice(&usa_offset.to_le_bytes());
            r[6..8].copy_from_slice(&usa_count.to_le_bytes());
            r[0x10..0x12].copy_from_slice(&self.sequence.to_le_bytes());
            r[0x16..0x18].copy_from_slice(&self.flags.to_le_bytes());
            r[0x20..0x28].copy_from_slice(&self.base_ref.to_le_bytes());
            let attrs_offset = 0x38usize;
            r[0x14..0x16].copy_from_slice(&u16::try_from(attrs_offset).unwrap().to_le_bytes());
            let mut at = attrs_offset;
            for a in &self.attributes {
                r[at..at + a.len()].copy_from_slice(a);
                at += a.len();
            }
            r[at..at + 4].copy_from_slice(&ATTR_END.to_le_bytes());
            let in_use = u32::try_from(at + 8).unwrap();
            r[0x18..0x1C].copy_from_slice(&in_use.to_le_bytes());
            r[0x1C..0x20].copy_from_slice(&u32::try_from(SIZE).unwrap().to_le_bytes());
            // Save each stride's last two bytes in the array and stamp the check value.
            let check = 0x0BADu16.to_le_bytes();
            r[0x30..0x32].copy_from_slice(&check);
            for i in 1..=SIZE / FIXUP_STRIDE {
                let end = i * FIXUP_STRIDE;
                let slot = 0x30 + i * 2;
                let saved = [r[end - 2], r[end - 1]];
                r[slot..slot + 2].copy_from_slice(&saved);
                r[end - 2..end].copy_from_slice(&check);
            }
            r
        }
    }

    fn resident(kind: u32, value: &[u8]) -> Vec<u8> {
        let mut a = vec![0u8; 24];
        a[0..4].copy_from_slice(&kind.to_le_bytes());
        a[16..20].copy_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
        a[20..22].copy_from_slice(&24u16.to_le_bytes());
        a.extend_from_slice(value);
        pad(&mut a);
        let len = u32::try_from(a.len()).unwrap();
        a[4..8].copy_from_slice(&len.to_le_bytes());
        a
    }

    fn pad(a: &mut Vec<u8>) {
        while !a.len().is_multiple_of(8) {
            a.push(0);
        }
    }

    fn parse(record: &[u8], number: u64) -> Result<Vec<FileName>, MftError> {
        let mut r = record.to_vec();
        let mut out = Vec::new();
        parse_record(&mut r, number, &mut out).map(|()| out)
    }

    #[test]
    fn every_hard_link_and_no_dos_alias() {
        let r = RecordBuilder::new(3)
            .name(0x0005_0000_0000_0005, "LONGFI~1.TXT", NAMESPACE_DOS, false)
            .name(0x0005_0000_0000_0005, "long file name.txt", 1, false)
            .name(0x0002_0000_0000_0100, "second link.txt", 0, false)
            .build();
        let names = parse(&r, 0x2A).unwrap();
        let summary: Vec<_> = names
            .iter()
            .map(|n| (n.file_ref, n.parent_ref, n.name.as_str(), n.is_dir))
            .collect();
        let me = (3u64 << 48) | 0x2A;
        assert_eq!(
            summary,
            [
                (me, 0x0005_0000_0000_0005, "long file name.txt", false),
                (me, 0x0002_0000_0000_0100, "second link.txt", false),
            ]
        );
    }

    #[test]
    fn directories_and_extension_records() {
        let dir = RecordBuilder::new(1).name(5, "src", 3, true).build();
        assert!(parse(&dir, 7).unwrap()[0].is_dir);

        let base = 0x0004_0000_0000_0063;
        let ext = RecordBuilder::new(9)
            .extension_of(base)
            .name(5, "link 9.txt", 1, false)
            .build();
        assert_eq!(parse(&ext, 500).unwrap()[0].file_ref, base);
    }

    #[test]
    fn free_and_unused_records_yield_nothing() {
        let free = RecordBuilder::new(1)
            .free()
            .name(5, "deleted.txt", 1, false)
            .build();
        assert_eq!(parse(&free, 8), Ok(vec![]));
        assert_eq!(parse(&[0u8; SIZE], 9), Ok(vec![]));
    }

    #[test]
    fn a_torn_record_fails_its_fixup_check() {
        let mut r = RecordBuilder::new(1).name(5, "a.txt", 1, false).build();
        r[FIXUP_STRIDE - 1] ^= 0xFF; // first stride's stamp no longer matches
        assert_eq!(parse(&r, 1), Err(MftError::BadFixup));
    }

    #[test]
    fn decodes_run_lists() {
        // 0x31: 1-byte length, 3-byte offset. Then a run going backwards, then sparse.
        let runs = [
            0x31, 0x10, 0x00, 0x00, 0x01, // 16 clusters at LCN 0x10000
            0x21, 0x08, 0x00, 0xF0, // 8 clusters at LCN 0x10000 - 0x1000
            0x01, 0x04, // 4 sparse clusters
            0x11, 0x02, 0x10, // 2 clusters at LCN 0xF000 + 0x10
        ];
        let mut r = RecordBuilder::new(1).data_runs(0, &runs).build();
        apply_fixups(&mut r).unwrap();
        assert_eq!(
            data_extents(&r).unwrap(),
            [
                Extent {
                    vcn: 0,
                    lcn: 0x10000,
                    clusters: 16
                },
                Extent {
                    vcn: 16,
                    lcn: 0xF000,
                    clusters: 8
                },
                Extent {
                    vcn: 28,
                    lcn: 0xF010,
                    clusters: 2
                },
            ]
        );
    }

    #[test]
    fn finds_data_continued_in_other_records() {
        let mut r = RecordBuilder::new(1)
            .attribute_list(&[0, 17, 18, 17])
            .data_runs(0, &[0x11, 0x08, 0x20])
            .build();
        apply_fixups(&mut r).unwrap();
        assert_eq!(data_records_in_attribute_list(&r, 0).unwrap(), [17, 18]);
    }

    #[test]
    fn rejects_negative_clusters() {
        let mut r = RecordBuilder::new(1)
            .data_runs(0, &[0x11, 0x08, 0x80])
            .build();
        apply_fixups(&mut r).unwrap();
        assert_eq!(data_extents(&r), Err(MftError::BadRunList));
    }

    proptest! {
        #[test]
        fn roundtrips_names(
            // Up to 3 names of up to 30 chars (60 UTF-16 units) fit one 1 KiB record.
            names in proptest::collection::vec(("[^\\x00]{1,30}", 0u64..1 << 40, any::<bool>()), 1..4),
            seq in any::<u16>(),
            number in 0u64..1 << 40,
        ) {
            let mut b = RecordBuilder::new(seq);
            for (name, parent, dir) in &names {
                b = b.name(*parent, name, 1, *dir);
            }
            let record = b.build();
            let parsed = parse(&record, number).unwrap();
            let expected: Vec<_> = names
                .iter()
                .map(|(n, p, d)| FileName {
                    file_ref: (u64::from(seq) << 48) | number,
                    parent_ref: *p,
                    name: n.clone(),
                    is_dir: *d,
                })
                .collect();
            prop_assert_eq!(parsed, expected);
        }

        #[test]
        fn corrupt_records_never_panic(at in 0usize..SIZE, value in any::<u8>(), number in any::<u64>()) {
            let mut r = RecordBuilder::new(2)
                .attribute_list(&[3])
                .name(5, "file.txt", 1, false)
                .data_runs(0, &[0x31, 0x10, 0x00, 0x00, 0x01])
                .build();
            r[at] = value;
            let _ = parse(&r, number);
            if apply_fixups(&mut r).is_ok() {
                let _ = data_extents(&r);
                let _ = data_records_in_attribute_list(&r, 0);
            }
        }

        #[test]
        fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..SIZE)) {
            let mut b = bytes.clone();
            let _ = parse_record(&mut b, 0, &mut Vec::new());
            let mut b = bytes;
            if b.len() >= 4 { b[0..4].copy_from_slice(b"FILE"); }
            let _ = parse_record(&mut b, 0, &mut Vec::new());
            let _ = data_extents(&b);
        }
    }
}

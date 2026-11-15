//! Matroska (EBML) and AVI helpers for byte-range seeking: find Cluster boundaries, cache the
//! file header, read the Cues seek index.

/// First position of `needle` in `hay` at or after `from`.
pub fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() || hay.len() - from < needle.len() {
        return None;
    }
    let first = needle[0];
    let last = hay.len() - needle.len();
    let mut i = from;
    while i <= last {
        if hay[i] == first && &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// EBML variable-length integer at `pos`: (value, length).
pub fn read_ebml_vint(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    if pos >= buf.len() {
        return None;
    }
    let first = buf[pos];
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if pos + len > buf.len() {
        return None;
    }
    let mask: u8 = 0x80 >> (len - 1);
    let mut value = (first & (mask.wrapping_sub(1))) as u64;
    for i in 1..len {
        value = value * 256 + buf[pos + i] as u64;
    }
    Some((value, len))
}

/// EBML element ID (1-4 bytes) at `pos`: (id, length).
pub fn read_ebml_id(buf: &[u8], pos: usize) -> Option<(u32, usize)> {
    if pos >= buf.len() {
        return None;
    }
    let first = buf[pos];
    let len = if first & 0x80 != 0 {
        1
    } else if first & 0x40 != 0 {
        2
    } else if first & 0x20 != 0 {
        3
    } else if first & 0x10 != 0 {
        4
    } else {
        return None;
    };
    if pos + len > buf.len() {
        return None;
    }
    let mut id = 0u32;
    for i in 0..len {
        id = id * 256 + buf[pos + i] as u32;
    }
    Some((id, len))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cluster {
    pub offset: usize,
    /// -1 when the timestamp could not be read.
    pub timestamp_ms: i64,
}

const CLUSTER_ID: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];
const SEGMENT_ID: [u8; 4] = [0x18, 0x53, 0x80, 0x67];
const SEEKHEAD_ID: [u8; 4] = [0x11, 0x4D, 0x9B, 0x74];
pub const CUES_ID: [u8; 4] = [0x1C, 0x53, 0xBB, 0x6B];

/// Bytes re-scanned at the tail of a growing buffer so a marker that straddles a chunk boundary
/// is never missed.
pub const SCAN_OVERLAP: usize = 24;

/// First structurally valid Cluster at or after `start`: Cluster ID, a sane size, and a
/// Timecode (0xE7) as first child (an optional CRC-32 before it is skipped).
pub fn find_mkv_cluster(buf: &[u8], start: usize) -> Option<Cluster> {
    let mut pos = start;
    while pos + 12 <= buf.len() {
        let idx = find(buf, &CLUSTER_ID, pos)?;
        if idx + 12 > buf.len() {
            return None;
        }
        let (size, size_len) = match read_ebml_vint(buf, idx + 4) {
            Some(v) => v,
            None => {
                pos = idx + 1;
                continue;
            }
        };
        // 0 < size < 128 MB, or the reserved "unknown length" value (all value bits set).
        let is_unknown = size + 1 == (1u64 << (7 * size_len));
        if !is_unknown && (size == 0 || size > 128 * 1024 * 1024) {
            pos = idx + 1;
            continue;
        }
        let mut child = idx + 4 + size_len;
        if child < buf.len() && buf[child] == 0xBF {
            match read_ebml_vint(buf, child + 1) {
                Some((crc_size, crc_len)) => child = child.saturating_add(1 + crc_len).saturating_add(crc_size as usize),
                None => {
                    pos = idx + 1;
                    continue;
                }
            }
        }
        if child >= buf.len() || buf[child] != 0xE7 {
            pos = idx + 1;
            continue;
        }
        let (tc_len_val, tc_size_len) = match read_ebml_vint(buf, child + 1) {
            Some(v) => v,
            None => return Some(Cluster { offset: idx, timestamp_ms: -1 }),
        };
        let ts_start = child + 1 + tc_size_len;
        let ts_len = tc_len_val as usize;
        if ts_len > 8 || ts_start + ts_len > buf.len() {
            return Some(Cluster { offset: idx, timestamp_ms: -1 });
        }
        let mut ts: u64 = 0;
        for i in 0..ts_len {
            ts = ts * 256 + buf[ts_start + i] as u64;
        }
        return Some(Cluster { offset: idx, timestamp_ms: ts as i64 });
    }
    None
}

/// Rewrites the Segment element's size to "unknown length" so ffmpeg accepts piped data that
/// runs past the size the file declared.
pub fn patch_mkv_segment_length(buf: &[u8]) -> Vec<u8> {
    let mut out = buf.to_vec();
    let idx = match find(buf, &SEGMENT_ID, 0) {
        Some(i) => i,
        None => return out,
    };
    if idx + 12 >= buf.len() {
        return out;
    }
    let first = out[idx + 4];
    let vint_len = first.leading_zeros() as usize + 1;
    if vint_len > 8 || idx + 4 + vint_len >= buf.len() {
        return out;
    }
    out[idx + 4] = ((1u16 << (9 - vint_len)) - 1) as u8;
    for i in 1..vint_len {
        out[idx + 4 + i] = 0xFF;
    }
    out
}

/// Offset where the data of an AVI's 'LIST movi' starts (right after the 'movi' tag).
pub fn find_avi_movi_offset(buf: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    while pos + 12 <= buf.len() {
        let idx = find(buf, b"movi", pos)?;
        if idx >= 8 && &buf[idx - 8..idx - 4] == b"LIST" {
            return Some(idx + 4);
        }
        pos = idx + 1;
    }
    None
}

/// Where the Segment's data starts (the base of SeekHead and Cues positions).
pub fn segment_data_start(header: &[u8]) -> Option<usize> {
    let seg = find(header, &SEGMENT_ID, 0)?;
    let (_, len) = read_ebml_vint(header, seg + 4)?;
    Some(seg + 4 + len)
}

fn read_uint(buf: &[u8], start: usize, size: u64) -> u64 {
    let mut v: u64 = 0;
    let mut i = 0usize;
    while (i as u64) < size && start + i < buf.len() {
        v = v.wrapping_mul(256).wrapping_add(buf[start + i] as u64);
        i += 1;
    }
    v
}

/// Absolute byte offset of the Cues element, from the SeekHead in the file header.
pub fn parse_mkv_seek_head(buf: &[u8]) -> Option<u64> {
    const SEEK_ID: u32 = 0x4DBB;
    const SEEKID_ID: u32 = 0x53AB;
    const SEEKPOS_ID: u32 = 0x53AC;

    let seg_data_start = segment_data_start(buf)?;
    let sh = find(buf, &SEEKHEAD_ID, 0)?;
    let (sh_size, sh_len) = read_ebml_vint(buf, sh + 4)?;
    let sh_data_start = sh + 4 + sh_len;
    let sh_data_end = (sh_data_start as u64).saturating_add(sh_size).min(buf.len() as u64) as usize;

    let mut pos = sh_data_start;
    while pos + 4 < sh_data_end {
        let (id, id_len) = match read_ebml_id(buf, pos) {
            Some(v) => v,
            None => {
                pos += 1;
                continue;
            }
        };
        let (size, size_len) = match read_ebml_vint(buf, pos + id_len) {
            Some(v) => v,
            None => {
                pos += 1;
                continue;
            }
        };
        let data_start = pos + id_len + size_len;
        let data_end = (data_start as u64).saturating_add(size);
        if id == SEEK_ID && data_end <= sh_data_end as u64 {
            let data_end = data_end as usize;
            let mut seek_id: Option<Vec<u8>> = None;
            let mut seek_pos: Option<u64> = None;
            let mut sp = data_start;
            while sp + 2 < data_end {
                let (cid, cid_len) = match read_ebml_id(buf, sp) {
                    Some(v) => v,
                    None => {
                        sp += 1;
                        continue;
                    }
                };
                let (csize, csize_len) = match read_ebml_vint(buf, sp + cid_len) {
                    Some(v) => v,
                    None => {
                        sp += 1;
                        continue;
                    }
                };
                let cstart = sp + cid_len + csize_len;
                if cid == SEEKID_ID {
                    let n = (csize as usize).min(buf.len().saturating_sub(cstart));
                    seek_id = Some(buf[cstart..cstart + n].to_vec());
                } else if cid == SEEKPOS_ID {
                    seek_pos = Some(read_uint(buf, cstart, csize));
                }
                sp = match (cstart as u64).checked_add(csize) {
                    Some(v) if v <= usize::MAX as u64 => v as usize,
                    _ => break,
                };
            }
            if let (Some(idb), Some(p)) = (seek_id, seek_pos) {
                if idb == CUES_ID {
                    return Some(seg_data_start as u64 + p);
                }
            }
        }
        if data_end > buf.len() as u64 + 16 {
            break;
        }
        pos = data_end as usize;
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CueEntry {
    pub time_ms: u64,
    pub byte_offset: u64,
}

/// Cues element (starting at buf[0]) → time/offset table. Offsets are made absolute.
pub fn parse_mkv_cues(buf: &[u8], seg_data_start: usize) -> Vec<CueEntry> {
    let mut entries = Vec::new();
    let (cid, cid_len) = match read_ebml_id(buf, 0) {
        Some(v) => v,
        None => return entries,
    };
    if cid != 0x1C53BB6B {
        return entries;
    }
    let (cues_size, cues_size_len) = match read_ebml_vint(buf, cid_len) {
        Some(v) => v,
        None => return entries,
    };
    let mut pos = cid_len + cues_size_len;
    let cues_end = (pos as u64).saturating_add(cues_size).min(buf.len() as u64) as usize;

    while pos + 4 < cues_end {
        let (id, id_len) = match read_ebml_id(buf, pos) {
            Some(v) => v,
            None => {
                pos += 1;
                continue;
            }
        };
        let (size, size_len) = match read_ebml_vint(buf, pos + id_len) {
            Some(v) => v,
            None => {
                pos += 1;
                continue;
            }
        };
        let data_start = pos + id_len + size_len;
        let data_end = (data_start as u64).saturating_add(size).min(buf.len() as u64) as usize;

        if id == 0xBB {
            // CuePoint
            let mut cue_time: Option<u64> = None;
            let mut cue_pos: Option<u64> = None;
            let mut cp = data_start;
            while cp + 2 < data_end {
                let (c_id, c_id_len) = match read_ebml_id(buf, cp) {
                    Some(v) => v,
                    None => {
                        cp += 1;
                        continue;
                    }
                };
                let (c_size, c_size_len) = match read_ebml_vint(buf, cp + c_id_len) {
                    Some(v) => v,
                    None => {
                        cp += 1;
                        continue;
                    }
                };
                let c_start = cp + c_id_len + c_size_len;
                let c_end = (c_start as u64).saturating_add(c_size).min(buf.len() as u64 + 1) as usize;
                if c_id == 0xB3 {
                    cue_time = Some(read_uint(buf, c_start, c_size));
                } else if c_id == 0xB7 {
                    // CueTrackPositions
                    let mut tp = c_start;
                    while tp + 2 < c_end {
                        let (t_id, t_id_len) = match read_ebml_id(buf, tp) {
                            Some(v) => v,
                            None => {
                                tp += 1;
                                continue;
                            }
                        };
                        let (t_size, t_size_len) = match read_ebml_vint(buf, tp + t_id_len) {
                            Some(v) => v,
                            None => {
                                tp += 1;
                                continue;
                            }
                        };
                        let t_start = tp + t_id_len + t_size_len;
                        if t_id == 0xF1 {
                            cue_pos = Some(read_uint(buf, t_start, t_size));
                        }
                        tp = (t_start as u64).saturating_add(t_size).min(buf.len() as u64 + 1) as usize;
                    }
                }
                cp = c_end;
            }
            if let (Some(t), Some(p)) = (cue_time, cue_pos) {
                entries.push(CueEntry { time_ms: t, byte_offset: seg_data_start as u64 + p });
            }
        }
        if data_end <= pos {
            break;
        }
        pos = data_end;
    }
    entries
}

/// Closest entry at or before `seek_ms` in a table sorted by time.
pub fn cues_lookup(table: &[CueEntry], seek_ms: u64) -> Option<CueEntry> {
    if table.is_empty() {
        return None;
    }
    let (mut lo, mut hi) = (0isize, table.len() as isize - 1);
    let mut best = None;
    while lo <= hi {
        let mid = ((lo + hi) / 2) as usize;
        if table[mid].time_ms <= seek_ms {
            best = Some(table[mid]);
            lo = mid as isize + 1;
        } else {
            hi = mid as isize - 1;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(ts: u8, payload: usize) -> Vec<u8> {
        // Cluster id, 1-byte size, Timecode (E7) size 1, value, then filler
        let mut v = vec![0x1F, 0x43, 0xB6, 0x75, 0x80 | (3 + payload as u8), 0xE7, 0x81, ts];
        v.extend(std::iter::repeat(0xAA).take(payload));
        v
    }

    #[test]
    fn vints() {
        assert_eq!(read_ebml_vint(&[0x81], 0), Some((1, 1)));
        assert_eq!(read_ebml_vint(&[0x40, 0x02], 0), Some((2, 2)));
        assert_eq!(read_ebml_vint(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], 0), Some(((1u64 << 56) - 1, 8)));
        assert_eq!(read_ebml_vint(&[0x00], 0), None);
        assert_eq!(read_ebml_vint(&[0x40], 0), None);
        assert_eq!(read_ebml_id(&[0x1A, 0x45, 0xDF, 0xA3], 0), Some((0x1A45DFA3, 4)));
        assert_eq!(read_ebml_id(&[0xE7], 0), Some((0xE7, 1)));
    }

    #[test]
    fn finds_a_valid_cluster_and_skips_fakes() {
        let mut buf = vec![0u8; 20];
        // a fake Cluster id followed by garbage (no Timecode child)
        buf.extend([0x1F, 0x43, 0xB6, 0x75, 0x85, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        let real_at = buf.len();
        buf.extend(cluster(42, 16));
        let c = find_mkv_cluster(&buf, 0).unwrap();
        assert_eq!(c, Cluster { offset: real_at, timestamp_ms: 42 });
        // scanning from the overlap region still finds it
        assert_eq!(find_mkv_cluster(&buf, real_at.saturating_sub(SCAN_OVERLAP)).unwrap().offset, real_at);
        assert_eq!(find_mkv_cluster(&buf[..real_at + 6], 0), None);
    }

    #[test]
    fn cluster_with_crc_and_unknown_length() {
        // unknown-length cluster (8-byte size, all ones), CRC-32 first, then Timecode 2 bytes
        let mut buf = vec![0x1F, 0x43, 0xB6, 0x75, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        buf.extend([0xBF, 0x84, 1, 2, 3, 4]);
        buf.extend([0xE7, 0x82, 0x01, 0x00]);
        buf.extend([0u8; 8]);
        let c = find_mkv_cluster(&buf, 0).unwrap();
        assert_eq!(c.offset, 0);
        assert_eq!(c.timestamp_ms, 256);
    }

    #[test]
    fn segment_length_is_patched_to_unknown() {
        let mut buf = vec![0x1A, 0x45, 0xDF, 0xA3, 0x80];
        buf.extend([0x18, 0x53, 0x80, 0x67, 0x01, 0, 0, 0, 0, 0x10, 0, 0]);
        buf.extend([0u8; 16]);
        let p = patch_mkv_segment_length(&buf);
        assert_eq!(&p[9..17], &[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(p.len(), buf.len());
        // 4-byte size form
        let mut b2 = vec![0x18, 0x53, 0x80, 0x67, 0x10, 0x00, 0x20, 0x00];
        b2.extend([0u8; 16]);
        let p2 = patch_mkv_segment_length(&b2);
        assert_eq!(&p2[4..8], &[0x1F, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn avi_movi() {
        let mut buf = b"RIFF\0\0\0\0AVI LIST\x10\0\0\0hdrlmovi-not-a-list".to_vec();
        let at = buf.len();
        buf.extend(b"LIST\x40\0\0\0movi00dc........");
        assert_eq!(find_avi_movi_offset(&buf, 0), Some(at + 12));
        assert_eq!(find_avi_movi_offset(&buf, at.saturating_sub(SCAN_OVERLAP)), Some(at + 12));
        assert_eq!(find_avi_movi_offset(b"RIFF....AVI ....", 0), None);
    }

    #[test]
    fn seek_head_and_cues() {
        // Segment (unknown size) → SeekHead { Seek { SeekID=Cues, SeekPosition=0x1234 } }
        let mut buf = vec![0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let seg_start = buf.len();
        let seek: Vec<u8> = vec![0x53, 0xAB, 0x84, 0x1C, 0x53, 0xBB, 0x6B, 0x53, 0xAC, 0x82, 0x12, 0x34];
        let mut seek_el = vec![0x4D, 0xBB, 0x80 | seek.len() as u8];
        seek_el.extend(&seek);
        buf.extend([0x11, 0x4D, 0x9B, 0x74, 0x80 | seek_el.len() as u8]);
        buf.extend(&seek_el);
        buf.extend([0u8; 8]);
        assert_eq!(segment_data_start(&buf), Some(seg_start));
        assert_eq!(parse_mkv_seek_head(&buf), Some(seg_start as u64 + 0x1234));

        // Cues { CuePoint { CueTime=5000, CueTrackPositions { CueTrack=1, CueClusterPosition=0x010203 } } x2 }
        let point = |t: u16, p: [u8; 3]| -> Vec<u8> {
            let tp = vec![0xF7, 0x81, 0x01, 0xF1, 0x83, p[0], p[1], p[2]];
            let mut body = vec![0xB3, 0x82, (t >> 8) as u8, t as u8, 0xB7, 0x80 | tp.len() as u8];
            body.extend(tp);
            let mut el = vec![0xBB, 0x80 | body.len() as u8];
            el.extend(body);
            el
        };
        let mut body = point(5000, [1, 2, 3]);
        body.extend(point(10000, [4, 5, 6]));
        let mut cues = vec![0x1C, 0x53, 0xBB, 0x6B, 0x80 | body.len() as u8];
        cues.extend(body);
        cues.extend([0u8; 4]);
        let table = parse_mkv_cues(&cues, 100);
        assert_eq!(table, vec![CueEntry { time_ms: 5000, byte_offset: 100 + 0x010203 }, CueEntry { time_ms: 10000, byte_offset: 100 + 0x040506 }]);
        assert_eq!(cues_lookup(&table, 4999), None);
        assert_eq!(cues_lookup(&table, 5000).unwrap().time_ms, 5000);
        assert_eq!(cues_lookup(&table, 9999).unwrap().time_ms, 5000);
        assert_eq!(cues_lookup(&table, 99999).unwrap().time_ms, 10000);
    }
}

//! Fragmented MP4 stream splitter: ffmpeg writes ftyp+moov+[moof+mdat]* to a pipe, this turns it
//! into an init segment and media segments of about the target duration.

fn be32(buf: &[u8], pos: usize) -> Option<u32> {
    let b = buf.get(pos..pos + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn fourcc(buf: &[u8], pos: usize) -> Option<&[u8]> {
    buf.get(pos..pos + 4)
}

/// baseMediaDecodeTime of the first moof→traf→tfdt, in seconds.
pub fn extract_tfdt(buf: &[u8], timescale: u32) -> Option<f64> {
    if buf.len() < 16 || timescale == 0 {
        return None;
    }
    let mut pos = 0usize;
    while pos + 8 < buf.len() {
        let size = be32(buf, pos)? as usize;
        let typ = fourcc(buf, pos + 4)?;
        if size < 8 || pos + size > buf.len() {
            break;
        }
        if typ == b"moof" {
            let mut mpos = pos + 8;
            while mpos + 8 < pos + size {
                let msize = be32(buf, mpos)? as usize;
                let mtype = fourcc(buf, mpos + 4)?;
                if msize < 8 || mpos + msize > pos + size {
                    break;
                }
                if mtype == b"traf" {
                    let mut tpos = mpos + 8;
                    while tpos + 8 < mpos + msize {
                        let tsize = be32(buf, tpos)? as usize;
                        let ttype = fourcc(buf, tpos + 4)?;
                        if tsize < 8 || tpos + tsize > mpos + msize {
                            break;
                        }
                        if ttype == b"tfdt" {
                            let version = *buf.get(tpos + 8)?;
                            let base: f64 = if version == 1 && tsize >= 20 {
                                be32(buf, tpos + 12)? as f64 * 4294967296.0 + be32(buf, tpos + 16)? as f64
                            } else if tsize >= 16 {
                                be32(buf, tpos + 12)? as f64
                            } else {
                                return None;
                            };
                            return Some(base / timescale as f64);
                        }
                        tpos += tsize;
                    }
                }
                mpos += msize;
            }
            return None; // a moof without tfdt
        }
        pos += size;
    }
    None
}

#[derive(Debug, PartialEq)]
pub enum Event {
    /// ftyp + moov.
    Init(Vec<u8>),
    /// One or more moof+mdat pairs, and their duration in seconds.
    Segment(Vec<u8>, f64),
}

pub struct Fmp4Parser {
    seg_duration: f64,
    buf: Vec<u8>,
    init_parts: Vec<u8>,
    init_emitted: bool,
    /// Boxes of the fragment being assembled (first one is the moof).
    seg_parts: Vec<Vec<u8>>,
    /// Completed fragments waiting to reach the target duration.
    seg_accum: Vec<Vec<u8>>,
    accum_duration: f64,
    in_segment: bool,
    pub timescale: u32,
}

impl Fmp4Parser {
    pub fn new(seg_duration: f64) -> Self {
        Fmp4Parser {
            seg_duration,
            buf: Vec::new(),
            init_parts: Vec::new(),
            init_emitted: false,
            seg_parts: Vec::new(),
            seg_accum: Vec::new(),
            accum_duration: 0.0,
            in_segment: false,
            timescale: 0,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        self.buf.extend_from_slice(chunk);
        let mut pos = 0usize;
        loop {
            let avail = self.buf.len() - pos;
            if avail < 8 {
                break;
            }
            let mut box_size = be32(&self.buf, pos).unwrap_or(0) as u64;
            if box_size == 1 {
                if avail >= 16 {
                    box_size = (be32(&self.buf, pos + 8).unwrap_or(0) as u64) << 32 | be32(&self.buf, pos + 12).unwrap_or(0) as u64;
                } else {
                    break; // need the extended header
                }
            }
            if box_size == 0 {
                box_size = avail as u64; // box runs to the end of the stream
            }
            if box_size < 8 || (avail as u64) < box_size {
                break; // need more data
            }
            let box_size = box_size as usize;
            let typ = [self.buf[pos + 4], self.buf[pos + 5], self.buf[pos + 6], self.buf[pos + 7]];
            let data = self.buf[pos..pos + box_size].to_vec();
            pos += box_size;

            match &typ {
                b"ftyp" | b"moov" => {
                    self.init_parts.extend_from_slice(&data);
                    if &typ == b"moov" && !self.init_emitted {
                        self.init_emitted = true;
                        let ts = extract_timescale(&data);
                        self.timescale = if ts > 0 { ts } else { 90000 };
                        events.push(Event::Init(self.init_parts.clone()));
                    }
                }
                b"moof" => {
                    if !self.seg_parts.is_empty() {
                        self.accum_fragment(&mut events);
                    }
                    self.in_segment = true;
                    self.seg_parts = vec![data];
                }
                _ => {
                    // mdat, and styp/sidx/free…: attached to the fragment being assembled
                    if self.in_segment {
                        self.seg_parts.push(data);
                    }
                }
            }
        }
        if pos > 0 {
            self.buf.drain(..pos);
        }
        events
    }

    /// End of stream: whatever is pending becomes the last segment.
    pub fn flush(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if !self.seg_parts.is_empty() {
            self.accum_fragment(&mut events);
        }
        self.emit_accumulated(&mut events);
        events
    }

    fn accum_fragment(&mut self, events: &mut Vec<Event>) {
        if self.seg_parts.is_empty() {
            return;
        }
        let frag_dur = self.extract_duration(&self.seg_parts[0]).unwrap_or(0.0);
        let parts = std::mem::take(&mut self.seg_parts);
        self.seg_accum.extend(parts);
        self.accum_duration += frag_dur;
        self.in_segment = false;
        if self.accum_duration >= self.seg_duration || (frag_dur == 0.0 && self.seg_accum.len() >= 2) {
            self.emit_accumulated(events);
        }
    }

    fn emit_accumulated(&mut self, events: &mut Vec<Event>) {
        if self.seg_accum.is_empty() {
            return;
        }
        let total: usize = self.seg_accum.iter().map(|p| p.len()).sum();
        let mut seg = Vec::with_capacity(total);
        for p in self.seg_accum.drain(..) {
            seg.extend_from_slice(&p);
        }
        let duration = if self.accum_duration > 0.0 { self.accum_duration } else { self.seg_duration };
        self.accum_duration = 0.0;
        events.push(Event::Segment(seg, duration));
    }

    /// Sum of the sample durations in the moof's first traf, in seconds.
    fn extract_duration(&self, moof: &[u8]) -> Option<f64> {
        let mut pos = 8usize;
        while pos + 8 < moof.len() {
            let size = be32(moof, pos)? as usize;
            let typ = fourcc(moof, pos + 4)?;
            if size < 8 {
                break;
            }
            if typ == b"traf" {
                return self.parse_traf(moof, pos + 8, pos + size);
            }
            pos += size;
        }
        None
    }

    fn parse_traf(&self, buf: &[u8], start: usize, end: usize) -> Option<f64> {
        let mut pos = start;
        let mut default_sample_duration: u64 = 0;
        let mut total: u64 = 0;
        let mut sample_count: u64 = 0;
        while pos + 8 < end {
            let size = be32(buf, pos)? as usize;
            let typ = fourcc(buf, pos + 4)?;
            if size < 8 {
                break;
            }
            if typ == b"tfhd" {
                let flags = be32(buf, pos + 8)? & 0xFF_FFFF;
                let mut off = pos + 12 + 4; // version+flags, track_id
                if flags & 0x01 != 0 {
                    off += 8; // base_data_offset
                }
                if flags & 0x02 != 0 {
                    off += 4; // sample_description_index
                }
                if flags & 0x08 != 0 {
                    default_sample_duration = be32(buf, off)? as u64;
                }
            }
            if typ == b"trun" {
                let flags = be32(buf, pos + 8)? & 0xFF_FFFF;
                sample_count = be32(buf, pos + 12)? as u64;
                let mut off = pos + 16;
                if flags & 0x01 != 0 {
                    off += 4; // data_offset
                }
                if flags & 0x04 != 0 {
                    off += 4; // first_sample_flags
                }
                let mut i = 0u64;
                while i < sample_count && off < pos + size {
                    let mut dur = default_sample_duration;
                    if flags & 0x100 != 0 {
                        dur = be32(buf, off)? as u64;
                        off += 4;
                    }
                    if flags & 0x200 != 0 {
                        off += 4; // sample_size
                    }
                    if flags & 0x400 != 0 {
                        off += 4; // sample_flags
                    }
                    if flags & 0x800 != 0 {
                        off += 4; // sample_composition_time_offset
                    }
                    total += dur;
                    i += 1;
                }
            }
            pos += size;
        }
        let ts = if self.timescale > 0 { self.timescale } else { 90000 } as f64;
        if total > 0 {
            return Some(total as f64 / ts);
        }
        if sample_count > 0 && default_sample_duration > 0 {
            return Some((sample_count * default_sample_duration) as f64 / ts);
        }
        None
    }
}

fn find_box(buf: &[u8], start: usize, end: usize, target: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = start;
    while pos + 8 < end {
        let size = be32(buf, pos)? as usize;
        let typ = fourcc(buf, pos + 4)?;
        if size < 8 || pos + size > end {
            break;
        }
        if typ == target {
            return Some((pos, pos + size));
        }
        pos += size;
    }
    None
}

/// Timescale from moov→trak→mdia→mdhd, the video track's when there is one. 0 when not found.
pub fn extract_timescale(moov: &[u8]) -> u32 {
    let mut pos = 8usize;
    let (mut video_ts, mut any_ts) = (0u32, 0u32);
    while pos + 8 < moov.len() {
        let size = match be32(moov, pos) {
            Some(s) => s as usize,
            None => break,
        };
        if size < 8 {
            break;
        }
        if fourcc(moov, pos + 4) == Some(b"trak") {
            let end = (pos + size).min(moov.len());
            if let Some((ms, me)) = find_box(moov, pos + 8, end, b"mdia") {
                if let Some((hs, _)) = find_box(moov, ms + 8, me, b"mdhd") {
                    let ver = moov.get(hs + 8).copied().unwrap_or(0);
                    let ts = if ver == 1 { be32(moov, hs + 28) } else { be32(moov, hs + 20) }.unwrap_or(0);
                    if ts > 0 {
                        if any_ts == 0 {
                            any_ts = ts;
                        }
                        if let Some((ds, _)) = find_box(moov, ms + 8, me, b"hdlr") {
                            if fourcc(moov, ds + 16) == Some(b"vide") {
                                video_ts = ts;
                            }
                        }
                    }
                }
            }
        }
        pos += size;
    }
    if video_ts > 0 {
        video_ts
    } else {
        any_ts
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((8 + body.len()) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(body);
        v
    }

    fn build_tfdt(version: u8, base: u64) -> Vec<u8> {
        let mut body = vec![version, 0, 0, 0];
        if version == 1 {
            body.extend((base as u64).to_be_bytes());
        } else {
            body.extend((base as u32).to_be_bytes());
        }
        bx(b"tfdt", &body)
    }

    fn build_moof(tfdt: &[u8]) -> Vec<u8> {
        bx(b"moof", &bx(b"traf", tfdt))
    }

    // ── Ported from transcode/test_multiaudio.js (tests 1-3: extractTfdt) ──

    #[test]
    fn tfdt_32bit_at_timescale_90000() {
        assert_eq!(extract_tfdt(&build_moof(&build_tfdt(0, 90000 * 10)), 90000), Some(10.0));
    }

    #[test]
    fn tfdt_32bit_at_timescale_48000() {
        assert_eq!(extract_tfdt(&build_moof(&build_tfdt(0, 48000 * 5)), 48000), Some(5.0));
    }

    #[test]
    fn tfdt_32bit_at_time_zero() {
        assert_eq!(extract_tfdt(&build_moof(&build_tfdt(0, 0)), 90000), Some(0.0));
    }

    #[test]
    fn tfdt_64bit_large_timestamps() {
        assert_eq!(extract_tfdt(&build_moof(&build_tfdt(1, 90000 * 3600)), 90000), Some(3600.0));
    }

    #[test]
    fn tfdt_64bit_overflowing_32bit() {
        assert_eq!(extract_tfdt(&build_moof(&build_tfdt(1, 90000 * 50000)), 90000), Some(50000.0));
    }

    #[test]
    fn tfdt_none_for_empty_buffer() {
        assert_eq!(extract_tfdt(&[], 90000), None);
    }

    #[test]
    fn tfdt_none_for_zero_timescale() {
        assert_eq!(extract_tfdt(&build_moof(&build_tfdt(0, 90000)), 0), None);
    }

    #[test]
    fn tfdt_none_without_moof() {
        let mut buf = vec![0u8; 100];
        buf[..4].copy_from_slice(&100u32.to_be_bytes());
        buf[4..8].copy_from_slice(b"mdat");
        assert_eq!(extract_tfdt(&buf, 90000), None);
    }

    #[test]
    fn tfdt_none_for_moof_without_traf() {
        let buf = bx(b"moof", &bx(b"mfhd", &[]));
        assert_eq!(buf.len(), 16);
        assert_eq!(extract_tfdt(&buf, 90000), None);
    }

    #[test]
    fn tfdt_after_other_boxes() {
        let mut buf = bx(b"styp", &[0u8; 8]);
        buf.extend(build_moof(&build_tfdt(0, 48000 * 20)));
        assert_eq!(extract_tfdt(&buf, 48000), Some(20.0));
    }

    // ── Parser ──

    pub fn moov(timescale: u32, video: bool) -> Vec<u8> {
        let mut mdhd = vec![0u8; 4]; // version 0 + flags
        mdhd.extend([0u8; 8]); // creation, modification
        mdhd.extend(timescale.to_be_bytes());
        mdhd.extend([0u8; 8]);
        let mut hdlr = vec![0u8; 8];
        hdlr.extend(if video { b"vide" } else { b"soun" });
        hdlr.extend([0u8; 12]);
        let mut mdia = bx(b"mdhd", &mdhd);
        mdia.extend(bx(b"hdlr", &hdlr));
        bx(b"moov", &bx(b"trak", &bx(b"mdia", &mdia)))
    }

    /// moof with a tfhd default duration and a trun of `samples` samples, plus tfdt at `base`.
    pub fn fragment(base: u32, samples: u32, sample_dur: u32, payload: usize) -> Vec<u8> {
        let mut tfhd = vec![0, 0, 0, 0x08, 0, 0, 0, 1];
        tfhd.extend(sample_dur.to_be_bytes());
        let mut trun = vec![0, 0, 0, 0];
        trun.extend(samples.to_be_bytes());
        let mut traf = bx(b"tfhd", &tfhd);
        traf.extend(build_tfdt(0, base as u64));
        traf.extend(bx(b"trun", &trun));
        let mut out = bx(b"moof", &bx(b"traf", &traf));
        out.extend(bx(b"mdat", &vec![7u8; payload]));
        out
    }

    #[test]
    fn timescale_prefers_the_video_track() {
        assert_eq!(extract_timescale(&moov(12800, true)), 12800);
        assert_eq!(extract_timescale(&moov(48000, false)), 48000);
        let mut two = bx(b"trak", &moov(48000, false)[16..]);
        two.extend(bx(b"trak", &moov(12800, true)[16..]));
        assert_eq!(extract_timescale(&bx(b"moov", &two)), 12800);
        assert_eq!(extract_timescale(&bx(b"moov", &[0u8; 4])), 0);
    }

    #[test]
    fn splits_init_and_merges_fragments_to_target_duration() {
        let mut stream = bx(b"ftyp", b"isom\0\0\0\0");
        stream.extend(moov(1000, true));
        let init_len = stream.len();
        // four 1-second fragments, then one 0.5 s
        for i in 0..4 {
            stream.extend(fragment(i * 1000, 10, 100, 32));
        }
        stream.extend(fragment(4000, 5, 100, 32));

        let mut p = Fmp4Parser::new(3.0);
        let mut events = Vec::new();
        // feed in awkward chunk sizes
        for chunk in stream.chunks(13) {
            events.extend(p.push(chunk));
        }
        assert_eq!(p.timescale, 1000);
        assert_eq!(events.len(), 2, "init + one 3 s segment so far");
        match &events[0] {
            Event::Init(b) => assert_eq!(b.len(), init_len),
            e => panic!("expected init, got {:?}", e),
        }
        let one = fragment(0, 10, 100, 32).len();
        match &events[1] {
            Event::Segment(b, d) => {
                assert_eq!(b.len(), 3 * one);
                assert!((d - 3.0).abs() < 1e-9);
                assert_eq!(extract_tfdt(b, p.timescale), Some(0.0));
            }
            e => panic!("expected segment, got {:?}", e),
        }
        let tail = p.flush();
        assert_eq!(tail.len(), 1);
        match &tail[0] {
            Event::Segment(b, d) => {
                assert!((d - 1.5).abs() < 1e-9);
                assert_eq!(extract_tfdt(b, p.timescale), Some(3.0));
            }
            e => panic!("expected segment, got {:?}", e),
        }
        assert!(p.flush().is_empty());
    }

    #[test]
    fn fragments_without_durations_are_emitted_one_by_one() {
        let mut stream = moov(1000, true);
        let frag = {
            let mut f = bx(b"moof", &bx(b"mfhd", &[0u8; 8]));
            f.extend(bx(b"mdat", &[1u8; 10]));
            f
        };
        stream.extend(&frag);
        stream.extend(&frag);
        stream.extend(&frag);
        let mut p = Fmp4Parser::new(3.0);
        let ev = p.push(&stream);
        // init, then the first two fragments (the third is still open)
        assert_eq!(ev.len(), 3);
        assert_eq!(ev[1], Event::Segment(frag.clone(), 3.0));
        assert_eq!(p.flush(), vec![Event::Segment(frag.clone(), 3.0)]);
    }

    #[test]
    fn truncated_boxes_never_panic() {
        let mut p = Fmp4Parser::new(3.0);
        let mut junk = bx(b"moov", &[0xFF; 40]);
        junk.extend(bx(b"moof", &bx(b"traf", &bx(b"trun", &[0, 0, 1, 0, 0, 0, 0, 9]))));
        junk.extend(bx(b"mdat", &[0u8; 3]));
        junk.extend(bx(b"moof", &[0xFF; 7]));
        let _ = p.push(&junk);
        let _ = p.flush();
        assert_eq!(extract_tfdt(&junk, 1000), None);
    }
}

//! Just enough AV1 bitstream reading to hand a PC's AV1 stream to a Mac.
//!
//! An AV1 stream is a run of OBUs ("open bitstream units"), each with a
//! one-byte header and (in the format encoders send) its size. The Mac's
//! decoder wants two things the PC's encoder doesn't give it separately:
//! the stream's description ("av1C": profile, level, colour layout plus the
//! sequence header), and each frame without its "temporal delimiter" OBU.
//! Both come from reading the sequence header, which is what this does.
//! No OS code, so it is unit-tested here; the Mac side only calls it.

// In plain words: only used when a PC sends AV1 video to a Mac. The Mac's
// decoder needs a short description of the stream before it starts; this
// file reads that description out of the stream itself.
/// OBU types we care about.
pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;

/// One OBU inside a frame's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obu<'a> {
    pub kind: u8,
    /// The whole OBU, header included.
    pub bytes: &'a [u8],
    /// Just the payload.
    pub payload: &'a [u8],
}

// A LEB128 number: 7 bits per byte, the top bit says "more bytes follow".
// Up to 8 bytes; returns the value and how many bytes it used.
fn leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &b) in data.iter().take(8).enumerate() {
        value |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Split a frame into its OBUs. Stops at the first malformed one.
pub fn obus(mut data: &[u8]) -> Vec<Obu<'_>> {
    let mut out = Vec::new();
    while let Some(&h) = data.first() {
        // First byte of an OBU: bits 3-6 are its type, bit 2 "has an extension
        // byte", bit 1 "has a size field".
        let kind = (h >> 3) & 0x0F;
        let has_ext = h & 0x04 != 0;
        let has_size = h & 0x02 != 0;
        let header_len = 1 + usize::from(has_ext);
        if data.len() < header_len {
            break;
        }
        // With a size field, read it; without, the OBU runs to the end.
        let (size, size_len) = if has_size {
            match leb128(&data[header_len..]) {
                Some((s, n)) => (usize::try_from(s).unwrap_or(usize::MAX), n),
                None => break,
            }
        } else {
            (data.len() - header_len, 0)
        };
        let start = header_len + size_len;
        // An OBU claiming to be longer than the data left is broken: stop here.
        let Some(end) = start.checked_add(size).filter(|&e| e <= data.len()) else { break };
        out.push(Obu { kind, bytes: &data[..end], payload: &data[start..end] });
        data = &data[end..];
    }
    out
}

/// The frame without temporal delimiters (what a Mac decoder sample holds).
pub fn without_delimiters(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for o in obus(data) {
        if o.kind != OBU_TEMPORAL_DELIMITER {
            out.extend_from_slice(o.bytes);
        }
    }
    out
}

/// The sequence header OBU (whole), if this frame carries one.
pub fn sequence_header(data: &[u8]) -> Option<&[u8]> {
    obus(data).into_iter().find(|o| o.kind == OBU_SEQUENCE_HEADER).map(|o| o.bytes)
}

/// Does this frame start a new picture the decoder can begin from? (A
/// sequence header comes with every keyframe from the encoders we use.)
pub fn is_keyframe(data: &[u8]) -> bool {
    sequence_header(data).is_some()
}

// Reads a byte string one bit at a time, most significant bit first, as
// the AV1 specification describes its fields.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    // Read the next n bits as a number.
    fn f(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            let bit = (byte >> (7 - (self.pos % 8))) & 1;
            v = (v << 1) | u32::from(bit);
            self.pos += 1;
        }
        Some(v)
    }

    fn flag(&mut self) -> Option<bool> {
        self.f(1).map(|b| b == 1)
    }

    // A variable-length number used by AV1: count leading zero bits, then read
    // that many bits.
    fn uvlc(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros >= 32 {
                return Some(u32::MAX);
            }
        }
        Some(self.f(zeros)? + ((1u32 << zeros) - 1))
    }
}

/// What the Mac needs to know from a sequence header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // the header's own yes/no fields
pub struct SequenceInfo {
    pub profile: u8,
    pub level: u8,
    pub tier: u8,
    pub width: u32,
    pub height: u32,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub monochrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
}

/// Read a sequence header OBU's payload (AV1 spec, section 5.5).
#[allow(clippy::too_many_lines)] // the spec's field order, read top to bottom
pub fn parse_sequence_header(payload: &[u8]) -> Option<SequenceInfo> {
    // Fields in exactly the order of the AV1 specification. Many are only
    // skipped (read and thrown away) to reach the ones the Mac needs: profile,
    // level, size and colour layout. Any field running past the end gives None.
    let mut b = Bits { data: payload, pos: 0 };
    let profile = b.f(3)? as u8;
    let _still_picture = b.flag()?;
    let reduced = b.flag()?;
    let (level, tier);
    if reduced {
        level = b.f(5)? as u8;
        tier = 0;
    } else {
        let timing = b.flag()?;
        let mut decoder_model = false;
        let mut buffer_delay_len = 0;
        if timing {
            b.f(32)?; // num_units_in_display_tick
            b.f(32)?; // time_scale
            if b.flag()? {
                b.uvlc()?; // num_ticks_per_picture_minus_1
            }
            decoder_model = b.flag()?;
            if decoder_model {
                buffer_delay_len = b.f(5)? + 1;
                b.f(32)?; // num_units_in_decoding_tick
                b.f(5)?; // buffer_removal_time_length_minus_1
                b.f(5)?; // frame_presentation_time_length_minus_1
            }
        }
        let initial_display_delay = b.flag()?;
        let count = b.f(5)? + 1;
        let (mut first_level, mut first_tier) = (0u8, 0u8);
        for i in 0..count {
            b.f(12)?; // operating_point_idc
            let lv = b.f(5)? as u8;
            let tr = if lv > 7 { b.f(1)? as u8 } else { 0 };
            if decoder_model && b.flag()? {
                b.f(buffer_delay_len)?; // decoder_buffer_delay
                b.f(buffer_delay_len)?; // encoder_buffer_delay
                b.f(1)?; // low_delay_mode_flag
            }
            if initial_display_delay && b.flag()? {
                b.f(4)?;
            }
            if i == 0 {
                first_level = lv;
                first_tier = tr;
            }
        }
        level = first_level;
        tier = first_tier;
    }
    // Picture size: a 4-bit length, then that many bits, for width and height.
    let width_bits = b.f(4)? + 1;
    let height_bits = b.f(4)? + 1;
    let width = b.f(width_bits)? + 1;
    let height = b.f(height_bits)? + 1;
    if !reduced && b.flag()? {
        b.f(4)?; // delta_frame_id_length_minus_2
        b.f(3)?; // additional_frame_id_length_minus_1
    }
    b.f(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    if !reduced {
        b.f(4)?; // interintra, masked compound, warped motion, dual filter
        let order_hint = b.flag()?;
        if order_hint {
            b.f(2)?; // enable_jnt_comp, enable_ref_frame_mvs
        }
        let force_screen_content = if b.flag()? { 2 } else { b.f(1)? };
        if force_screen_content > 0 && !b.flag()? {
            b.f(1)?; // seq_force_integer_mv
        }
        if order_hint {
            b.f(3)?; // order_hint_bits_minus_1
        }
    }
    b.f(3)?; // enable_superres, enable_cdef, enable_restoration
             // color_config()
    let high_bitdepth = b.flag()?;
    let twelve_bit = profile == 2 && high_bitdepth && b.flag()?;
    let monochrome = profile != 1 && b.flag()?;
    // Colour description: defaults mean "not specified" (2).
    let (mut cp, mut tc, mut mc) = (2, 2, 2);
    if b.flag()? {
        cp = b.f(8)?;
        tc = b.f(8)?;
        mc = b.f(8)?;
    }
    let (sub_x, sub_y, mut position) = if monochrome {
        b.f(1)?; // color_range
        (true, true, 0)
    } else if cp == 1 && tc == 13 && mc == 0 {
        (false, false, 0)
    } else {
        b.f(1)?; // color_range
        match profile {
            0 => (true, true, 0),
            1 => (false, false, 0),
            _ if twelve_bit => {
                let x = b.flag()?;
                let y = x && b.flag()?;
                (x, y, 0)
            }
            _ => (true, false, 0),
        }
    };
    if !monochrome && sub_x && sub_y {
        position = b.f(2)? as u8;
    }
    Some(SequenceInfo {
        profile,
        level,
        tier,
        width,
        height,
        high_bitdepth,
        twelve_bit,
        monochrome,
        subsampling_x: sub_x,
        subsampling_y: sub_y,
        chroma_sample_position: position,
    })
}

/// The "av1C" description a Mac decoder wants: four bytes of summary, then
/// the sequence header OBU itself.
pub fn av1c(seq_obu: &[u8], info: &SequenceInfo) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + seq_obu.len());
    // Byte 1: marker bit + version. Byte 2: profile and level. Byte 3: tier,
    // bit depth and colour layout flags. Byte 4: no presentation delay. Then the
    // sequence header itself.
    out.push(0x81); // marker + version 1
    out.push((info.profile << 5) | (info.level & 0x1F));
    out.push(
        (info.tier << 7)
            | (u8::from(info.high_bitdepth) << 6)
            | (u8::from(info.twelve_bit) << 5)
            | (u8::from(info.monochrome) << 4)
            | (u8::from(info.subsampling_x) << 3)
            | (u8::from(info.subsampling_y) << 2)
            | (info.chroma_sample_position & 3),
    );
    out.push(0); // no initial presentation delay
    out.extend_from_slice(seq_obu);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes bits MSB-first, to build test sequence headers.
    #[derive(Default)]
    struct W {
        bytes: Vec<u8>,
        bits: usize,
    }

    impl W {
        fn put(&mut self, n: u32, v: u32) -> &mut Self {
            for i in (0..n).rev() {
                if self.bits % 8 == 0 {
                    self.bytes.push(0);
                }
                let bit = ((v >> i) & 1) as u8;
                let last = self.bytes.last_mut().unwrap();
                *last |= bit << (7 - (self.bits % 8));
                self.bits += 1;
            }
            self
        }
    }

    /// A typical real-time encoder's header: profile 0, level 4.1 (13),
    /// 1920x1080, 8-bit 4:2:0, BT.709 colour, order hints on.
    fn sample_header() -> Vec<u8> {
        let mut w = W::default();
        w.put(3, 0).put(1, 0).put(1, 0); // profile, still, reduced
        w.put(1, 0); // timing_info_present
        w.put(1, 0); // initial_display_delay_present
        w.put(5, 0); // one operating point
        w.put(12, 0).put(5, 13); // idc, level 13 (no tier bit: <= 7? no, 13 > 7)
        w.put(1, 0); // tier 0
        w.put(4, 10).put(4, 10); // 11-bit width/height fields
        w.put(11, 1919).put(11, 1079);
        w.put(1, 0); // frame ids
        w.put(3, 0b011); // 128 sb off, filter intra on, edge filter on
        w.put(4, 0); // compound tools off
        w.put(1, 1); // order hint on
        w.put(2, 0); // jnt, ref mvs
        w.put(1, 1); // choose screen content tools
        w.put(1, 1); // choose integer mv
        w.put(3, 6); // order hint bits - 1
        w.put(3, 0b010); // superres, cdef, restoration
        w.put(1, 0); // high bitdepth
        w.put(1, 0); // mono
        w.put(1, 1).put(8, 1).put(8, 1).put(8, 1); // BT.709
        w.put(1, 0); // color range (video)
        w.put(2, 0); // chroma sample position
        w.put(1, 1); // trailing bit
        w.bytes
    }

    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![(kind << 3) | 0x02];
        out.push(payload.len() as u8); // < 128 in tests
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn reads_a_sequence_header() {
        let info = parse_sequence_header(&sample_header()).unwrap();
        assert_eq!((info.profile, info.level, info.tier), (0, 13, 0));
        assert_eq!((info.width, info.height), (1920, 1080));
        assert!(!info.high_bitdepth && !info.monochrome);
        assert!(info.subsampling_x && info.subsampling_y);
    }

    #[test]
    fn splits_frames_and_builds_av1c() {
        let seq = obu(OBU_SEQUENCE_HEADER, &sample_header());
        let mut frame = obu(OBU_TEMPORAL_DELIMITER, &[]);
        frame.extend_from_slice(&seq);
        frame.extend_from_slice(&obu(6, &[1, 2, 3])); // OBU_FRAME
        let parts = obus(&frame);
        assert_eq!(parts.iter().map(|o| o.kind).collect::<Vec<_>>(), vec![2, 1, 6]);
        assert!(is_keyframe(&frame));
        let clean = without_delimiters(&frame);
        assert_eq!(clean.len(), frame.len() - 2);
        assert_eq!(sequence_header(&frame), Some(&seq[..]));
        let info = parse_sequence_header(parts[1].payload).unwrap();
        let c = av1c(&seq, &info);
        assert_eq!(&c[..4], &[0x81, 13, 0b0000_1100, 0]);
        assert_eq!(&c[4..], &seq[..]);
        // A P-frame alone: no header, not a keyframe.
        assert!(!is_keyframe(&obu(6, &[9])));
    }

    #[test]
    fn junk_does_not_panic() {
        assert!(obus(&[0xFF]).is_empty() || obus(&[0xFF]).len() == 1);
        assert_eq!(parse_sequence_header(&[]), None);
        let _ = obus(&[0x0A, 0xFF, 0xFF, 0xFF]);
    }
}

//! Reads a `.bit` file, to check before configuring that it is for this board.
//!
//! A Vivado `.bit` is a small header followed by the raw configuration data. The
//! header holds the design name, the device part and the date and time, so a
//! bitstream for another board can be refused before it is sent. A wrong
//! bitstream does no harm, because JTAG configuration is volatile, but a board
//! that "does not work" is expensive to debug later.
//!
//! ```text
//!   00 09  0f f0 0f f0 0f f0 0f f0 00     fixed header
//!   00 01  'a'  <u16 len> <design name>   text fields end with NUL
//!          'b'  <u16 len> <device part>
//!          'c'  <u16 len> <date>
//!          'd'  <u16 len> <time>
//!          'e'  <u32 len> <configuration data>
//! ```

/// The fixed bytes at the start of the header.
const MAGIC: [u8; 11] = [
    0x00, 0x09, 0x0f, 0xf0, 0x0f, 0xf0, 0x0f, 0xf0, 0x0f, 0xf0, 0x00,
];

#[derive(Debug)]
pub enum Error {
    /// The file does not look like a `.bit`.
    NotABitstream { at: usize, why: &'static str },
    /// A header field is missing.
    Missing { key: char },
    /// The device does not match the target.
    WrongDevice { bitstream: String, target: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotABitstream { at, why } => write!(
                f,
                "this does not look like a Vivado .bit file ({why}, at byte {at}).\n\
                 A raw .bin has no header, so hio cannot check it against the board. \
                 Use the .bit that Vivado wrote next to it."
            ),
            Error::Missing { key } => write!(
                f,
                "the .bit header has no '{key}' field, so it cannot be read.\n\
                 Fields run 'a' design, 'b' device, 'c' date, 'd' time, 'e' data."
            ),
            Error::WrongDevice { bitstream, target } => write!(
                f,
                "the bitstream is for {bitstream:?}, but the target says {target:?}.\n\
                 This would configure the wrong device, so hio refuses. The names are \
                 compared without the `xc` prefix and the speed grade. If they really \
                 match, pass the target used to build this bitstream."
            ),
        }
    }
}

impl std::error::Error for Error {}

/// The parsed header and configuration data.
#[derive(Debug)]
pub struct Bitstream {
    /// Design name. Vivado writes it as `<top>;UserID=...;Version=...`.
    pub design: String,
    /// Device part as written in the header. For 7-series it has no `xc` prefix
    /// and no speed grade; see `part_matches`.
    pub part: String,
    pub date: String,
    pub time: String,
    /// Raw configuration data, sent to `CFG_IN` as it is.
    pub data: Vec<u8>,
}

impl Bitstream {
    /// The part of the design name before `;` (the top module name).
    pub fn top(&self) -> &str {
        self.design.split(';').next().unwrap_or(&self.design)
    }
}

fn be16(b: &[u8], at: usize) -> Result<usize, Error> {
    if at + 2 > b.len() {
        return Err(Error::NotABitstream {
            at,
            why: "ran off the end reading a length",
        });
    }
    Ok(u16::from_be_bytes([b[at], b[at + 1]]) as usize)
}

/// Drops the NUL terminator.
fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .trim_end_matches('\0')
        .to_string()
}

pub fn parse(bytes: &[u8]) -> Result<Bitstream, Error> {
    if bytes.len() < MAGIC.len() || bytes[..MAGIC.len()] != MAGIC {
        return Err(Error::NotABitstream {
            at: 0,
            why: "the fixed header at the start does not match",
        });
    }
    let mut at = MAGIC.len();

    // `00 01`, then the keyed fields start with 'a'.
    if be16(bytes, at)? != 1 {
        return Err(Error::NotABitstream {
            at,
            why: "expected a one-byte key to follow the header",
        });
    }
    at += 2;

    let mut fields: [Option<String>; 4] = [None, None, None, None];
    let mut data: Option<Vec<u8>> = None;

    while at < bytes.len() {
        let key = bytes[at] as char;
        at += 1;
        match key {
            'a' | 'b' | 'c' | 'd' => {
                let len = be16(bytes, at)?;
                at += 2;
                if at + len > bytes.len() {
                    return Err(Error::NotABitstream {
                        at,
                        why: "a header field claims more bytes than the file has",
                    });
                }
                fields[key as usize - 'a' as usize] = Some(text(&bytes[at..at + len]));
                at += len;
            }
            'e' => {
                if at + 4 > bytes.len() {
                    return Err(Error::NotABitstream {
                        at,
                        why: "ran off the end reading the data length",
                    });
                }
                let len =
                    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
                        as usize;
                at += 4;
                if at + len > bytes.len() {
                    return Err(Error::NotABitstream {
                        at,
                        why: "the configuration data is shorter than the header says",
                    });
                }
                data = Some(bytes[at..at + len].to_vec());
                break;
            }
            _ => {
                return Err(Error::NotABitstream {
                    at: at - 1,
                    why: "unknown header key",
                });
            }
        }
    }

    let [design, part, date, time] = fields;
    Ok(Bitstream {
        design: design.ok_or(Error::Missing { key: 'a' })?,
        part: part.ok_or(Error::Missing { key: 'b' })?,
        date: date.ok_or(Error::Missing { key: 'c' })?,
        time: time.ok_or(Error::Missing { key: 'd' })?,
        data: data.ok_or(Error::Missing { key: 'e' })?,
    })
}

/// Bus width detect pattern. Each SLR's sub-bitstream starts with it.
const BUS_WIDTH_DETECT: [u8; 8] = [0x00, 0x00, 0x00, 0xbb, 0x11, 0x22, 0x00, 0x44];

/// A Type 1 write of no words to BOUT (register 0x1e). Right before each
/// sub-bitstream after the first, followed by a Type 2 write header whose
/// word count is how much of the file goes on to the next SLR.
const BOUT: [u8; 4] = [0x30, 0x03, 0xc0, 0x00];

/// A run of the configuration data, and the SLR it goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    pub at: usize,
    pub bytes: usize,
    /// The sub-bitstream it belongs to, counted in file order.
    pub slr: usize,
    /// The end of a sub-bitstream that resumes after the later ones.
    pub tail: bool,
}

/// Why a multi-SLR `.bit` cannot be split.
#[derive(Debug)]
pub struct SplitError {
    pub at: usize,
    pub why: String,
}

impl std::fmt::Display for SplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the .bit cannot be split per SLR: {} (at byte {}).\n\
             Program with an SVF instead: `make svf` writes one next to the .bit, and \
             `hio program <file>.svf` replays it.",
            self.why, self.at
        )
    }
}

impl std::error::Error for SplitError {}

impl Bitstream {
    /// Splits the data into the runs each SLR receives, in file order.
    ///
    /// Sub-bitstreams nest. Each one after the first is wrapped in a BOUT
    /// write whose length covers it and every later one, so the file reads
    ///
    /// ```text
    ///   SLR a head | BOUT(n) [ SLR b head | BOUT(m) [ SLR c ] SLR b tail ] SLR a tail
    /// ```
    ///
    /// The BOUT words themselves are not sent. The lengths differ between
    /// designs on the same device, so they are read here, never written down.
    ///
    /// A single-SLR device gives one piece that covers the whole file.
    pub fn slr_pieces(&self) -> Result<Vec<Piece>, SplitError> {
        let d = &self.data;
        let starts = self.slr_starts();
        if starts.len() <= 1 {
            return Ok(vec![Piece {
                at: 0,
                bytes: d.len(),
                slr: 0,
                tail: false,
            }]);
        }
        if starts[0] != 0 {
            return Err(SplitError {
                at: 0,
                why: "the first sub-bitstream does not start the file".into(),
            });
        }

        // ends[k]: where the BOUT write that wraps sub-bitstream k ends.
        let mut ends = vec![d.len()];
        for (k, &s) in starts.iter().enumerate().skip(1) {
            if s < 8 || d[s - 8..s - 4] != BOUT {
                return Err(SplitError {
                    at: s,
                    why: format!("sub-bitstream {k} is not preceded by a BOUT write"),
                });
            }
            let word = u32::from_be_bytes([d[s - 4], d[s - 3], d[s - 2], d[s - 1]]);
            // Type 2 (`010`), write (`10`).
            if word >> 27 != 0b01010 {
                return Err(SplitError {
                    at: s - 4,
                    why: format!("the BOUT write before sub-bitstream {k} has no Type 2 length"),
                });
            }
            let end = s + (word & 0x07ff_ffff) as usize * 4;
            let outer = ends[k - 1];
            if end > outer || s <= starts[k - 1] {
                return Err(SplitError {
                    at: s,
                    why: format!("sub-bitstream {k} does not sit inside the one before it"),
                });
            }
            ends.push(end);
        }

        let n = starts.len();
        let mut out = Vec::new();
        for k in 0..n {
            let end = if k + 1 < n {
                starts[k + 1] - 8
            } else {
                ends[k]
            };
            if end <= starts[k] {
                return Err(SplitError {
                    at: starts[k],
                    why: format!("sub-bitstream {k} is empty"),
                });
            }
            out.push(Piece {
                at: starts[k],
                bytes: end - starts[k],
                slr: k,
                tail: false,
            });
        }
        for k in (0..n - 1).rev() {
            if ends[k] > ends[k + 1] {
                out.push(Piece {
                    at: ends[k + 1],
                    bytes: ends[k] - ends[k + 1],
                    slr: k,
                    tail: true,
                });
            }
        }
        Ok(out)
    }

    /// Finds where each SLR's sub-bitstream starts.
    ///
    /// On a device with several SLRs (such as VU9P) the `.bit` is the SLR
    /// bitstreams joined together. Each one starts with dummy words, the bus
    /// width detect pattern and the sync word, which marks the boundary.
    ///
    /// A single-SLR device returns one start.
    pub fn slr_starts(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let d = &self.data;
        let mut at = 0;
        while at + 8 <= d.len() {
            if d[at..at + 8] == BUS_WIDTH_DETECT {
                // Step back over the dummy words (`ffffffff`) before it.
                let mut start = at;
                while start >= 4 && d[start - 4..start] == [0xff; 4] {
                    start -= 4;
                }
                out.push(start);
                at += 8;
            } else {
                at += 4;
            }
        }
        out
    }
}

/// Normalizes a part name for comparison: lower case, no separators, no `xc`.
fn normalize(part: &str) -> String {
    let lower = part.to_ascii_lowercase();
    let stripped = lower.strip_prefix("xc").unwrap_or(&lower);
    stripped
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// Whether the bitstream's device is the target's device.
///
/// It is a prefix match, because the spelling differs:
///
/// | 'b' field seen in a real `.bit` | target description |
/// |---|---|
/// | `7a35ticsg324` | `xc7a35ticsg324-1L` |
/// | `xcvu9p-flga2104-2L-e` | `xcvu9p-flga2104-2L-e` |
///
/// 7-series drops the `xc` prefix and the speed grade; UltraScale+ does not.
/// A target name shorter than the bitstream's does not match: that is a
/// different device.
pub fn part_matches(bitstream: &str, target: &str) -> bool {
    let (b, t) = (normalize(bitstream), normalize(target));
    !b.is_empty() && t.starts_with(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_with(design: &str, part: &str, data: &[u8]) -> Vec<u8> {
        build(design, part, data)
    }

    fn build(design: &str, part: &str, data: &[u8]) -> Vec<u8> {
        let mut v = MAGIC.to_vec();
        v.extend_from_slice(&[0x00, 0x01]);
        for (key, text) in [
            ('a', design),
            ('b', part),
            ('c', "2026/09/02"),
            ('d', "12:00:00"),
        ] {
            v.push(key as u8);
            let mut s = text.as_bytes().to_vec();
            s.push(0);
            v.extend_from_slice(&(s.len() as u16).to_be_bytes());
            v.extend_from_slice(&s);
        }
        v.push(b'e');
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn a_header_yields_the_design_the_device_and_the_data() {
        let raw = build(
            "top;UserID=0XFFFFFFFF",
            "7a35ticsg324",
            &[0xaa, 0x99, 0x55, 0x66],
        );
        let b = parse(&raw).unwrap();
        assert_eq!(b.top(), "top");
        assert_eq!(b.part, "7a35ticsg324");
        assert_eq!(b.data, [0xaa, 0x99, 0x55, 0x66]);
    }

    /// A `.bin` has no header to check, and the error says so.
    #[test]
    fn a_raw_bin_is_refused_with_a_reason() {
        let err = parse(&[0xff; 64]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains(".bin"), "{text}");
    }

    /// A truncated file is an error, not shorter data.
    #[test]
    fn a_truncated_file_does_not_parse_as_short_data() {
        let mut raw = build("top", "7a35ticsg324", &[0u8; 100]);
        raw.truncate(raw.len() - 10);
        assert!(parse(&raw).is_err());
    }

    #[test]
    fn the_slr_starts_are_found_by_the_bus_width_pattern() {
        let mut data = Vec::new();
        for _ in 0..3 {
            data.extend_from_slice(&[0xff; 16]);
            data.extend_from_slice(&BUS_WIDTH_DETECT);
            data.extend_from_slice(&[0xaa, 0x99, 0x55, 0x66]);
            data.extend_from_slice(&[0x20u8, 0x00, 0x00, 0x00].repeat(8));
        }
        let raw = build_with("top", "7a35ticsg324", &data);
        let b = parse(&raw).unwrap();
        assert_eq!(b.slr_starts(), vec![0, 60, 120]);
    }

    /// One sub-bitstream: dummy words, the pattern, sync, then `words` NOOPs.
    fn sub(words: usize) -> Vec<u8> {
        let mut v = [0xff; 8].to_vec();
        v.extend_from_slice(&BUS_WIDTH_DETECT);
        v.extend_from_slice(&[0xaa, 0x99, 0x55, 0x66]);
        v.extend_from_slice(&[0x20u8, 0x00, 0x00, 0x00].repeat(words));
        v
    }

    /// Wraps `inner` in a BOUT write of its own length.
    fn bout(inner: &[u8]) -> Vec<u8> {
        let mut v = BOUT.to_vec();
        v.extend_from_slice(&(0x5000_0000u32 | (inner.len() / 4) as u32).to_be_bytes());
        v.extend_from_slice(inner);
        v
    }

    /// Three SLRs nested the way Vivado writes them for VU9P and VU440.
    fn three(head_words: [usize; 3], tail_words: [usize; 2]) -> Vec<u8> {
        let nop = |n: usize| [0x20u8, 0x00, 0x00, 0x00].repeat(n);
        let mut b = sub(head_words[1]);
        b.extend_from_slice(&bout(&sub(head_words[2])));
        b.extend_from_slice(&nop(tail_words[1]));
        let mut a = sub(head_words[0]);
        a.extend_from_slice(&bout(&b));
        a.extend_from_slice(&nop(tail_words[0]));
        a
    }

    #[test]
    fn the_bout_lengths_split_the_slrs_with_their_tails() {
        let data = three([10, 8, 6], [411, 15]);
        let b = parse(&build("top", "xcvu440", &data)).unwrap();
        let p = b.slr_pieces().unwrap();
        let head = |n: usize| 20 + n * 4;
        let piece = |at, bytes, slr, tail| Piece {
            at,
            bytes,
            slr,
            tail,
        };
        let a1 = head(10) + 8;
        let a2 = a1 + head(8) + 8;
        let t1 = a2 + head(6);
        let t0 = t1 + 15 * 4;
        assert_eq!(
            p,
            vec![
                piece(0, head(10), 0, false),
                piece(a1, head(8), 1, false),
                piece(a2, head(6), 2, false),
                piece(t1, 15 * 4, 1, true),
                piece(t0, 411 * 4, 0, true),
            ]
        );
        // Everything but the two BOUT pairs is sent.
        let sent: usize = p.iter().map(|p| p.bytes).sum();
        assert_eq!(sent + 16, data.len());
    }

    /// The same device with a different design: the lengths move, and are
    /// still read from the file.
    #[test]
    fn a_different_design_moves_the_boundaries() {
        let a = parse(&build("top", "xcvu440", &three([10, 10, 10], [411, 15]))).unwrap();
        let b = parse(&build("top", "xcvu440", &three([10, 8, 8], [411, 15]))).unwrap();
        assert_ne!(a.slr_pieces().unwrap(), b.slr_pieces().unwrap());
    }

    /// A second pattern with no BOUT write before it is refused, not guessed at.
    #[test]
    fn a_boundary_without_bout_is_refused() {
        let mut data = sub(4);
        data.extend_from_slice(&sub(4));
        let b = parse(&build("top", "xcvu440", &data)).unwrap();
        let e = b.slr_pieces().unwrap_err().to_string();
        assert!(e.contains("BOUT") && e.contains(".svf"), "{e}");
    }

    #[test]
    fn one_slr_is_one_piece() {
        let data = sub(4);
        let b = parse(&build("top", "7a35ticsg324", &data)).unwrap();
        assert_eq!(
            b.slr_pieces().unwrap(),
            vec![Piece {
                at: 0,
                bytes: data.len(),
                slr: 0,
                tail: false
            }]
        );
    }

    /// The spelling differs per family. Both strings come from real files.
    #[test]
    fn the_device_matches_whichever_spelling_the_tools_used() {
        // The 'b' field of a .bit that Vivado 2021.2 wrote for Arty.
        assert!(part_matches("7a35ticsg324", "xc7a35ticsg324-1L"));
        // The same Vivado for VCU118. This one is not shortened.
        assert!(part_matches("xcvu9p-flga2104-2L-e", "xcvu9p-flga2104-2L-e"));
    }

    #[test]
    fn a_different_device_does_not_match() {
        assert!(!part_matches("7a100tcsg324", "xc7a35ticsg324-1L"));
        assert!(!part_matches("xcvu9p-flga2104-2L-e", "xc7a35ticsg324-1L"));
        // A target shorter than the bitstream's name does not match either.
        assert!(!part_matches("7a35ticsg324xyz", "xc7a35ticsg324"));
        assert!(!part_matches("", "xc7a35ticsg324"));
    }
}

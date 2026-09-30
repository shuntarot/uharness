//! Replays an SVF (Serial Vector Format) file that Vivado wrote.
//!
//! ## Why replay
//!
//! On a device with several SLRs (such as VU9P), the `.bit` is the SLR
//! bitstreams joined together, and each one must go to its own `CFG_IN`. Xilinx
//! does not publish where they split, and a guessed split must not be trusted.
//!
//! Vivado writes an SVF without a board. The build machine already has Vivado,
//! so it can write the `.svf` next to the `.bit`, and the machine with the FPGA
//! still needs no Vivado. Replay works for any device family, and it includes
//! the status checks that Vivado does.
//!
//! ## Scope
//!
//! Only the commands Vivado uses for programming. An unknown command is an
//! error. Skipping it would report a step as done when it was not done.

use crate::mpsse::{Chan, Exit, Mpsse};

#[derive(Debug)]
pub enum Error<E> {
    Io(E),
    /// A command that cannot be parsed.
    Parse {
        line: usize,
        what: String,
    },
    /// A command this player does not implement.
    Unsupported {
        line: usize,
        command: String,
    },
    /// TDO differs from the expected value: the device reports a failed step.
    Mismatch {
        line: usize,
        want: String,
        got: String,
        mask: String,
    },
}

impl<E: std::fmt::Debug> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "jtag io failed: {e:?}"),
            Error::Parse { line, what } => {
                write!(f, "line {line} of the SVF could not be read: {what}")
            }
            Error::Unsupported { line, command } => write!(
                f,
                "line {line} of the SVF uses `{command}`, which this player does not \
                 implement.\n\
                 hio stops here. Skipping it would report a step as replayed when it was \
                 not replayed."
            ),
            Error::Mismatch {
                line,
                want,
                got,
                mask,
            } => write!(
                f,
                "line {line} of the SVF expected TDO {want} (mask {mask}) but the device \
                 gave {got}.\n\
                 The device says this step failed. Programming stops here, so it does not \
                 go on with a half-configured device."
            ),
        }
    }
}

/// A stable TAP state named by SVF `ENDIR` / `ENDDR` / `STATE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rest {
    Idle,
    Pause,
    Reset,
}

impl Rest {
    fn parse(word: &str) -> Option<Rest> {
        match word {
            "IDLE" => Some(Rest::Idle),
            "DRPAUSE" | "IRPAUSE" => Some(Rest::Pause),
            "RESET" => Some(Rest::Reset),
            _ => None,
        }
    }

    fn exit(self) -> Exit {
        match self {
            Rest::Idle => Exit::Idle,
            Rest::Pause => Exit::Pause,
            Rest::Reset => Exit::Reset,
        }
    }
}

/// Converts a hex string to bytes in shift order (LSB first).
///
/// SVF writes an `n`-bit value MSB first, but JTAG shifts LSB first, so the end
/// of the string is shifted first. Returns `None` if the string is not hex.
pub fn hex_to_bytes(hex: &str, bits: usize) -> Option<Vec<u8>> {
    // Vivado wraps long hex values with a newline every 254 characters, so
    // whitespace is skipped.
    let mut digits: Vec<u8> = Vec::with_capacity(hex.len());
    for c in hex.bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        if !c.is_ascii_hexdigit() {
            return None;
        }
        digits.push(c);
    }
    let want = bits.div_ceil(8);
    let mut out = Vec::with_capacity(want);
    let mut at = digits.len();
    while out.len() < want {
        let lo = if at >= 1 {
            (digits[at - 1] as char).to_digit(16)? as u8
        } else {
            0
        };
        let hi = if at >= 2 {
            (digits[at - 2] as char).to_digit(16)? as u8
        } else {
            0
        };
        out.push((hi << 4) | lo);
        at = at.saturating_sub(2);
    }
    // Clear the bits above `bits` in the last byte.
    let rest = bits % 8;
    if rest != 0 {
        let last = out.len() - 1;
        out[last] &= (1u16 << rest).wrapping_sub(1) as u8;
    }
    Some(out)
}

fn bytes_to_hex(bytes: &[u8], bits: usize) -> String {
    let mut s = String::new();
    for b in bytes.iter().rev() {
        s.push_str(&format!("{b:02x}"));
    }
    let want = bits.div_ceil(4);
    if s.len() > want {
        s = s[s.len() - want..].to_string();
    }
    s
}

/// One SVF command.
#[derive(Debug)]
enum Op {
    Endir(Rest),
    Enddr(Rest),
    State(Rest),
    Scan {
        ir: bool,
        bits: usize,
        tdi: Vec<u8>,
        tdo: Option<(Vec<u8>, Vec<u8>)>,
    },
    Runtest {
        clocks: Option<u64>,
        seconds: Option<f64>,
    },
    /// Parsed but has no effect (`FREQUENCY`, `TRST OFF`).
    Ignore,
}

/// Returns the text inside `(...)` after `key`.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let at = text.find(key)?;
    let rest = &text[at + key.len()..];
    let open = rest.find('(')?;
    let close = rest.find(')')?;
    Some(rest[open + 1..close].trim())
}

fn parse_op<E>(text: &str, line: usize) -> Result<Op, Error<E>> {
    let bad = |what: &str| Error::Parse {
        line,
        what: what.to_string(),
    };
    let words: Vec<&str> = text.split_whitespace().collect();
    let head = words.first().copied().unwrap_or("");

    match head {
        "TRST" | "FREQUENCY" | "HDR" | "HIR" | "TDR" | "TIR" => Ok(Op::Ignore),
        "ENDIR" | "ENDDR" | "STATE" => {
            // STATE may list several states. Only the last one matters: the
            // others are only passed through.
            let last = words
                .iter()
                .skip(1)
                .rev()
                .find_map(|w| Rest::parse(w.trim_end_matches(';')))
                .ok_or_else(|| bad("no state that this player knows"))?;
            Ok(match head {
                "ENDIR" => Op::Endir(last),
                "ENDDR" => Op::Enddr(last),
                _ => Op::State(last),
            })
        }
        "SIR" | "SDR" => {
            let bits: usize = words
                .get(1)
                .and_then(|w| w.parse().ok())
                .ok_or_else(|| bad("no bit count"))?;
            let tdi = match field(text, "TDI") {
                Some(h) => hex_to_bytes(h, bits).ok_or_else(|| bad("TDI is not hex"))?,
                None => vec![0u8; bits.div_ceil(8)],
            };
            let tdo = match field(text, "TDO") {
                Some(h) => {
                    let want = hex_to_bytes(h, bits).ok_or_else(|| bad("TDO is not hex"))?;
                    let mask = match field(text, "MASK") {
                        Some(m) => hex_to_bytes(m, bits).ok_or_else(|| bad("MASK is not hex"))?,
                        None => vec![0xff; bits.div_ceil(8)],
                    };
                    Some((want, mask))
                }
                None => None,
            };
            Ok(Op::Scan {
                ir: head == "SIR",
                bits,
                tdi,
                tdo,
            })
        }
        "RUNTEST" => {
            let mut clocks = None;
            let mut seconds = None;
            let mut i = 1;
            while i < words.len() {
                let w = words[i].trim_end_matches(';');
                if let Ok(v) = w.parse::<f64>() {
                    match words.get(i + 1).map(|u| u.trim_end_matches(';')) {
                        Some("TCK") => clocks = Some(v as u64),
                        Some("SEC") => seconds = Some(v),
                        _ => {}
                    }
                }
                i += 1;
            }
            Ok(Op::Runtest { clocks, seconds })
        }
        "" => Ok(Op::Ignore),
        other => Err(Error::Unsupported {
            line,
            command: other.to_string(),
        }),
    }
}

/// SVF player.
pub struct Player<'a, C: Chan> {
    m: &'a mut Mpsse<C>,
    endir: Rest,
    enddr: Rest,
    /// Current TAP state. Needed to continue a scan from Pause.
    at: Rest,
    /// Bits scanned so far, for progress output.
    pub scanned_bits: u64,
}

/// Bytes per USB transfer.
const PIECE: usize = 16 * 1024;

impl<'a, C: Chan> Player<'a, C> {
    pub fn new(m: &'a mut Mpsse<C>) -> Self {
        Player {
            m,
            endir: Rest::Idle,
            enddr: Rest::Idle,
            at: Rest::Reset,
            scanned_bits: 0,
        }
    }

    /// Replays the SVF command by command. `on_progress` gets the bits scanned
    /// so far.
    pub fn run(
        &mut self,
        text: &str,
        mut on_progress: impl FnMut(u64),
    ) -> Result<(), Error<C::Error>> {
        // SVF commands end with `;`. `line` counts commands, not text lines.
        for (i, raw) in text.split(';').enumerate() {
            let line = i + 1;
            // Only drop comments here. Keep the newlines: `hex_to_bytes`
            // handles wrapped hex, and extra separators would break it.
            let cleaned: String = raw
                .lines()
                .map(|l| l.split("//").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");
            if cleaned.trim().is_empty() {
                continue;
            }
            let op = parse_op(&cleaned, line)?;
            self.exec(op, line)?;
            on_progress(self.scanned_bits);
        }
        Ok(())
    }

    fn exec(&mut self, op: Op, line: usize) -> Result<(), Error<C::Error>> {
        match op {
            Op::Ignore => Ok(()),
            Op::Endir(r) => {
                self.endir = r;
                Ok(())
            }
            Op::Enddr(r) => {
                self.enddr = r;
                Ok(())
            }
            Op::State(r) => self.goto(r),
            Op::Runtest { clocks, seconds } => {
                self.goto(Rest::Idle)?;
                if let Some(n) = clocks {
                    let mut cmds = Vec::new();
                    crate::mpsse::push_idle_clocks(&mut cmds, n as usize);
                    self.m.send(&cmds).map_err(Error::Io)?;
                }
                if let Some(s) = seconds {
                    std::thread::sleep(std::time::Duration::from_secs_f64(s));
                }
                Ok(())
            }
            Op::Scan { ir, bits, tdi, tdo } => self.scan(ir, bits, &tdi, tdo.as_ref(), line),
        }
    }

    /// Moves the TAP to `to`.
    fn goto(&mut self, to: Rest) -> Result<(), Error<C::Error>> {
        if self.at == to {
            return Ok(());
        }
        let mut cmds = Vec::new();
        match to {
            Rest::Reset => crate::mpsse::push_tap_reset(&mut cmds),
            Rest::Idle => match self.at {
                // From Reset, TMS=0 goes to Idle.
                Rest::Reset => crate::mpsse::push_idle_clocks(&mut cmds, 1),
                // From Pause: Exit2 -> Update -> Idle.
                Rest::Pause => crate::mpsse::push_pause_to_idle(&mut cmds),
                Rest::Idle => {}
            },
            Rest::Pause => {
                // Vivado never writes a STATE that ends in Pause.
                return Err(Error::Unsupported {
                    line: 0,
                    command: "STATE ...PAUSE".to_string(),
                });
            }
        }
        self.m.send(&cmds).map_err(Error::Io)?;
        self.at = to;
        Ok(())
    }
}

impl<C: Chan> Player<'_, C> {
    /// One scan.
    ///
    /// A scan with no TDO check is sent without reading back. A bitstream has
    /// tens of millions of bits, and reading them back doubles the USB traffic.
    fn scan(
        &mut self,
        ir: bool,
        bits: usize,
        tdi: &[u8],
        tdo: Option<&(Vec<u8>, Vec<u8>)>,
        line: usize,
    ) -> Result<(), Error<C::Error>> {
        if bits == 0 {
            return Ok(());
        }
        let rest = if ir { self.endir } else { self.enddr };
        let exit = rest.exit();

        let mut cmds = Vec::new();
        // Continuing from Pause must not pass Capture, or the DR contents change.
        if self.at == Rest::Pause && !ir {
            crate::mpsse::push_resume_from_pause(&mut cmds);
        } else {
            self.goto(Rest::Idle)?;
            crate::mpsse::push_enter(&mut cmds, ir);
        }

        let body = bits - 1;
        let whole = body / 8;
        let ragged = body % 8;
        let last = (tdi[body / 8] >> (body % 8)) & 1 == 1;

        if let Some((want, mask)) = tdo {
            // Read back. Only short scans check TDO, so one round trip is enough.
            let bodybits: Vec<bool> = (0..body)
                .map(|i| (tdi[i / 8] >> (i % 8)) & 1 == 1)
                .collect();
            let read_len = crate::mpsse::push_body(&mut cmds, &bodybits) + 1;
            let (_, tail_bits) = crate::mpsse::push_tail(&mut cmds, last, exit);
            cmds.push(crate::mpsse::CMD_SEND_IMMEDIATE);
            let raw = self.m.exchange(&cmds, read_len).map_err(Error::Io)?;
            let got = crate::mpsse::scan_decode(&raw, bits, tail_bits);

            let mut bytes = vec![0u8; bits.div_ceil(8)];
            for (i, &b) in got.iter().enumerate() {
                if b {
                    bytes[i / 8] |= 1 << (i % 8);
                }
            }
            for i in 0..bytes.len() {
                if bytes[i] & mask[i] != want[i] & mask[i] {
                    return Err(Error::Mismatch {
                        line,
                        want: bytes_to_hex(want, bits),
                        got: bytes_to_hex(&bytes, bits),
                        mask: bytes_to_hex(mask, bits),
                    });
                }
            }
        } else {
            // No read back. Large scans take this path.
            let mut sent = 0;
            while sent < whole {
                let n = (whole - sent).min(PIECE);
                crate::mpsse::push_write_bytes(&mut cmds, &tdi[sent..sent + n]);
                self.m.send(&cmds).map_err(Error::Io)?;
                cmds.clear();
                sent += n;
            }
            if ragged > 0 {
                crate::mpsse::push_write_bits(&mut cmds, tdi[whole], ragged);
            }
            crate::mpsse::push_tail_quiet(&mut cmds, last, exit);
            self.m.send(&cmds).map_err(Error::Io)?;
        }

        self.at = rest;
        self.scanned_bits += bits as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpsse::tests::{new_ftdi, probe_cfg};

    /// Vivado wraps hex every 254 characters.
    #[test]
    fn wrapped_hex_is_read_as_one_number() {
        assert_eq!(
            hex_to_bytes("04b3\n1093", 32).unwrap(),
            hex_to_bytes("04b31093", 32).unwrap()
        );
        assert!(hex_to_bytes("04b3 xx93", 32).is_none());
    }

    /// SVF writes values MSB first, but JTAG shifts LSB first. If this order is
    /// wrong, every scan is wrong.
    #[test]
    fn the_end_of_the_hex_string_is_shifted_first() {
        assert_eq!(
            hex_to_bytes("04b31093", 32).unwrap(),
            [0x93, 0x10, 0xb3, 0x04]
        );
        // Bits above the length are dropped.
        assert_eq!(hex_to_bytes("3f", 6).unwrap(), [0x3f]);
        assert_eq!(hex_to_bytes("ff", 6).unwrap(), [0x3f]);
        // Missing digits are zero.
        assert_eq!(hex_to_bytes("1", 18).unwrap(), [0x01, 0x00, 0x00]);
    }

    #[test]
    fn an_unknown_command_stops_the_replay() {
        let mut io = crate::mpsse::Mpsse::open(new_ftdi(7, 0), &probe_cfg()).unwrap();
        let err = Player::new(&mut io)
            .run("PIOMAP (OUT 1);", |_| {})
            .unwrap_err();
        assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
        assert!(err.to_string().contains("not replayed"), "{err}");
    }

    /// IR and DR scans from an SVF reach the model TAP: select USER1, write to
    /// the window, and read it back.
    #[test]
    fn a_scan_written_as_svf_reaches_the_window() {
        let mut io = crate::mpsse::Mpsse::attach(new_ftdi(7, 0), &probe_cfg()).unwrap();
        // 41-bit DR: op=write(10), wdata=0x1234, addr=0x08.
        let frame = crate::frame::Frame::write(0x08, 0x1234).to_bits(7);
        let mut v = 0u128;
        for (i, &b) in frame.iter().enumerate() {
            if b {
                v |= 1 << i;
            }
        }
        let svf = format!("STATE RESET;\nSTATE IDLE;\nSIR 6 TDI (02);\nSDR 41 TDI ({v:011x});\n");
        Player::new(&mut io).run(&svf, |_| {}).unwrap();

        let read = crate::frame::Frame::read(0x08).to_bits(7);
        let mut r = 0u128;
        for (i, &b) in read.iter().enumerate() {
            if b {
                r |= 1 << i;
            }
        }
        Player::new(&mut io)
            .run(&format!("SDR 41 TDI ({r:011x});\n"), |_| {})
            .unwrap();

        let got = crate::bridge::JtagIo::scan_dr(&mut io, &crate::frame::Frame::nop().to_bits(7))
            .unwrap();
        assert_eq!(
            crate::frame::Response::from_bits(&got).rdata,
            0x1234,
            "the SVF must have reached the window"
        );
    }

    /// A TDO mismatch stops the replay instead of going on.
    #[test]
    fn a_tdo_mismatch_stops_rather_than_carrying_on() {
        let mut io = crate::mpsse::Mpsse::attach(new_ftdi(7, 0), &probe_cfg()).unwrap();
        let err = Player::new(&mut io)
            .run(
                "STATE RESET;\nSTATE IDLE;\nSIR 6 TDI (02) TDO (3f) MASK (3f);\n",
                |_| {},
            )
            .unwrap_err();
        assert!(matches!(err, Error::Mismatch { .. }), "{err:?}");
        assert!(err.to_string().contains("half-configured"), "{err}");
    }
}

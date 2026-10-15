//! RISC-V semihosting: the program asks the host to do something for it.
//!
//! The program runs `slli x0, x0, 0x1f; ebreak; srai x0, x0, 7`, with the
//! operation in `a0` and its parameter in `a1`. With `dcsr.ebreakm` set the
//! `ebreak` halts into the debugger, which does the operation, puts the
//! result in `a0` and lets the hart go on past it. The operations are ARM's.
//!
//! Only what a program on a bare board needs is answered: writing to the
//! terminal, the exit status and the time. Host files and keyboard input are
//! not offered; those calls return -1.

use crate::rtt::Memory;

/// The three instructions around the `ebreak`.
pub const SEQUENCE: [u32; 3] = [0x01f0_1013, 0x0010_0073, 0x4070_5013];

/// Whether `code` holds the call sequence, so the program may call the host.
pub fn appears_in(code: &[u8]) -> bool {
    let bytes: Vec<u8> = SEQUENCE.iter().flat_map(|w| w.to_le_bytes()).collect();
    // Compressed code puts instructions on 2-byte boundaries.
    code.windows(bytes.len())
        .step_by(2)
        .any(|window| window == bytes.as_slice())
}

/// What the program asked for, after the host has done its part.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Goes back in `a0`; the program runs on.
    Return(u64),
    /// The program is over, with this status.
    Exit(i32),
}

/// What a call writes to the terminal.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// `ADP_Stopped_ApplicationExit`: the program ended as it meant to.
const APPLICATION_EXIT: u64 = 0x20026;

/// The handles `:tt` opens as, by mode: read, write, append.
const STDIN: u64 = 0;
const STDOUT: u64 = 1;
const STDERR: u64 = 2;

/// The longest string `SYS_WRITE0` follows before giving up on its NUL.
const MAX_STRING: usize = 1 << 16;

/// Does operation `op` with parameter `param` for a hart of `xlen` bits.
/// `started` is when the program started, for `SYS_CLOCK`.
pub fn serve(
    mem: &mut dyn Memory,
    op: u64,
    param: u64,
    xlen: u32,
    started: std::time::Instant,
    out: &mut Output,
) -> Result<Outcome, String> {
    let field = u64::from(xlen / 8);
    let fail = Outcome::Return(u64::MAX);
    // The `n`th field of the parameter block.
    let arg = |mem: &mut dyn Memory, n: u64| -> Result<u64, String> {
        let mut raw = [0u8; 8];
        mem.read(param + n * field, &mut raw[..field as usize])?;
        Ok(u64::from_le_bytes(raw))
    };
    let bytes = |mem: &mut dyn Memory, addr: u64, len: u64| -> Result<Vec<u8>, String> {
        let mut buf = vec![0; len as usize];
        mem.read(addr, &mut buf)?;
        Ok(buf)
    };
    Ok(match op {
        // SYS_OPEN: only the terminal, `:tt`.
        0x01 => {
            let (name, mode, len) = (arg(mem, 0)?, arg(mem, 1)?, arg(mem, 2)?);
            if len == 3 && bytes(mem, name, 3)? == b":tt" {
                Outcome::Return(match mode {
                    0..=3 => STDIN,
                    4..=7 => STDOUT,
                    _ => STDERR,
                })
            } else {
                fail
            }
        }
        // SYS_CLOSE
        0x02 => match arg(mem, 0)? {
            STDIN..=STDERR => Outcome::Return(0),
            _ => fail,
        },
        // SYS_WRITEC: the parameter points at the character.
        0x03 => {
            out.stdout.extend(bytes(mem, param, 1)?);
            Outcome::Return(0)
        }
        // SYS_WRITE0: a string up to its NUL.
        0x04 => {
            let mut at = param;
            loop {
                let chunk = bytes(mem, at, 64)?;
                match chunk.iter().position(|&b| b == 0) {
                    Some(end) => {
                        out.stdout.extend_from_slice(&chunk[..end]);
                        break;
                    }
                    None => out.stdout.extend_from_slice(&chunk),
                }
                at += 64;
                if out.stdout.len() > MAX_STRING {
                    return Err("SYS_WRITE0 found no end to its string".to_string());
                }
            }
            Outcome::Return(0)
        }
        // SYS_WRITE: returns the bytes it did not write.
        0x05 => {
            let (handle, buf, len) = (arg(mem, 0)?, arg(mem, 1)?, arg(mem, 2)?);
            let to = match handle {
                STDOUT => &mut out.stdout,
                STDERR => &mut out.stderr,
                _ => return Ok(Outcome::Return(len)),
            };
            to.extend(bytes(mem, buf, len)?);
            Outcome::Return(0)
        }
        // SYS_READ: nothing is read, as at the end of a file.
        0x06 => Outcome::Return(arg(mem, 2)?),
        // SYS_ISERROR: whether a status is negative.
        0x08 => {
            let status = arg(mem, 0)?;
            Outcome::Return(u64::from(status >> (xlen - 1) & 1 == 1))
        }
        // SYS_ISTTY
        0x09 => Outcome::Return(u64::from(matches!(arg(mem, 0)?, STDIN..=STDERR))),
        // SYS_CLOCK: centiseconds since the program started.
        0x10 => Outcome::Return((started.elapsed().as_millis() / 10) as u64),
        // SYS_TIME: seconds since 1970.
        0x11 => Outcome::Return(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        ),
        // SYS_ERRNO: the calls that fail set none.
        0x13 => Outcome::Return(0),
        // SYS_EXIT: a 32-bit hart passes the reason itself; a 64-bit one
        // points at the reason and a status.
        0x18 => {
            let (reason, status) = if xlen == 32 {
                (param, 0)
            } else {
                (arg(mem, 0)?, arg(mem, 1)?)
            };
            Outcome::Exit(exit_status(reason, status))
        }
        // SYS_EXIT_EXTENDED: always the reason and a status.
        0x20 => Outcome::Exit(exit_status(arg(mem, 0)?, arg(mem, 1)?)),
        _ => fail,
    })
}

/// The status a program that exits normally gives; any other way out is 1.
fn exit_status(reason: u64, status: u64) -> i32 {
    if reason == APPLICATION_EXIT {
        status as i32
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Memory at 0x1000.
    struct Ram(Vec<u8>);

    impl Memory for Ram {
        fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), String> {
            let at = (addr - 0x1000) as usize;
            buf.copy_from_slice(&self.0[at..at + buf.len()]);
            Ok(())
        }
        fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), String> {
            let at = (addr - 0x1000) as usize;
            self.0[at..at + data.len()].copy_from_slice(data);
            Ok(())
        }
    }

    fn ram() -> Ram {
        Ram(vec![0; 0x400])
    }

    fn put(ram: &mut Ram, addr: u64, data: &[u8]) {
        ram.write(addr, data).unwrap();
    }

    fn words(ram: &mut Ram, addr: u64, words: &[u32]) {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        put(ram, addr, &bytes);
    }

    fn call(ram: &mut Ram, op: u64, param: u64, out: &mut Output) -> Outcome {
        serve(ram, op, param, 32, std::time::Instant::now(), out).unwrap()
    }

    #[test]
    fn the_call_sequence_is_found_on_a_halfword() {
        let mut code = vec![0x13, 0x00];
        code.extend(SEQUENCE.iter().flat_map(|w| w.to_le_bytes()));
        assert!(appears_in(&code));
        assert!(!appears_in(&code[2..12]));
        assert!(!appears_in(&[0x73, 0x00, 0x10, 0x00]));
    }

    /// What a Rust `hprintln!` does: open `:tt` for writing, write to it.
    #[test]
    fn writing_to_the_terminal_goes_to_stdout_and_stderr() {
        let mut ram = ram();
        let mut out = Output::default();
        put(&mut ram, 0x1100, b":tt");
        words(&mut ram, 0x1000, &[0x1100, 4, 3]);
        assert_eq!(
            call(&mut ram, 0x01, 0x1000, &mut out),
            Outcome::Return(STDOUT)
        );
        words(&mut ram, 0x1000, &[0x1100, 8, 3]);
        assert_eq!(
            call(&mut ram, 0x01, 0x1000, &mut out),
            Outcome::Return(STDERR)
        );

        put(&mut ram, 0x1200, b"hello\n");
        words(&mut ram, 0x1000, &[STDOUT as u32, 0x1200, 6]);
        assert_eq!(call(&mut ram, 0x05, 0x1000, &mut out), Outcome::Return(0));
        words(&mut ram, 0x1000, &[STDERR as u32, 0x1200, 5]);
        assert_eq!(call(&mut ram, 0x05, 0x1000, &mut out), Outcome::Return(0));
        assert_eq!(out.stdout, b"hello\n");
        assert_eq!(out.stderr, b"hello");

        // WRITE0 runs to the NUL, past one read chunk.
        let long = [b'x'; 100];
        put(&mut ram, 0x1300, &long);
        assert_eq!(call(&mut ram, 0x04, 0x1300, &mut out), Outcome::Return(0));
        assert_eq!(out.stdout.len(), 6 + 100);
        put(&mut ram, 0x1380, b"!");
        assert_eq!(call(&mut ram, 0x03, 0x1380, &mut out), Outcome::Return(0));
        assert_eq!(out.stdout.last(), Some(&b'!'));
    }

    #[test]
    fn files_are_not_offered() {
        let mut ram = ram();
        let mut out = Output::default();
        put(&mut ram, 0x1100, b"/etc/passwd");
        words(&mut ram, 0x1000, &[0x1100, 0, 11]);
        assert_eq!(
            call(&mut ram, 0x01, 0x1000, &mut out),
            Outcome::Return(u64::MAX)
        );
        // A write to a handle that is not the terminal writes nothing.
        words(&mut ram, 0x1000, &[7, 0x1100, 4]);
        assert_eq!(call(&mut ram, 0x05, 0x1000, &mut out), Outcome::Return(4));
        assert!(out.stdout.is_empty());
        assert_eq!(call(&mut ram, 0x30, 0, &mut out), Outcome::Return(u64::MAX));
    }

    #[test]
    fn an_exit_gives_the_status() {
        let mut ram = ram();
        let mut out = Output::default();
        words(&mut ram, 0x1000, &[0x20026, 3]);
        assert_eq!(call(&mut ram, 0x20, 0x1000, &mut out), Outcome::Exit(3));
        assert_eq!(call(&mut ram, 0x18, 0x20026, &mut out), Outcome::Exit(0));
        // Any other reason is a failure.
        assert_eq!(call(&mut ram, 0x18, 0x20023, &mut out), Outcome::Exit(1));
        // 64-bit SYS_EXIT points at a block.
        put(
            &mut ram,
            0x1000,
            &[0x26, 0x00, 0x02, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0],
        );
        let exit = serve(
            &mut ram,
            0x18,
            0x1000,
            64,
            std::time::Instant::now(),
            &mut out,
        );
        assert_eq!(exit, Ok(Outcome::Exit(5)));
    }
}

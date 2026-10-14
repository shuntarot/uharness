//! Reads and writes a SEGGER RTT control block in DUT memory.
//!
//! RTT is a control block and ring buffers that the firmware keeps in its own
//! memory. The host finds the block, reads up channels (target to host) and
//! writes down channels (host to target). Nothing in the harness RTL takes
//! part: the host reaches the memory through the window, like `hio read`.
//!
//! Only the memory access depends on the harness. It is the [`Memory`] trait,
//! so the rest runs in tests against a plain buffer.

/// DUT memory, by DUT byte address.
pub trait Memory {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), String>;
    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), String>;
}

/// The id at the start of the control block. The firmware writes it last,
/// and backwards, so a block that shows it is fully set up.
pub const MAGIC: &[u8] = b"SEGGER RTT";

const ID_BYTES: u64 = 16;

/// How wide the fields of the control block are.
///
/// Two layouts are in use on 64-bit targets. rtt-target (Rust) makes every
/// field a `usize`. SEGGER's C code keeps the counts, size and offsets at 32
/// bits, and only the pointers are 8 bytes. On a 32-bit target both are the
/// same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Bytes of a pointer (name, buffer).
    pub ptr: u64,
    /// Bytes of a count, a size or an offset.
    pub field: u64,
}

impl Layout {
    /// Bytes of the header: the id, then the numbers of up and down channels.
    pub fn header_bytes(&self) -> u64 {
        ID_BYTES + 2 * self.field
    }

    /// Bytes of one channel: name, buffer, size, write offset, read offset,
    /// flags.
    pub fn channel_bytes(&self) -> u64 {
        2 * self.ptr + 4 * self.field
    }

    /// Bytes of a whole block with these channel counts.
    pub fn block_bytes(&self, up: u64, down: u64) -> u64 {
        self.header_bytes() + (up + down) * self.channel_bytes()
    }

    /// The layouts a target with `ptr`-byte pointers can have.
    fn candidates(ptr: u64) -> Vec<Layout> {
        if ptr == 4 {
            vec![Layout { ptr: 4, field: 4 }]
        } else {
            vec![Layout { ptr, field: ptr }, Layout { ptr, field: 4 }]
        }
    }
}

/// Up (target to host) or down (host to target).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
}

/// A control block found in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rtt {
    pub addr: u64,
    pub layout: Layout,
    pub up: u64,
    pub down: u64,
}

/// Why [`Rtt::attach`] did not find a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachError {
    /// No magic yet. The firmware may not have set it up; try again.
    NotReady,
    /// Something is there but cannot be used. Trying again will not help.
    Broken(String),
}

impl From<String> for AttachError {
    fn from(error: String) -> Self {
        AttachError::Broken(error)
    }
}

/// What one channel says now. Read fresh for every use: a target may point a
/// channel at another buffer after boot (NuttX's RTT serial driver does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Channel {
    buffer: u64,
    size: u64,
    write: u64,
    read: u64,
}

impl Rtt {
    /// Finds the block at `addr`.
    ///
    /// `ptr` is the target's pointer width (4 or 8, from the ELF class).
    /// `size` is `st_size` of `_SEGGER_RTT` when known. On a 64-bit target it
    /// picks the layout: the one whose computed size matches. Without it, the
    /// layout is guessed from the header, as a hand-given address has no
    /// size.
    pub fn attach(
        mem: &mut impl Memory,
        addr: u64,
        ptr: u64,
        size: Option<u64>,
    ) -> Result<Rtt, AttachError> {
        let mut header = [0u8; 32];
        mem.read(addr, &mut header)?;
        if !header.starts_with(MAGIC) {
            return Err(AttachError::NotReady);
        }

        let counts = |layout: &Layout| {
            let at = ID_BYTES as usize;
            let f = layout.field as usize;
            (le(&header[at..at + f]), le(&header[at + f..at + 2 * f]))
        };
        let plausible = |(up, down): (u64, u64)| (1..=64).contains(&up) && down <= 64;

        let candidates = Layout::candidates(ptr);
        let matching: Vec<Layout> = match size {
            Some(size) => candidates
                .iter()
                .copied()
                .filter(|layout| {
                    let (up, down) = counts(layout);
                    plausible((up, down)) && layout.block_bytes(up, down) == size
                })
                .collect(),
            // As a u64, a 32-bit max_up has max_down in its upper half. With
            // no size to compare, prefer the layout whose counts look sane.
            None => candidates
                .iter()
                .copied()
                .filter(|layout| plausible(counts(layout)))
                .take(1)
                .collect(),
        };
        let layout = match matching.as_slice() {
            [one] => *one,
            [] => {
                return Err(AttachError::Broken(format!(
                    "the RTT control block at {addr:#x} does not match any known layout{}.\n\
                     It may be a different RTT implementation, or the address is wrong.",
                    size.map(|s| format!(" of {s} bytes")).unwrap_or_default()
                )));
            }
            _ => {
                return Err(AttachError::Broken(format!(
                    "the RTT control block at {addr:#x} fits more than one layout.\n\
                     Say which with --rtt-layout."
                )));
            }
        };
        let (up, down) = counts(&layout);
        Ok(Rtt {
            addr,
            layout,
            up,
            down,
        })
    }

    /// Finds the block at `addr` with a layout the caller states, for a block
    /// that fits more than one.
    pub fn attach_as(mem: &mut impl Memory, addr: u64, layout: Layout) -> Result<Rtt, AttachError> {
        let mut header = vec![0u8; layout.header_bytes() as usize];
        mem.read(addr, &mut header)?;
        if !header.starts_with(MAGIC) {
            return Err(AttachError::NotReady);
        }
        let f = layout.field as usize;
        let at = ID_BYTES as usize;
        Ok(Rtt {
            addr,
            layout,
            up: le(&header[at..at + f]),
            down: le(&header[at + f..at + 2 * f]),
        })
    }

    /// The channel of `dir` whose name is `name`, if there is one. Names are
    /// how a channel with a special meaning is found; the numbers are the
    /// firmware's to choose.
    pub fn find(&self, mem: &mut impl Memory, dir: Dir, name: &str) -> Result<Option<u64>, String> {
        let count = match dir {
            Dir::Up => self.up,
            Dir::Down => self.down,
        };
        for n in 0..count {
            let at = self.channel_addr(dir, n)?;
            let mut ptr = vec![0u8; self.layout.ptr as usize];
            mem.read(at, &mut ptr)?;
            let ptr = le(&ptr);
            if ptr == 0 {
                continue;
            }
            // One more byte than the name, to see that it ends there.
            let mut text = vec![0u8; name.len() + 1];
            if mem.read(ptr, &mut text).is_ok()
                && text[..name.len()] == *name.as_bytes()
                && text[name.len()] == 0
            {
                return Ok(Some(n));
            }
        }
        Ok(None)
    }

    /// Whether the magic is still there. Gone means the target was reset or
    /// reloaded under the host.
    pub fn alive(&self, mem: &mut impl Memory) -> Result<bool, String> {
        let mut id = [0u8; MAGIC.len()];
        mem.read(self.addr, &mut id)?;
        Ok(id == MAGIC)
    }

    /// Where channel `n` of `dir` starts.
    fn channel_addr(&self, dir: Dir, n: u64) -> Result<u64, String> {
        let (count, first) = match dir {
            Dir::Up => (self.up, 0),
            Dir::Down => (self.down, self.up),
        };
        if n >= count {
            return Err(format!(
                "the target has {count} {} channel(s), so there is no channel {n}",
                match dir {
                    Dir::Up => "up",
                    Dir::Down => "down",
                }
            ));
        }
        Ok(self.addr + self.layout.header_bytes() + (first + n) * self.layout.channel_bytes())
    }

    fn channel(&self, mem: &mut impl Memory, at: u64) -> Result<Channel, String> {
        let l = self.layout;
        let mut raw = vec![0u8; l.channel_bytes() as usize];
        mem.read(at, &mut raw)?;
        let (p, f) = (l.ptr as usize, l.field as usize);
        let channel = Channel {
            buffer: le(&raw[p..2 * p]),
            size: le(&raw[2 * p..2 * p + f]),
            write: le(&raw[2 * p + f..2 * p + 2 * f]),
            read: le(&raw[2 * p + 2 * f..2 * p + 3 * f]),
        };
        Ok(channel)
    }

    /// Writes an offset field. Only the low 32 bits: an offset is below the
    /// buffer size, so the upper half of a wide field stays 0, and the
    /// window writes 32-bit words.
    fn set_offset(&self, mem: &mut impl Memory, at: u64, value: u64) -> Result<(), String> {
        mem.write(at, &(value as u32).to_le_bytes())
    }

    /// Takes up to `max` bytes out of up channel `n`. Returns them; empty when
    /// there is nothing new, or when the channel is not set up yet.
    pub fn read_up(&self, mem: &mut impl Memory, n: u64, max: usize) -> Result<Vec<u8>, String> {
        let at = self.channel_addr(Dir::Up, n)?;
        let ch = self.channel(mem, at)?;
        if ch.buffer == 0 || ch.size == 0 || ch.write >= ch.size || ch.read >= ch.size {
            return Ok(Vec::new());
        }
        // Up to the end of the buffer only; the rest comes on the next call.
        // That keeps the wrap out of the copy.
        let available = if ch.write >= ch.read {
            ch.write - ch.read
        } else {
            ch.size - ch.read
        };
        let take = available.min(max as u64);
        if take == 0 {
            return Ok(Vec::new());
        }
        let mut out = vec![0u8; take as usize];
        mem.read(ch.buffer + ch.read, &mut out)?;
        // The data is read before the offset moves, so the target cannot
        // overwrite it in between.
        let read_at = at + 2 * self.layout.ptr + 2 * self.layout.field;
        self.set_offset(mem, read_at, (ch.read + take) % ch.size)?;
        Ok(out)
    }

    /// Puts bytes into down channel `n`. Returns how many fit; the rest is
    /// for the caller to drop or retry.
    pub fn write_down(&self, mem: &mut impl Memory, n: u64, data: &[u8]) -> Result<usize, String> {
        let at = self.channel_addr(Dir::Down, n)?;
        let ch = self.channel(mem, at)?;
        if ch.buffer == 0 || ch.size == 0 || ch.write >= ch.size || ch.read >= ch.size {
            return Ok(0);
        }
        // One slot stays empty, so full and empty differ.
        let space = (ch.read + ch.size - ch.write - 1) % ch.size;
        // Up to the end of the buffer only, like `read_up`.
        let take = (data.len() as u64).min(space).min(ch.size - ch.write);
        if take == 0 {
            return Ok(0);
        }
        mem.write(ch.buffer + ch.write, &data[..take as usize])?;
        let write_at = at + 2 * self.layout.ptr + self.layout.field;
        self.set_offset(mem, write_at, (ch.write + take) % ch.size)?;
        Ok(take as usize)
    }
}

/// A little-endian number of 4 or 8 bytes.
fn le(bytes: &[u8]) -> u64 {
    let mut raw = [0u8; 8];
    raw[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Memory from `base` up, as a plain buffer.
    struct Ram {
        base: u64,
        bytes: Vec<u8>,
    }

    impl Memory for Ram {
        fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<(), String> {
            let at = (addr - self.base) as usize;
            buf.copy_from_slice(&self.bytes[at..at + buf.len()]);
            Ok(())
        }
        fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), String> {
            let at = (addr - self.base) as usize;
            self.bytes[at..at + data.len()].copy_from_slice(data);
            Ok(())
        }
    }

    const BASE: u64 = 0x8000_0000;
    const CB: u64 = BASE + 0x100;
    const UP_BUF: u64 = BASE + 0x400;
    const DOWN_BUF: u64 = BASE + 0x500;

    impl Ram {
        fn put(&mut self, addr: u64, value: u64, bytes: u64) {
            let at = (addr - self.base) as usize;
            self.bytes[at..at + bytes as usize]
                .copy_from_slice(&value.to_le_bytes()[..bytes as usize]);
        }

        /// A block with one up and one down channel, laid out as `layout`.
        fn with_block(layout: Layout, up_size: u64, down_size: u64) -> Ram {
            let mut ram = Ram {
                base: BASE,
                bytes: vec![0; 0x1000],
            };
            let at = (CB - BASE) as usize;
            ram.bytes[at..at + MAGIC.len()].copy_from_slice(MAGIC);
            ram.put(CB + ID_BYTES, 1, layout.field);
            ram.put(CB + ID_BYTES + layout.field, 1, layout.field);
            for (i, (buffer, size)) in [(UP_BUF, up_size), (DOWN_BUF, down_size)]
                .into_iter()
                .enumerate()
            {
                let ch = CB + layout.header_bytes() + i as u64 * layout.channel_bytes();
                ram.put(ch + layout.ptr, buffer, layout.ptr);
                ram.put(ch + 2 * layout.ptr, size, layout.field);
            }
            ram
        }

        fn offsets(&self, layout: Layout, dir: Dir) -> (u64, u64) {
            let n = if dir == Dir::Up { 0 } else { 1 };
            let ch = CB + layout.header_bytes() + n * layout.channel_bytes();
            let get = |addr: u64| {
                let at = (addr - self.base) as usize;
                le(&self.bytes[at..at + layout.field as usize])
            };
            (
                get(ch + 2 * layout.ptr + layout.field),
                get(ch + 2 * layout.ptr + 2 * layout.field),
            )
        }

        /// The target puts `text` into the up buffer.
        fn target_writes(&mut self, layout: Layout, text: &[u8]) {
            let (write, _) = self.offsets(layout, Dir::Up);
            let size = 16;
            for (i, b) in text.iter().enumerate() {
                let at = (UP_BUF - BASE) as usize + ((write as usize + i) % size);
                self.bytes[at] = *b;
            }
            let ch = CB + layout.header_bytes();
            self.put(
                ch + 2 * layout.ptr + layout.field,
                (write + text.len() as u64) % size as u64,
                layout.field,
            );
        }
    }

    const NARROW32: Layout = Layout { ptr: 4, field: 4 };
    const WIDE64: Layout = Layout { ptr: 8, field: 8 };
    const C64: Layout = Layout { ptr: 8, field: 4 };

    #[test]
    fn the_layout_follows_the_size_of_the_symbol() {
        // One up and one down: 72, 128 and 88 bytes.
        assert_eq!(NARROW32.block_bytes(1, 1), 72);
        assert_eq!(WIDE64.block_bytes(1, 1), 128);
        assert_eq!(C64.block_bytes(1, 1), 88);
        for (layout, ptr) in [(NARROW32, 4), (WIDE64, 8), (C64, 8)] {
            let mut ram = Ram::with_block(layout, 16, 16);
            let rtt = Rtt::attach(&mut ram, CB, ptr, Some(layout.block_bytes(1, 1))).unwrap();
            assert_eq!(rtt.layout, layout);
            assert_eq!((rtt.up, rtt.down), (1, 1));
            // A hand-given address has no size; the counts still decide.
            let guessed = Rtt::attach(&mut ram, CB, ptr, None).unwrap();
            assert_eq!(guessed.layout, layout);
        }
    }

    #[test]
    fn no_magic_is_not_ready_and_a_wrong_size_is_broken() {
        let mut ram = Ram::with_block(NARROW32, 16, 16);
        assert_eq!(
            Rtt::attach(&mut ram, CB + 4, 4, None),
            Err(AttachError::NotReady)
        );
        assert!(matches!(
            Rtt::attach(&mut ram, CB, 4, Some(100)),
            Err(AttachError::Broken(_))
        ));
    }

    #[test]
    fn up_text_comes_out_across_the_wrap() {
        for layout in [NARROW32, WIDE64, C64] {
            let mut ram = Ram::with_block(layout, 16, 16);
            let rtt =
                Rtt::attach(&mut ram, CB, layout.ptr, Some(layout.block_bytes(1, 1))).unwrap();
            assert!(rtt.read_up(&mut ram, 0, 64).unwrap().is_empty());

            ram.target_writes(layout, b"hello, ");
            assert_eq!(rtt.read_up(&mut ram, 0, 64).unwrap(), b"hello, ");
            // 7 + 12 passes the end of the 16-byte buffer.
            ram.target_writes(layout, b"world again!");
            let mut got = rtt.read_up(&mut ram, 0, 64).unwrap();
            got.extend(rtt.read_up(&mut ram, 0, 64).unwrap());
            assert_eq!(got, b"world again!");
            let (write, read) = ram.offsets(layout, Dir::Up);
            assert_eq!(write, read);
            // `max` is honored.
            ram.target_writes(layout, b"abc");
            assert_eq!(rtt.read_up(&mut ram, 0, 2).unwrap(), b"ab");
        }
    }

    #[test]
    fn down_bytes_stop_one_short_of_full() {
        let layout = C64;
        let mut ram = Ram::with_block(layout, 16, 8);
        let rtt = Rtt::attach(&mut ram, CB, 8, Some(layout.block_bytes(1, 1))).unwrap();
        assert_eq!(rtt.write_down(&mut ram, 0, b"abcdefghij").unwrap(), 7);
        let at = (DOWN_BUF - BASE) as usize;
        assert_eq!(&ram.bytes[at..at + 7], b"abcdefg");
        assert_eq!(ram.offsets(layout, Dir::Down), (7, 0));
        // Full until the target reads.
        assert_eq!(rtt.write_down(&mut ram, 0, b"x").unwrap(), 0);
    }

    #[test]
    fn a_reset_target_loses_its_magic() {
        let mut ram = Ram::with_block(NARROW32, 16, 16);
        let rtt = Rtt::attach(&mut ram, CB, 4, None).unwrap();
        assert!(rtt.alive(&mut ram).unwrap());
        ram.bytes[(CB - BASE) as usize] = 0;
        assert!(!rtt.alive(&mut ram).unwrap());
    }

    #[test]
    fn a_channel_is_found_by_its_name() {
        let layout = WIDE64;
        let mut ram = Ram::with_block(layout, 16, 16);
        let name = BASE + 0x600;
        let at = (name - BASE) as usize;
        ram.bytes[at..at + 9].copy_from_slice(b"hio-exit\0");
        ram.put(CB + layout.header_bytes(), name, layout.ptr);
        let rtt = Rtt::attach(&mut ram, CB, 8, Some(layout.block_bytes(1, 1))).unwrap();
        assert_eq!(rtt.find(&mut ram, Dir::Up, "hio-exit").unwrap(), Some(0));
        // A prefix is not a match.
        assert_eq!(rtt.find(&mut ram, Dir::Up, "hio").unwrap(), None);
        assert_eq!(rtt.find(&mut ram, Dir::Down, "hio-exit").unwrap(), None);
    }

    #[test]
    fn a_stated_layout_is_used_as_is() {
        let mut ram = Ram::with_block(C64, 16, 16);
        let rtt = Rtt::attach_as(&mut ram, CB, C64).unwrap();
        assert_eq!((rtt.layout, rtt.up, rtt.down), (C64, 1, 1));
    }

    #[test]
    fn a_channel_that_does_not_exist_is_an_error() {
        let mut ram = Ram::with_block(NARROW32, 16, 16);
        let rtt = Rtt::attach(&mut ram, CB, 4, None).unwrap();
        assert!(rtt.read_up(&mut ram, 1, 8).unwrap_err().contains("1 up"));
    }
}

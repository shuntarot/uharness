//! Reads what a loader needs from an ELF: the loadable segments and the
//! symbol table.
//!
//! Only the fields in use are read, to avoid a dependency. Little-endian
//! only; both 32-bit and 64-bit.

/// A parsed ELF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elf {
    /// 64-bit (`ELFCLASS64`). Pointers on the target are this wide.
    pub wide: bool,
    /// `PT_LOAD` segments, in file order. Includes those with no file bytes.
    pub segments: Vec<Segment>,
    /// The entries of `.symtab`. Empty for a stripped file.
    pub symbols: Vec<Symbol>,
}

/// One `PT_LOAD` segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// `p_paddr`: where a loader places it. The harness window is physical
    /// memory, so this is the address that counts. `p_vaddr` is the view
    /// through an MMU.
    pub paddr: u64,
    /// `p_memsz`. Past `bytes`, the program expects zeros (`.bss`, and often
    /// the stack too).
    pub memsz: u64,
    /// The file bytes (`p_filesz` of them).
    pub bytes: Vec<u8>,
}

/// One symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// `st_value`: the run-time address. Equal to the load address for bare
    /// metal and firmware; a virtual address for an image that runs behind
    /// an MMU.
    pub value: u64,
    /// `st_size`, in bytes.
    pub size: u64,
    /// A plain number rather than an address (`SHN_ABS`), as linker scripts
    /// define sizes and counts (`_hart_stack_size`). It still names itself,
    /// but names no address.
    pub absolute: bool,
}

const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHN_ABS: u16 = 0xfff1;

impl Elf {
    /// Whether the bytes start like an ELF.
    pub fn looks_like(raw: &[u8]) -> bool {
        raw.starts_with(&[0x7f, b'E', b'L', b'F'])
    }

    pub fn parse(raw: &[u8]) -> Result<Elf, String> {
        let r = Reader { raw };
        if !Elf::looks_like(raw) {
            return Err(bad("it does not start with the ELF magic"));
        }
        if r.u8(5)? != 1 {
            return Err(bad("only little-endian ELFs are handled"));
        }
        let wide = match r.u8(4)? {
            1 => false,
            2 => true,
            _ => return Err(bad("neither 32-bit nor 64-bit")),
        };

        let (phoff, phentsize, phnum) = if wide {
            (r.u64(0x20)?, r.u16(0x36)?, r.u16(0x38)?)
        } else {
            (r.u32(0x1c)? as u64, r.u16(0x2a)?, r.u16(0x2c)?)
        };
        let mut segments = Vec::new();
        for i in 0..phnum as u64 {
            let ph = (phoff + i * phentsize as u64) as usize;
            if r.u32(ph)? != PT_LOAD {
                continue;
            }
            let (offset, paddr, filesz, memsz) = if wide {
                (
                    r.u64(ph + 0x08)?,
                    r.u64(ph + 0x18)?,
                    r.u64(ph + 0x20)?,
                    r.u64(ph + 0x28)?,
                )
            } else {
                (
                    r.u32(ph + 0x04)? as u64,
                    r.u32(ph + 0x0c)? as u64,
                    r.u32(ph + 0x10)? as u64,
                    r.u32(ph + 0x14)? as u64,
                )
            };
            let bytes = r
                .bytes(offset, filesz)
                .ok_or_else(|| bad("a segment runs past the end of the file"))?
                .to_vec();
            segments.push(Segment {
                paddr,
                memsz,
                bytes,
            });
        }

        let symbols = symbols(&r, wide)?;
        Ok(Elf {
            wide,
            segments,
            symbols,
        })
    }

    /// Looks a symbol up by name.
    pub fn symbol(&self, name: &str) -> Option<&Symbol> {
        self.symbols.iter().find(|symbol| symbol.name == name)
    }
}

/// Reads `.symtab` and its string table. None of either is not an error: a
/// stripped file has no symbols, and the caller decides whether it needs one.
fn symbols(r: &Reader, wide: bool) -> Result<Vec<Symbol>, String> {
    let (shoff, shentsize, shnum) = if wide {
        (r.u64(0x28)?, r.u16(0x3a)?, r.u16(0x3c)?)
    } else {
        (r.u32(0x20)? as u64, r.u16(0x2e)?, r.u16(0x30)?)
    };
    // (offset, size, link, entsize) of a section.
    let section = |i: u64| -> Result<(u64, u64, u32, u64, u32), String> {
        let sh = (shoff + i * shentsize as u64) as usize;
        let kind = r.u32(sh + 4)?;
        Ok(if wide {
            (
                r.u64(sh + 0x18)?,
                r.u64(sh + 0x20)?,
                r.u32(sh + 0x28)?,
                r.u64(sh + 0x38)?,
                kind,
            )
        } else {
            (
                r.u32(sh + 0x10)? as u64,
                r.u32(sh + 0x14)? as u64,
                r.u32(sh + 0x18)?,
                r.u32(sh + 0x24)? as u64,
                kind,
            )
        })
    };

    let mut out = Vec::new();
    for i in 0..shnum as u64 {
        let (offset, size, link, entsize, kind) = section(i)?;
        if kind != SHT_SYMTAB || entsize == 0 {
            continue;
        }
        let (str_offset, str_size, ..) = section(link as u64)?;
        let strings = r
            .bytes(str_offset, str_size)
            .ok_or_else(|| bad("the symbol names run past the end of the file"))?;
        for n in 0..size / entsize {
            let at = (offset + n * entsize) as usize;
            let (name, value, size, shndx) = if wide {
                (r.u32(at)?, r.u64(at + 8)?, r.u64(at + 16)?, r.u16(at + 6)?)
            } else {
                (
                    r.u32(at)?,
                    r.u32(at + 4)? as u64,
                    r.u32(at + 8)? as u64,
                    r.u16(at + 14)?,
                )
            };
            let name = strings
                .get(name as usize..)
                .and_then(|rest| rest.split(|b| *b == 0).next())
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .unwrap_or_default();
            if !name.is_empty() {
                out.push(Symbol {
                    name,
                    value,
                    size,
                    absolute: shndx == SHN_ABS,
                });
            }
        }
    }
    Ok(out)
}

fn bad(what: &str) -> String {
    format!("this ELF cannot be read: {what}")
}

struct Reader<'a> {
    raw: &'a [u8],
}

impl Reader<'_> {
    fn bytes(&self, offset: u64, len: u64) -> Option<&[u8]> {
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(usize::try_from(len).ok()?)?;
        self.raw.get(start..end)
    }

    fn array<const N: usize>(&self, at: usize) -> Result<[u8; N], String> {
        self.raw
            .get(at..at + N)
            .map(|b| b.try_into().unwrap())
            .ok_or_else(|| bad("truncated"))
    }

    fn u8(&self, at: usize) -> Result<u8, String> {
        Ok(self.array::<1>(at)?[0])
    }

    fn u16(&self, at: usize) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.array(at)?))
    }

    fn u32(&self, at: usize) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.array(at)?))
    }

    fn u64(&self, at: usize) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.array(at)?))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a small little-endian ELF: one `PT_LOAD` segment and a
    /// `.symtab` with the given symbols.
    pub(crate) fn build(
        wide: bool,
        paddr: u64,
        bytes: &[u8],
        memsz: u64,
        symbols: &[(&str, u64, u64)],
    ) -> Vec<u8> {
        let (ehsize, phentsize, shentsize, symentsize) = if wide {
            (64, 56, 64, 24)
        } else {
            (52, 32, 40, 16)
        };
        let mut strtab = vec![0u8];
        let mut names = Vec::new();
        for (name, ..) in symbols {
            names.push(strtab.len() as u32);
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
        }
        let mut symtab = vec![0u8; symentsize]; // the null symbol
        for ((_, value, size), name) in symbols.iter().zip(&names) {
            let mut e = vec![0u8; symentsize];
            e[0..4].copy_from_slice(&name.to_le_bytes());
            if wide {
                e[8..16].copy_from_slice(&value.to_le_bytes());
                e[16..24].copy_from_slice(&size.to_le_bytes());
            } else {
                e[4..8].copy_from_slice(&(*value as u32).to_le_bytes());
                e[8..12].copy_from_slice(&(*size as u32).to_le_bytes());
            }
            symtab.extend(e);
        }

        let phoff = ehsize;
        let data_off = phoff + phentsize;
        let symtab_off = data_off + bytes.len();
        let strtab_off = symtab_off + symtab.len();
        let shoff = strtab_off + strtab.len();
        let mut out = vec![0u8; shoff + 3 * shentsize];
        out[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        out[4] = if wide { 2 } else { 1 };
        out[5] = 1;
        let put16 = |out: &mut Vec<u8>, at: usize, v: u16| {
            out[at..at + 2].copy_from_slice(&v.to_le_bytes())
        };
        let put32 = |out: &mut Vec<u8>, at: usize, v: u32| {
            out[at..at + 4].copy_from_slice(&v.to_le_bytes())
        };
        let put64 = |out: &mut Vec<u8>, at: usize, v: u64| {
            out[at..at + 8].copy_from_slice(&v.to_le_bytes())
        };
        if wide {
            put64(&mut out, 0x20, phoff as u64);
            put64(&mut out, 0x28, shoff as u64);
            put16(&mut out, 0x36, phentsize as u16);
            put16(&mut out, 0x38, 1);
            put16(&mut out, 0x3a, shentsize as u16);
            put16(&mut out, 0x3c, 3);
        } else {
            put32(&mut out, 0x1c, phoff as u32);
            put32(&mut out, 0x20, shoff as u32);
            put16(&mut out, 0x2a, phentsize as u16);
            put16(&mut out, 0x2c, 1);
            put16(&mut out, 0x2e, shentsize as u16);
            put16(&mut out, 0x30, 3);
        }
        let ph = phoff;
        put32(&mut out, ph, PT_LOAD);
        if wide {
            put64(&mut out, ph + 0x08, data_off as u64);
            put64(&mut out, ph + 0x18, paddr);
            put64(&mut out, ph + 0x20, bytes.len() as u64);
            put64(&mut out, ph + 0x28, memsz);
        } else {
            put32(&mut out, ph + 0x04, data_off as u32);
            put32(&mut out, ph + 0x0c, paddr as u32);
            put32(&mut out, ph + 0x10, bytes.len() as u32);
            put32(&mut out, ph + 0x14, memsz as u32);
        }
        out[data_off..symtab_off].copy_from_slice(bytes);
        out[symtab_off..strtab_off].copy_from_slice(&symtab);
        out[strtab_off..shoff].copy_from_slice(&strtab);
        // Section 1: .symtab (link 2), section 2: .strtab. Section 0 is null.
        let sections = [
            (SHT_SYMTAB, symtab_off, symtab.len(), 2u32, symentsize),
            (3, strtab_off, strtab.len(), 0, 0),
        ];
        for (i, (kind, off, size, link, entsize)) in sections.iter().enumerate() {
            let sh = shoff + (i + 1) * shentsize;
            put32(&mut out, sh + 4, *kind);
            if wide {
                put64(&mut out, sh + 0x18, *off as u64);
                put64(&mut out, sh + 0x20, *size as u64);
                put32(&mut out, sh + 0x28, *link);
                put64(&mut out, sh + 0x38, *entsize as u64);
            } else {
                put32(&mut out, sh + 0x10, *off as u32);
                put32(&mut out, sh + 0x14, *size as u32);
                put32(&mut out, sh + 0x18, *link);
                put32(&mut out, sh + 0x24, *entsize as u32);
            }
        }
        out
    }

    #[test]
    fn segments_and_symbols_are_read_in_both_classes() {
        for wide in [false, true] {
            let raw = build(
                wide,
                0x8000_0000,
                &[1, 2, 3, 4],
                0x100,
                &[("_SEGGER_RTT", 0x8000_1000, 72), ("tohost", 0x8000_2000, 8)],
            );
            let elf = Elf::parse(&raw).unwrap();
            assert_eq!(elf.wide, wide);
            assert_eq!(
                elf.segments,
                vec![Segment {
                    paddr: 0x8000_0000,
                    memsz: 0x100,
                    bytes: vec![1, 2, 3, 4],
                }]
            );
            let rtt = elf.symbol("_SEGGER_RTT").unwrap();
            assert_eq!((rtt.value, rtt.size), (0x8000_1000, 72));
            assert_eq!(elf.symbol("tohost").unwrap().value, 0x8000_2000);
            assert!(elf.symbol("main").is_none());
        }
    }

    #[test]
    fn a_file_that_is_not_an_elf_is_refused() {
        assert!(Elf::parse(b"hello").unwrap_err().contains("ELF magic"));
        let mut raw = build(false, 0, &[0], 1, &[]);
        raw[5] = 2;
        assert!(Elf::parse(&raw).unwrap_err().contains("little-endian"));
    }
}

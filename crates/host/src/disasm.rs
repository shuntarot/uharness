//! A RISC-V disassembler for `hio cpu disasm`.
//!
//! It covers RV32 and RV64 with I, M, A, C, F, D, Zicsr, Zifencei, Zba, Zbb,
//! Zbs, Zicond, Zicbom and the privileged instructions. The text follows GNU
//! objdump, aliases included (`li`, `mv`, `ret`, `beqz`, ...), so it reads
//! like the listings people already know. An encoding it does not know comes
//! back as `None`; the caller prints the raw value.
//!
//! A compressed instruction is first expanded to the 32-bit one it stands
//! for, then printed like it. objdump does the same.

/// One decoded instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Insn {
    /// 2 or 4 bytes.
    pub len: usize,
    /// The encoding, as read (16 bits for a compressed one).
    pub raw: u32,
    pub mnemonic: String,
    /// The operands, comma-separated. A branch or jump target is not in
    /// here: it is in `target`, and the caller prints it after these.
    pub operands: String,
    /// Where a branch or a direct jump goes.
    pub target: Option<u64>,
}

/// The integer register names of the ABI.
pub const X: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

/// The floating-point register names of the ABI.
pub const F: [&str; 32] = [
    "ft0", "ft1", "ft2", "ft3", "ft4", "ft5", "ft6", "ft7", "fs0", "fs1", "fa0", "fa1", "fa2",
    "fa3", "fa4", "fa5", "fa6", "fa7", "fs2", "fs3", "fs4", "fs5", "fs6", "fs7", "fs8", "fs9",
    "fs10", "fs11", "ft8", "ft9", "ft10", "ft11",
];

/// Decodes the instruction at the start of `bytes`, which sits at `addr`.
/// `xlen` is 32 or 64. `None` when the bytes are too few or the encoding is
/// not one this knows.
pub fn decode(bytes: &[u8], addr: u64, xlen: u32) -> Option<Insn> {
    let low = u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]);
    if low & 3 != 3 {
        let word = expand(low, xlen)?;
        let (mnemonic, operands, target) = decode32(word, addr, xlen)?;
        return Some(Insn {
            len: 2,
            raw: u32::from(low),
            mnemonic,
            operands,
            target,
        });
    }
    // 48-bit and longer encodings (low bits 11111) are not used here.
    if low & 0x1f == 0x1f {
        return None;
    }
    let word = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    let (mnemonic, operands, target) = decode32(word, addr, xlen)?;
    Some(Insn {
        len: 4,
        raw: word,
        mnemonic,
        operands,
        target,
    })
}

/// The address an `auipc` and the instruction after it reach together, as
/// in `auipc ra,0x0` + `jalr 384(ra)` (a call) or `auipc a0,0x1` +
/// `addi a0,a0,1868` (an address). `None` when they are not such a pair.
pub fn auipc_pair(first: &Insn, first_addr: u64, next: &Insn, xlen: u32) -> Option<u64> {
    if first.len != 4 || next.len != 4 || first.raw & 0x7f != 0x17 {
        return None;
    }
    let rd = bits(first.raw, 11, 7);
    let w = next.raw;
    let imm = match w & 0x7f {
        // jalr, addi, loads
        0x67 | 0x13 | 0x03 | 0x07 if bits(w, 19, 15) == rd => sext(bits(w, 31, 20), 12),
        // stores
        0x23 | 0x27 if bits(w, 19, 15) == rd => sext((bits(w, 31, 25) << 5) | bits(w, 11, 7), 12),
        _ => return None,
    };
    if rd == 0 || (w & 0x7f == 0x13 && bits(w, 14, 12) != 0) {
        return None;
    }
    let upper = sext(first.raw & 0xffff_f000, 32);
    let at = first_addr
        .wrapping_add(upper as u64)
        .wrapping_add(imm as u64);
    Some(if xlen == 32 { at & 0xffff_ffff } else { at })
}

/// How many bytes the instruction starting with these two bytes takes.
pub fn length(low: u16) -> usize {
    if low & 3 != 3 { 2 } else { 4 }
}

fn bits(w: u32, hi: u32, lo: u32) -> u32 {
    (w >> lo) & ((1 << (hi - lo + 1)) - 1)
}

fn sext(v: u32, width: u32) -> i64 {
    let shift = 64 - width;
    ((u64::from(v) << shift) as i64) >> shift
}

// --- Compressed instructions -------------------------------------------------

/// The 32-bit instruction a compressed one stands for, built from fields.
/// `c.mv` becomes `addi rd,rs,0` rather than `add rd,zero,rs`: both mean the
/// same, and objdump prints it as `mv`.
fn expand(c: u16, xlen: u32) -> Option<u32> {
    let c = u32::from(c);
    let op = c & 3;
    let f3 = bits(c, 15, 13);
    let rd = bits(c, 11, 7);
    let rs2 = bits(c, 6, 2);
    // The 3-bit register fields name x8..x15.
    let rdp = bits(c, 4, 2) + 8;
    let rs1p = bits(c, 9, 7) + 8;
    match (op, f3) {
        // c.addi4spn: addi rd', sp, nzuimm
        (0, 0) => {
            let imm = (bits(c, 12, 11) << 4)
                | (bits(c, 10, 7) << 6)
                | (bits(c, 6, 6) << 2)
                | (bits(c, 5, 5) << 3);
            (imm != 0).then(|| i_type(0x13, rdp, 0, 2, imm as i64))
        }
        // c.fld
        (0, 1) => Some(i_type(0x07, rdp, 3, rs1p, cld_imm(c) as i64)),
        // c.lw
        (0, 2) => Some(i_type(0x03, rdp, 2, rs1p, clw_imm(c) as i64)),
        // c.flw (RV32) / c.ld (RV64)
        (0, 3) if xlen == 32 => Some(i_type(0x07, rdp, 2, rs1p, clw_imm(c) as i64)),
        (0, 3) => Some(i_type(0x03, rdp, 3, rs1p, cld_imm(c) as i64)),
        // c.fsd
        (0, 5) => Some(s_type(0x27, 3, rs1p, rdp, cld_imm(c) as i64)),
        // c.sw
        (0, 6) => Some(s_type(0x23, 2, rs1p, rdp, clw_imm(c) as i64)),
        // c.fsw (RV32) / c.sd (RV64)
        (0, 7) if xlen == 32 => Some(s_type(0x27, 2, rs1p, rdp, clw_imm(c) as i64)),
        (0, 7) => Some(s_type(0x23, 3, rs1p, rdp, cld_imm(c) as i64)),
        // c.addi (c.nop when rd is 0)
        (1, 0) => Some(i_type(0x13, rd, 0, rd, ci_imm(c))),
        // c.jal (RV32) / c.addiw (RV64)
        (1, 1) if xlen == 32 => Some(j_type(1, cj_imm(c))),
        (1, 1) => (rd != 0).then(|| i_type(0x1b, rd, 0, rd, ci_imm(c))),
        // c.li
        (1, 2) => Some(i_type(0x13, rd, 0, 0, ci_imm(c))),
        // c.addi16sp / c.lui
        (1, 3) if rd == 2 => {
            let imm = (bits(c, 12, 12) << 9)
                | (bits(c, 6, 6) << 4)
                | (bits(c, 5, 5) << 6)
                | (bits(c, 4, 3) << 7)
                | (bits(c, 2, 2) << 5);
            (imm != 0).then(|| i_type(0x13, 2, 0, 2, sext(imm, 10)))
        }
        (1, 3) => {
            let imm = (bits(c, 12, 12) << 5) | bits(c, 6, 2);
            (imm != 0 && rd != 0).then(|| {
                let up = (sext(imm, 6) as u32) & 0xfffff;
                (up << 12) | (rd << 7) | 0x37
            })
        }
        // c.srli / c.srai / c.andi / c.sub ... c.addw
        (1, 4) => {
            let shamt = (bits(c, 12, 12) << 5) | bits(c, 6, 2);
            match bits(c, 11, 10) {
                0 => Some(i_type(0x13, rs1p, 5, rs1p, i64::from(shamt))),
                1 => Some(i_type(0x13, rs1p, 5, rs1p, i64::from(shamt | 0x400))),
                2 => Some(i_type(0x13, rs1p, 7, rs1p, ci_imm(c))),
                _ => {
                    let rs2p = bits(c, 4, 2) + 8;
                    let (opcode, f3, f7) = match (bits(c, 12, 12), bits(c, 6, 5)) {
                        (0, 0) => (0x33, 0, 0x20),
                        (0, 1) => (0x33, 4, 0),
                        (0, 2) => (0x33, 6, 0),
                        (0, 3) => (0x33, 7, 0),
                        (1, 0) if xlen == 64 => (0x3b, 0, 0x20),
                        (1, 1) if xlen == 64 => (0x3b, 0, 0),
                        _ => return None,
                    };
                    Some(r_type(opcode, rs1p, f3, rs1p, rs2p, f7))
                }
            }
        }
        // c.j
        (1, 5) => Some(j_type(0, cj_imm(c))),
        // c.beqz / c.bnez
        (1, 6) | (1, 7) => {
            let imm = (bits(c, 12, 12) << 8)
                | (bits(c, 11, 10) << 3)
                | (bits(c, 6, 5) << 6)
                | (bits(c, 4, 3) << 1)
                | (bits(c, 2, 2) << 5);
            Some(b_type(f3 - 6, rs1p, 0, sext(imm, 9)))
        }
        // c.slli
        (2, 0) => {
            let shamt = (bits(c, 12, 12) << 5) | bits(c, 6, 2);
            Some(i_type(0x13, rd, 1, rd, i64::from(shamt)))
        }
        // c.fldsp
        (2, 1) => Some(i_type(0x07, rd, 3, 2, cldsp_imm(c) as i64)),
        // c.lwsp
        (2, 2) => (rd != 0).then(|| i_type(0x03, rd, 2, 2, clwsp_imm(c) as i64)),
        // c.flwsp (RV32) / c.ldsp (RV64)
        (2, 3) if xlen == 32 => Some(i_type(0x07, rd, 2, 2, clwsp_imm(c) as i64)),
        (2, 3) => (rd != 0).then(|| i_type(0x03, rd, 3, 2, cldsp_imm(c) as i64)),
        // c.jr / c.mv / c.ebreak / c.jalr / c.add
        (2, 4) => match (bits(c, 12, 12), rd, rs2) {
            (0, 0, _) => None,
            (0, _, 0) => Some(i_type(0x67, 0, 0, rd, 0)),
            (0, _, _) => Some(i_type(0x13, rd, 0, rs2, 0)),
            (1, 0, 0) => Some(0x0010_0073),
            (1, _, 0) => Some(i_type(0x67, 1, 0, rd, 0)),
            (1, _, _) => Some(r_type(0x33, rd, 0, rd, rs2, 0)),
            _ => None,
        },
        // c.fsdsp
        (2, 5) => Some(s_type(0x27, 3, 2, rs2, csdsp_imm(c) as i64)),
        // c.swsp
        (2, 6) => Some(s_type(0x23, 2, 2, rs2, cswsp_imm(c) as i64)),
        // c.fswsp (RV32) / c.sdsp (RV64)
        (2, 7) if xlen == 32 => Some(s_type(0x27, 2, 2, rs2, cswsp_imm(c) as i64)),
        (2, 7) => Some(s_type(0x23, 3, 2, rs2, csdsp_imm(c) as i64)),
        _ => None,
    }
}

fn ci_imm(c: u32) -> i64 {
    sext((bits(c, 12, 12) << 5) | bits(c, 6, 2), 6)
}

fn cj_imm(c: u32) -> i64 {
    let imm = (bits(c, 12, 12) << 11)
        | (bits(c, 11, 11) << 4)
        | (bits(c, 10, 9) << 8)
        | (bits(c, 8, 8) << 10)
        | (bits(c, 7, 7) << 6)
        | (bits(c, 6, 6) << 7)
        | (bits(c, 5, 3) << 1)
        | (bits(c, 2, 2) << 5);
    sext(imm, 12)
}

fn clw_imm(c: u32) -> u32 {
    (bits(c, 12, 10) << 3) | (bits(c, 6, 6) << 2) | (bits(c, 5, 5) << 6)
}

fn cld_imm(c: u32) -> u32 {
    (bits(c, 12, 10) << 3) | (bits(c, 6, 5) << 6)
}

fn clwsp_imm(c: u32) -> u32 {
    (bits(c, 12, 12) << 5) | (bits(c, 6, 4) << 2) | (bits(c, 3, 2) << 6)
}

fn cldsp_imm(c: u32) -> u32 {
    (bits(c, 12, 12) << 5) | (bits(c, 6, 5) << 3) | (bits(c, 4, 2) << 6)
}

fn cswsp_imm(c: u32) -> u32 {
    (bits(c, 12, 9) << 2) | (bits(c, 8, 7) << 6)
}

fn csdsp_imm(c: u32) -> u32 {
    (bits(c, 12, 10) << 3) | (bits(c, 9, 7) << 6)
}

fn i_type(opcode: u32, rd: u32, f3: u32, rs1: u32, imm: i64) -> u32 {
    ((imm as u32 & 0xfff) << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | opcode
}

fn s_type(opcode: u32, f3: u32, rs1: u32, rs2: u32, imm: i64) -> u32 {
    let imm = imm as u32;
    (bits(imm, 11, 5) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (f3 << 12)
        | (bits(imm, 4, 0) << 7)
        | opcode
}

fn r_type(opcode: u32, rd: u32, f3: u32, rs1: u32, rs2: u32, f7: u32) -> u32 {
    (f7 << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | opcode
}

fn b_type(f3: u32, rs1: u32, rs2: u32, imm: i64) -> u32 {
    let imm = imm as u32;
    (bits(imm, 12, 12) << 31)
        | (bits(imm, 10, 5) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (f3 << 12)
        | (bits(imm, 4, 1) << 8)
        | (bits(imm, 11, 11) << 7)
        | 0x63
}

fn j_type(rd: u32, imm: i64) -> u32 {
    let imm = imm as u32;
    (bits(imm, 20, 20) << 31)
        | (bits(imm, 10, 1) << 21)
        | (bits(imm, 11, 11) << 20)
        | (bits(imm, 19, 12) << 12)
        | (rd << 7)
        | 0x6f
}

// --- 32-bit instructions -----------------------------------------------------

type Text = (String, String, Option<u64>);

fn t(mnemonic: &str, operands: String) -> Option<Text> {
    Some((mnemonic.to_string(), operands, None))
}

fn mem(base: u32, off: i64) -> String {
    format!("{off}({})", X[base as usize])
}

const RM: [&str; 8] = ["rne", "rtz", "rdn", "rup", "rmm", "", "", "dyn"];

/// `,rtz` and so on, or nothing for the dynamic mode. A reserved mode
/// makes the instruction unknown.
fn rm(f3: u32) -> Option<String> {
    match f3 {
        7 => Some(String::new()),
        0..=4 => Some(format!(",{}", RM[f3 as usize])),
        _ => None,
    }
}

/// The same for a conversion that never rounds (to a wider float, or from a
/// 32-bit integer to a double): objdump leaves out `rne` there too.
fn rm_exact(f3: u32) -> Option<String> {
    if f3 == 0 { Some(String::new()) } else { rm(f3) }
}

fn decode32(w: u32, addr: u64, xlen: u32) -> Option<Text> {
    let opcode = w & 0x7f;
    let rd = bits(w, 11, 7);
    let f3 = bits(w, 14, 12);
    let rs1 = bits(w, 19, 15);
    let rs2 = bits(w, 24, 20);
    let (xd, x1, x2) = (X[rd as usize], X[rs1 as usize], X[rs2 as usize]);
    let imm_i = sext(bits(w, 31, 20), 12);
    let imm_s = sext((bits(w, 31, 25) << 5) | rd, 12);
    let wide = xlen == 64;
    match opcode {
        0x37 => t("lui", format!("{xd},{:#x}", bits(w, 31, 12))),
        0x17 => t("auipc", format!("{xd},{:#x}", bits(w, 31, 12))),
        0x6f => {
            let imm = (bits(w, 31, 31) << 20)
                | (bits(w, 30, 21) << 1)
                | (bits(w, 20, 20) << 11)
                | (bits(w, 19, 12) << 12);
            let target = addr.wrapping_add(sext(imm, 21) as u64);
            let (m, ops) = match rd {
                0 => ("j", String::new()),
                1 => ("jal", String::new()),
                _ => ("jal", format!("{xd},")),
            };
            Some((m.to_string(), ops, Some(target)))
        }
        0x67 if f3 == 0 => match (rd, rs1, imm_i) {
            (0, 1, 0) => t("ret", String::new()),
            (0, _, 0) => t("jr", x1.to_string()),
            (0, _, _) => t("jr", mem(rs1, imm_i)),
            (1, _, 0) => t("jalr", x1.to_string()),
            (1, _, _) => t("jalr", mem(rs1, imm_i)),
            (_, _, 0) => t("jalr", format!("{xd},{x1}")),
            _ => t("jalr", format!("{xd},{}", mem(rs1, imm_i))),
        },
        0x63 => {
            let imm = (bits(w, 31, 31) << 12)
                | (bits(w, 30, 25) << 5)
                | (bits(w, 11, 8) << 1)
                | (bits(w, 7, 7) << 11);
            let target = addr.wrapping_add(sext(imm, 13) as u64);
            let name = ["beq", "bne", "", "", "blt", "bge", "bltu", "bgeu"][f3 as usize];
            if name.is_empty() {
                return None;
            }
            let (m, ops) = match (f3, rs1, rs2) {
                (0, _, 0) => ("beqz".to_string(), format!("{x1},")),
                (1, _, 0) => ("bnez".to_string(), format!("{x1},")),
                (4, _, 0) => ("bltz".to_string(), format!("{x1},")),
                (5, _, 0) => ("bgez".to_string(), format!("{x1},")),
                (4, 0, _) => ("bgtz".to_string(), format!("{x2},")),
                (5, 0, _) => ("blez".to_string(), format!("{x2},")),
                _ => (name.to_string(), format!("{x1},{x2},")),
            };
            Some((m, ops, Some(target)))
        }
        0x03 => {
            let name = match (f3, wide) {
                (0, _) => "lb",
                (1, _) => "lh",
                (2, _) => "lw",
                (3, true) => "ld",
                (4, _) => "lbu",
                (5, _) => "lhu",
                (6, true) => "lwu",
                _ => return None,
            };
            t(name, format!("{xd},{}", mem(rs1, imm_i)))
        }
        0x23 => {
            let name = match (f3, wide) {
                (0, _) => "sb",
                (1, _) => "sh",
                (2, _) => "sw",
                (3, true) => "sd",
                _ => return None,
            };
            t(name, format!("{x2},{}", mem(rs1, imm_s)))
        }
        0x13 => op_imm(w, xlen),
        0x1b if wide => op_imm32(w),
        0x33 => op(w, xlen),
        0x3b if wide => op32(w),
        0x0f => misc_mem(w),
        0x73 => system(w),
        0x2f => amo(w, wide),
        0x07 | 0x27 => {
            let name = match (opcode, f3) {
                (0x07, 2) => "flw",
                (0x07, 3) => "fld",
                (0x27, 2) => "fsw",
                (0x27, 3) => "fsd",
                _ => return None,
            };
            if opcode == 0x07 {
                t(name, format!("{},{}", F[rd as usize], mem(rs1, imm_i)))
            } else {
                t(name, format!("{},{}", F[rs2 as usize], mem(rs1, imm_s)))
            }
        }
        0x43 | 0x47 | 0x4b | 0x4f => {
            let fmt = match bits(w, 26, 25) {
                0 => "s",
                1 => "d",
                _ => return None,
            };
            let base = match opcode {
                0x43 => "fmadd",
                0x47 => "fmsub",
                0x4b => "fnmsub",
                _ => "fnmadd",
            };
            let rs3 = bits(w, 31, 27);
            t(
                &format!("{base}.{fmt}"),
                format!(
                    "{},{},{},{}{}",
                    F[rd as usize],
                    F[rs1 as usize],
                    F[rs2 as usize],
                    F[rs3 as usize],
                    rm(f3)?
                ),
            )
        }
        0x53 => op_fp(w, wide),
        _ => None,
    }
}

fn op_imm(w: u32, xlen: u32) -> Option<Text> {
    let rd = bits(w, 11, 7);
    let f3 = bits(w, 14, 12);
    let rs1 = bits(w, 19, 15);
    let (xd, x1) = (X[rd as usize], X[rs1 as usize]);
    let imm = sext(bits(w, 31, 20), 12);
    let shamt_bits = if xlen == 64 { 6 } else { 5 };
    let shamt = bits(w, 19 + shamt_bits, 20);
    match f3 {
        0 => match (rd, rs1, imm) {
            (0, 0, 0) => t("nop", String::new()),
            (_, 0, _) => t("li", format!("{xd},{imm}")),
            (_, _, 0) => t("mv", format!("{xd},{x1}")),
            _ => t("addi", format!("{xd},{x1},{imm}")),
        },
        2 => t("slti", format!("{xd},{x1},{imm}")),
        3 if imm == 1 => t("seqz", format!("{xd},{x1}")),
        3 => t("sltiu", format!("{xd},{x1},{imm}")),
        4 if imm == -1 => t("not", format!("{xd},{x1}")),
        4 => t("xori", format!("{xd},{x1},{imm}")),
        6 => t("ori", format!("{xd},{x1},{imm}")),
        7 if imm == 255 => t("zext.b", format!("{xd},{x1}")),
        7 => t("andi", format!("{xd},{x1},{imm}")),
        1 => {
            let f12 = bits(w, 31, 20);
            match f12 {
                0x600 => return t("clz", format!("{xd},{x1}")),
                0x601 => return t("ctz", format!("{xd},{x1}")),
                0x602 => return t("cpop", format!("{xd},{x1}")),
                0x604 => return t("sext.b", format!("{xd},{x1}")),
                0x605 => return t("sext.h", format!("{xd},{x1}")),
                _ => {}
            }
            let name = match bits(w, 31, 26) {
                0x00 => "slli",
                0x0a => "bseti",
                0x12 => "bclri",
                0x1a => "binvi",
                _ => return None,
            };
            if xlen == 32 && bits(w, 25, 25) != 0 {
                return None;
            }
            t(name, format!("{xd},{x1},{shamt:#x}"))
        }
        5 => {
            let f12 = bits(w, 31, 20);
            if f12 == 0x287 {
                return t("orc.b", format!("{xd},{x1}"));
            }
            if (xlen == 32 && f12 == 0x698) || (xlen == 64 && f12 == 0x6b8) {
                return t("rev8", format!("{xd},{x1}"));
            }
            let name = match bits(w, 31, 26) {
                0x00 => "srli",
                0x10 => "srai",
                0x18 => "rori",
                0x12 => "bexti",
                _ => return None,
            };
            if xlen == 32 && bits(w, 25, 25) != 0 {
                return None;
            }
            t(name, format!("{xd},{x1},{shamt:#x}"))
        }
        _ => None,
    }
}

fn op_imm32(w: u32) -> Option<Text> {
    let rd = bits(w, 11, 7);
    let f3 = bits(w, 14, 12);
    let rs1 = bits(w, 19, 15);
    let (xd, x1) = (X[rd as usize], X[rs1 as usize]);
    let imm = sext(bits(w, 31, 20), 12);
    let shamt = bits(w, 24, 20);
    match (f3, bits(w, 31, 25)) {
        (0, _) if imm == 0 => t("sext.w", format!("{xd},{x1}")),
        (0, _) => t("addiw", format!("{xd},{x1},{imm}")),
        (1, 0x00) => t("slliw", format!("{xd},{x1},{shamt:#x}")),
        (1, 0x30) => match shamt {
            0 => t("clzw", format!("{xd},{x1}")),
            1 => t("ctzw", format!("{xd},{x1}")),
            2 => t("cpopw", format!("{xd},{x1}")),
            _ => None,
        },
        (1, 0x04) | (1, 0x05) => t("slli.uw", format!("{xd},{x1},{:#x}", bits(w, 25, 20))),
        (5, 0x00) => t("srliw", format!("{xd},{x1},{shamt:#x}")),
        (5, 0x20) => t("sraiw", format!("{xd},{x1},{shamt:#x}")),
        (5, 0x30) => t("roriw", format!("{xd},{x1},{shamt:#x}")),
        _ => None,
    }
}

fn op(w: u32, xlen: u32) -> Option<Text> {
    let rd = bits(w, 11, 7);
    let f3 = bits(w, 14, 12);
    let rs1 = bits(w, 19, 15);
    let rs2 = bits(w, 24, 20);
    let f7 = bits(w, 31, 25);
    let (xd, x1, x2) = (X[rd as usize], X[rs1 as usize], X[rs2 as usize]);
    let three = || format!("{xd},{x1},{x2}");
    let name = match (f7, f3) {
        (0x00, 0) => "add",
        (0x20, 0) if rs1 == 0 => return t("neg", format!("{xd},{x2}")),
        (0x20, 0) => "sub",
        (0x00, 1) => "sll",
        (0x00, 2) if rs2 == 0 => return t("sltz", format!("{xd},{x1}")),
        (0x00, 2) if rs1 == 0 => return t("sgtz", format!("{xd},{x2}")),
        (0x00, 2) => "slt",
        (0x00, 3) if rs1 == 0 => return t("snez", format!("{xd},{x2}")),
        (0x00, 3) => "sltu",
        (0x00, 4) => "xor",
        (0x00, 5) => "srl",
        (0x20, 5) => "sra",
        (0x00, 6) => "or",
        (0x00, 7) => "and",
        (0x01, _) => [
            "mul", "mulh", "mulhsu", "mulhu", "div", "divu", "rem", "remu",
        ][f3 as usize],
        (0x20, 7) => "andn",
        (0x20, 6) => "orn",
        (0x20, 4) => "xnor",
        (0x05, 1) => "clmul",
        (0x05, 2) => "clmulr",
        (0x05, 3) => "clmulh",
        (0x05, 4) => "min",
        (0x05, 5) => "minu",
        (0x05, 6) => "max",
        (0x05, 7) => "maxu",
        (0x30, 1) => "rol",
        (0x30, 5) => "ror",
        (0x10, 2) => "sh1add",
        (0x10, 4) => "sh2add",
        (0x10, 6) => "sh3add",
        (0x24, 1) => "bclr",
        (0x24, 5) => "bext",
        (0x34, 1) => "binv",
        (0x14, 1) => "bset",
        (0x07, 5) => "czero.eqz",
        (0x07, 7) => "czero.nez",
        (0x04, 4) if rs2 == 0 && xlen == 32 => return t("zext.h", format!("{xd},{x1}")),
        _ => return None,
    };
    t(name, three())
}

fn op32(w: u32) -> Option<Text> {
    let rd = bits(w, 11, 7);
    let f3 = bits(w, 14, 12);
    let rs1 = bits(w, 19, 15);
    let rs2 = bits(w, 24, 20);
    let f7 = bits(w, 31, 25);
    let (xd, x1, x2) = (X[rd as usize], X[rs1 as usize], X[rs2 as usize]);
    let name = match (f7, f3) {
        (0x00, 0) => "addw",
        (0x20, 0) if rs1 == 0 => return t("negw", format!("{xd},{x2}")),
        (0x20, 0) => "subw",
        (0x00, 1) => "sllw",
        (0x00, 5) => "srlw",
        (0x20, 5) => "sraw",
        (0x01, 0) => "mulw",
        (0x01, 4) => "divw",
        (0x01, 5) => "divuw",
        (0x01, 6) => "remw",
        (0x01, 7) => "remuw",
        (0x30, 1) => "rolw",
        (0x30, 5) => "rorw",
        (0x04, 0) if rs2 == 0 => return t("zext.w", format!("{xd},{x1}")),
        (0x04, 0) => "add.uw",
        (0x04, 4) if rs2 == 0 => return t("zext.h", format!("{xd},{x1}")),
        (0x10, 2) => "sh1add.uw",
        (0x10, 4) => "sh2add.uw",
        (0x10, 6) => "sh3add.uw",
        _ => return None,
    };
    t(name, format!("{xd},{x1},{x2}"))
}

fn misc_mem(w: u32) -> Option<Text> {
    let f3 = bits(w, 14, 12);
    let rs1 = bits(w, 19, 15);
    match f3 {
        0 => {
            if w == 0x8330_000f {
                return t("fence.tso", String::new());
            }
            let set = |v: u32| -> String {
                let s: String = [(8, 'i'), (4, 'o'), (2, 'r'), (1, 'w')]
                    .iter()
                    .filter(|(b, _)| v & b != 0)
                    .map(|(_, c)| *c)
                    .collect();
                if s.is_empty() { "0".into() } else { s }
            };
            let (pred, succ) = (bits(w, 27, 24), bits(w, 23, 20));
            if pred == 0xf && succ == 0xf {
                t("fence", String::new())
            } else {
                t("fence", format!("{},{}", set(pred), set(succ)))
            }
        }
        1 => t("fence.i", String::new()),
        2 => {
            let name = match bits(w, 31, 20) {
                0 => "cbo.inval",
                1 => "cbo.clean",
                2 => "cbo.flush",
                4 => "cbo.zero",
                _ => return None,
            };
            t(name, format!("0({})", X[rs1 as usize]))
        }
        _ => None,
    }
}

fn system(w: u32) -> Option<Text> {
    let rd = bits(w, 11, 7);
    let f3 = bits(w, 14, 12);
    let rs1 = bits(w, 19, 15);
    let csr = bits(w, 31, 20);
    let (xd, x1) = (X[rd as usize], X[rs1 as usize]);
    // `csrrw zero,cycle,zero`: a write to a read-only CSR, which is how
    // the toolchain spells an always-illegal instruction.
    if w == 0xc000_1073 {
        return t("unimp", String::new());
    }
    if f3 == 0 {
        if bits(w, 31, 25) == 0x09 && rd == 0 {
            let rs2 = bits(w, 24, 20);
            return match (rs1, rs2) {
                (0, 0) => t("sfence.vma", String::new()),
                (_, 0) => t("sfence.vma", x1.to_string()),
                _ => t("sfence.vma", format!("{x1},{}", X[rs2 as usize])),
            };
        }
        if rd != 0 || rs1 != 0 {
            return None;
        }
        return match csr {
            0x000 => t("ecall", String::new()),
            0x001 => t("ebreak", String::new()),
            0x102 => t("sret", String::new()),
            0x302 => t("mret", String::new()),
            0x7b2 => t("dret", String::new()),
            0x105 => t("wfi", String::new()),
            _ => None,
        };
    }
    let name = csr_name(csr);
    // The forms with a register source, then with a 5-bit immediate.
    match f3 {
        1..=3 => {
            let base = ["", "csrrw", "csrrs", "csrrc"][f3 as usize];
            if f3 == 2 && rs1 == 0 {
                if let Some(alias) = read_alias(csr) {
                    return t(alias, xd.to_string());
                }
                return t("csrr", format!("{xd},{name}"));
            }
            if let Some(alias) = fp_alias(csr, f3, rd != 0) {
                return match rd {
                    0 => t(alias, x1.to_string()),
                    _ => t(alias, format!("{xd},{x1}")),
                };
            }
            if rd == 0 {
                let short = ["", "csrw", "csrs", "csrc"][f3 as usize];
                return t(short, format!("{name},{x1}"));
            }
            t(base, format!("{xd},{name},{x1}"))
        }
        5..=7 => {
            let base = ["", "", "", "", "", "csrrwi", "csrrsi", "csrrci"][f3 as usize];
            if f3 == 5 && (csr == 2 || csr == 1) {
                let alias = if csr == 2 { "fsrmi" } else { "fsflagsi" };
                return match rd {
                    0 => t(alias, format!("{rs1}")),
                    _ => t(alias, format!("{xd},{rs1}")),
                };
            }
            if rd == 0 {
                let short = ["", "", "", "", "", "csrwi", "csrsi", "csrci"][f3 as usize];
                return t(short, format!("{name},{rs1}"));
            }
            t(base, format!("{xd},{name},{rs1}"))
        }
        _ => None,
    }
}

/// objdump's names for reading the counters and the FP status.
fn read_alias(csr: u32) -> Option<&'static str> {
    Some(match csr {
        0x001 => "frflags",
        0x002 => "frrm",
        0x003 => "frcsr",
        0xc00 => "rdcycle",
        0xc01 => "rdtime",
        0xc02 => "rdinstret",
        0xc80 => "rdcycleh",
        0xc81 => "rdtimeh",
        0xc82 => "rdinstreth",
        _ => return None,
    })
}

/// objdump's names for writing the FP status (`csrrw` only).
fn fp_alias(csr: u32, f3: u32, _has_rd: bool) -> Option<&'static str> {
    if f3 != 1 {
        return None;
    }
    Some(match csr {
        0x001 => "fsflags",
        0x002 => "fsrm",
        0x003 => "fscsr",
        _ => return None,
    })
}

/// The name of a CSR, or its number.
pub fn csr_name(csr: u32) -> String {
    let name = match csr {
        0x001 => "fflags",
        0x002 => "frm",
        0x003 => "fcsr",
        0x100 => "sstatus",
        0x104 => "sie",
        0x105 => "stvec",
        0x106 => "scounteren",
        0x10a => "senvcfg",
        0x140 => "sscratch",
        0x141 => "sepc",
        0x142 => "scause",
        0x143 => "stval",
        0x144 => "sip",
        0x180 => "satp",
        0x300 => "mstatus",
        0x301 => "misa",
        0x302 => "medeleg",
        0x303 => "mideleg",
        0x304 => "mie",
        0x305 => "mtvec",
        0x306 => "mcounteren",
        0x30a => "menvcfg",
        0x310 => "mstatush",
        0x31a => "menvcfgh",
        0x320 => "mcountinhibit",
        0x340 => "mscratch",
        0x341 => "mepc",
        0x342 => "mcause",
        0x343 => "mtval",
        0x344 => "mip",
        0x34a => "mtinst",
        0x34b => "mtval2",
        0x7a0 => "tselect",
        0x7a1 => "tdata1",
        0x7a2 => "tdata2",
        0x7a3 => "tdata3",
        0x7a4 => "tinfo",
        0x7a5 => "tcontrol",
        0x7a8 => "mcontext",
        0x7b0 => "dcsr",
        0x7b1 => "dpc",
        0x7b2 => "dscratch0",
        0x7b3 => "dscratch1",
        0xb00 => "mcycle",
        0xb02 => "minstret",
        0xb80 => "mcycleh",
        0xb82 => "minstreth",
        0xc00 => "cycle",
        0xc01 => "time",
        0xc02 => "instret",
        0xc80 => "cycleh",
        0xc81 => "timeh",
        0xc82 => "instreth",
        0xf11 => "mvendorid",
        0xf12 => "marchid",
        0xf13 => "mimpid",
        0xf14 => "mhartid",
        0xf15 => "mconfigptr",
        0x3a0..=0x3af => return format!("pmpcfg{}", csr - 0x3a0),
        0x3b0..=0x3ef => return format!("pmpaddr{}", csr - 0x3b0),
        0x323..=0x33f => return format!("mhpmevent{}", csr - 0x320),
        0xb03..=0xb1f => return format!("mhpmcounter{}", csr - 0xb00),
        0xb83..=0xb9f => return format!("mhpmcounter{}h", csr - 0xb80),
        0xc03..=0xc1f => return format!("hpmcounter{}", csr - 0xc00),
        0xc83..=0xc9f => return format!("hpmcounter{}h", csr - 0xc80),
        _ => return format!("{csr:#x}"),
    };
    name.to_string()
}

fn amo(w: u32, wide: bool) -> Option<Text> {
    let rd = bits(w, 11, 7);
    let f3 = bits(w, 14, 12);
    let rs1 = bits(w, 19, 15);
    let rs2 = bits(w, 24, 20);
    let size = match (f3, wide) {
        (2, _) => "w",
        (3, true) => "d",
        _ => return None,
    };
    let order = match bits(w, 26, 25) {
        0 => "",
        1 => ".rl",
        2 => ".aq",
        _ => ".aqrl",
    };
    let base = match bits(w, 31, 27) {
        0x02 if rs2 == 0 => {
            return t(
                &format!("lr.{size}{order}"),
                format!("{},({})", X[rd as usize], X[rs1 as usize]),
            );
        }
        0x02 => return None,
        0x03 => "sc",
        0x01 => "amoswap",
        0x00 => "amoadd",
        0x04 => "amoxor",
        0x0c => "amoand",
        0x08 => "amoor",
        0x10 => "amomin",
        0x14 => "amomax",
        0x18 => "amominu",
        0x1c => "amomaxu",
        _ => return None,
    };
    t(
        &format!("{base}.{size}{order}"),
        format!(
            "{},{},({})",
            X[rd as usize], X[rs2 as usize], X[rs1 as usize]
        ),
    )
}

fn op_fp(w: u32, wide: bool) -> Option<Text> {
    let rd = bits(w, 11, 7);
    let f3 = bits(w, 14, 12);
    let rs1 = bits(w, 19, 15);
    let rs2 = bits(w, 24, 20);
    let f7 = bits(w, 31, 25);
    let fmt = match f7 & 3 {
        0 => "s",
        1 => "d",
        _ => return None,
    };
    let (fd, f1, f2) = (F[rd as usize], F[rs1 as usize], F[rs2 as usize]);
    let (xd, x1) = (X[rd as usize], X[rs1 as usize]);
    let ints = |n: u32| -> Option<&'static str> {
        match (n, wide) {
            (0, _) => Some("w"),
            (1, _) => Some("wu"),
            (2, true) => Some("l"),
            (3, true) => Some("lu"),
            _ => None,
        }
    };
    match f7 >> 2 {
        0x00..=0x03 => {
            let op = ["fadd", "fsub", "fmul", "fdiv"][(f7 >> 2) as usize];
            t(&format!("{op}.{fmt}"), format!("{fd},{f1},{f2}{}", rm(f3)?))
        }
        0x0b if rs2 == 0 => t(&format!("fsqrt.{fmt}"), format!("{fd},{f1}{}", rm(f3)?)),
        0x04 => {
            let (alias, base) = match f3 {
                0 => ("fmv", "fsgnj"),
                1 => ("fneg", "fsgnjn"),
                2 => ("fabs", "fsgnjx"),
                _ => return None,
            };
            if rs1 == rs2 {
                t(&format!("{alias}.{fmt}"), format!("{fd},{f1}"))
            } else {
                t(&format!("{base}.{fmt}"), format!("{fd},{f1},{f2}"))
            }
        }
        0x05 => {
            let op = match f3 {
                0 => "fmin",
                1 => "fmax",
                _ => return None,
            };
            t(&format!("{op}.{fmt}"), format!("{fd},{f1},{f2}"))
        }
        0x08 => match (fmt, rs2) {
            ("s", 1) => t("fcvt.s.d", format!("{fd},{f1}{}", rm(f3)?)),
            ("d", 0) => t("fcvt.d.s", format!("{fd},{f1}{}", rm_exact(f3)?)),
            _ => None,
        },
        0x14 => {
            let op = match f3 {
                0 => "fle",
                1 => "flt",
                2 => "feq",
                _ => return None,
            };
            t(&format!("{op}.{fmt}"), format!("{xd},{f1},{f2}"))
        }
        0x18 => t(
            &format!("fcvt.{}.{fmt}", ints(rs2)?),
            format!("{xd},{f1}{}", rm(f3)?),
        ),
        0x1a => {
            // An int32 always fits in a double.
            let exact = fmt == "d" && rs2 < 2;
            let mode = if exact { rm_exact(f3)? } else { rm(f3)? };
            t(
                &format!("fcvt.{fmt}.{}", ints(rs2)?),
                format!("{fd},{x1}{mode}"),
            )
        }
        0x1c if rs2 == 0 => match (f3, fmt) {
            (0, "s") => t("fmv.x.w", format!("{xd},{f1}")),
            (0, "d") if wide => t("fmv.x.d", format!("{xd},{f1}")),
            (1, _) => t(&format!("fclass.{fmt}"), format!("{xd},{f1}")),
            _ => None,
        },
        0x1e if rs2 == 0 && f3 == 0 => match fmt {
            "s" => t("fmv.w.x", format!("{fd},{x1}")),
            _ if wide => t("fmv.d.x", format!("{fd},{x1}")),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(hex: u32, addr: u64, xlen: u32) -> String {
        let bytes = hex.to_le_bytes();
        let i = decode(&bytes, addr, xlen).expect("decodes");
        let mut s = format!("{} {}", i.mnemonic, i.operands);
        if let Some(t) = i.target {
            s.push_str(&format!("{t:x}"));
        }
        s.trim_end().to_string()
    }

    /// Every line of `disasm_objdump.txt` is `xlen addr hex mnemonic
    /// operands`, cut from GNU objdump's listing of riscv-tests and of
    /// firmware for an RV32 and an RV64 core. A few encodings per mnemonic.
    #[test]
    fn it_reads_like_objdump() {
        let mut wrong = Vec::new();
        let cases = include_str!("disasm_objdump.txt");
        for line in cases.lines() {
            let mut p = line.splitn(5, ' ');
            let xlen: u32 = p.next().unwrap().parse().unwrap();
            let addr = u64::from_str_radix(p.next().unwrap(), 16).unwrap();
            let hex = p.next().unwrap();
            let want = format!("{} {}", p.next().unwrap(), p.next().unwrap_or(""));
            let raw = u32::from_str_radix(hex, 16).unwrap();
            let bytes = raw.to_le_bytes();
            let got = match decode(&bytes[..hex.len() / 2], addr, xlen) {
                Some(i) => {
                    let mut s = format!("{} {}", i.mnemonic, i.operands);
                    if let Some(t) = i.target {
                        s.push_str(&format!("{t:x}"));
                    }
                    s
                }
                None => "(none)".to_string(),
            };
            if got.trim_end() != want.trim_end() {
                wrong.push(format!("{line}\n    got {got}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {} differ:\n{}",
            wrong.len(),
            cases.lines().count(),
            wrong
                .iter()
                .take(60)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn a_few_by_hand() {
        assert_eq!(one(0x0000_0013, 0, 32), "nop");
        assert_eq!(one(0x0010_0513, 0, 32), "li a0,1");
        assert_eq!(one(0x0000_8067, 0, 32), "ret");
        assert_eq!(one(0x8082, 0, 32), "ret");
        assert_eq!(one(0x4505, 0, 32), "li a0,1");
        assert_eq!(one(0x3002_9073, 0, 32), "csrw mstatus,t0");
        assert_eq!(one(0x0000_006f, 0x8000_0000, 32), "j 80000000");
    }

    #[test]
    fn an_auipc_and_the_next_instruction_make_one_address() {
        let at = |raw: u32, addr| decode(&raw.to_le_bytes(), addr, 64).unwrap();
        // auipc ra,0x0 ; jalr 384(ra)
        let call = auipc_pair(&at(0x0000_0097, 0x228), 0x228, &at(0x1800_80e7, 0x22c), 64);
        assert_eq!(call, Some(0x228 + 384));
        // auipc a0,0x1 ; addi a0,a0,1868
        let addr = auipc_pair(&at(0x0000_1517, 0x21c), 0x21c, &at(0x74c5_0513, 0x220), 64);
        assert_eq!(addr, Some(0x21c + 0x1000 + 1868));
        // A different register is not a pair.
        assert_eq!(
            auipc_pair(&at(0x0000_1517, 0), 0, &at(0x0010_0593, 4), 64),
            None
        );
    }
}

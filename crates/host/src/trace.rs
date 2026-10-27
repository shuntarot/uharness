//! Reading a trace ring (`backing = "trace"`): which entries hold records,
//! and how a record prints.
//!
//! A record is a run of bits, lowest field first, as `regs.json` lays it
//! out. The first field is `cycle`, the cycles since the start, which the
//! harness adds. A trace with `format = "rvfi"` prints as instructions.

use hns_regs::Trace;

use crate::disasm;

/// The entries that hold records, oldest first.
///
/// `count` is the number of records written since the start. It wraps at
/// 2^32, so `wrapped` tells a full ring from a short one. `depth` is a power
/// of two.
pub fn entries(count: u32, wrapped: bool, depth: u32) -> Vec<u32> {
    if wrapped {
        let oldest = count % depth;
        (0..depth).map(|i| (oldest + i) % depth).collect()
    } else {
        (0..count.min(depth)).collect()
    }
}

/// The low 64 bits of the field `[lsb, lsb + width)` of a record.
pub fn value(words: &[u32], lsb: usize, width: usize) -> u64 {
    let mut out = 0u64;
    for i in 0..width.min(64) {
        let bit = lsb + i;
        let word = words.get(bit / 32).copied().unwrap_or(0);
        out |= u64::from((word >> (bit % 32)) & 1) << i;
    }
    out
}

/// A field in hex, with as many digits as its width needs, of any width.
pub fn hex(words: &[u32], lsb: usize, width: usize) -> String {
    let digits = width.div_ceil(4).max(1);
    let text: String = (0..digits)
        .rev()
        .map(|d| {
            let low = d * 4;
            let nibble = value(words, lsb + low, 4.min(width - low));
            char::from_digit(nibble as u32, 16).expect("a nibble")
        })
        .collect();
    format!("0x{text}")
}

/// Prints records one line each. It remembers the previous cycle, so that
/// each line shows the cycles since the record before it.
pub struct Printer<'a> {
    trace: &'a Trace,
    last_cycle: Option<u32>,
}

/// The RVFI fields a line shows in its own columns. The others follow as
/// `name=value`.
const RVFI_SHOWN: &[&str] = &[
    "cycle",
    "order",
    "insn",
    "pc_rdata",
    "pc_wdata",
    "mode",
    "ixl",
    "trap",
    "intr",
    "halt",
    "rd_addr",
    "rd_wdata",
    "rs1_addr",
    "rs2_addr",
    "rs1_rdata",
    "rs2_rdata",
    "mem_addr",
    "mem_rmask",
    "mem_wmask",
    "mem_rdata",
    "mem_wdata",
    "mem_fault",
    "mem_fault_rmask",
    "mem_fault_wmask",
];

impl<'a> Printer<'a> {
    pub fn new(trace: &'a Trace) -> Printer<'a> {
        Printer {
            trace,
            last_cycle: None,
        }
    }

    fn get(&self, words: &[u32], name: &str) -> Option<u64> {
        self.trace
            .field(name)
            .map(|field| value(words, field.lsb, field.width))
    }

    /// One record, without a line end.
    pub fn line(&mut self, words: &[u32]) -> String {
        let cycle = self.get(words, "cycle").unwrap_or(0) as u32;
        // Wrapping: the count itself wraps at 2^32.
        let delta = match self.last_cycle {
            Some(last) => format!("+{}", cycle.wrapping_sub(last)),
            None => String::new(),
        };
        self.last_cycle = Some(cycle);
        let head = format!("{cycle:>10} {delta:>6}");
        if self.trace.format.as_deref() == Some("rvfi") {
            format!("{head}  {}", self.rvfi(words))
        } else {
            let fields = self
                .trace
                .fields
                .iter()
                .filter(|field| field.name != "cycle")
                .map(|field| format!("{}={}", field.name, hex(words, field.lsb, field.width)))
                .collect::<Vec<_>>()
                .join("  ");
            format!("{head}  {fields}")
        }
    }

    fn rvfi(&self, words: &[u32]) -> String {
        let xlen = self.trace.field("pc_rdata").map_or(32, |f| f.width);
        let digits = xlen / 4;
        let pc = self.get(words, "pc_rdata").unwrap_or(0);
        let insn = self.get(words, "insn").unwrap_or(0) as u32;
        let mode = match self.get(words, "mode") {
            Some(0) => "U ",
            Some(1) => "S ",
            Some(3) => "M ",
            Some(_) => "? ",
            None => "",
        };
        let (raw, asm) = match disasm::decode(&insn.to_le_bytes(), pc, xlen as u32) {
            Some(i) => {
                let raw = if i.len == 2 {
                    format!("{:04x}    ", i.raw)
                } else {
                    format!("{:08x}", i.raw)
                };
                let target = i.target.map(|t| format!("{t:#x}")).unwrap_or_default();
                (raw, format!("{:<8}{}{target}", i.mnemonic, i.operands))
            }
            None => (format!("{insn:08x}"), format!(".word   {insn:#010x}")),
        };

        let mut effects = Vec::new();
        if self.get(words, "intr") == Some(1) {
            effects.push("intr".to_string());
        }
        if self.get(words, "trap") == Some(1) {
            effects.push("trap".to_string());
        }
        if let Some(rd) = self.get(words, "rd_addr").filter(|rd| *rd != 0) {
            let data = self.get(words, "rd_wdata").unwrap_or(0);
            effects.push(format!("{}={data:0digits$x}", disasm::X[rd as usize & 31]));
        }
        let addr = self.get(words, "mem_addr").unwrap_or(0);
        let bytes = xlen / 8;
        if let Some(mask) = self.get(words, "mem_rmask").filter(|m| *m != 0) {
            let data = self.get(words, "mem_rdata").unwrap_or(0);
            effects.push(format!(
                "ld [{addr:0digits$x}]={} m{mask:0w$b}",
                format_args!("{data:0digits$x}"),
                w = bytes
            ));
        }
        if let Some(mask) = self.get(words, "mem_wmask").filter(|m| *m != 0) {
            let data = self.get(words, "mem_wdata").unwrap_or(0);
            effects.push(format!(
                "st [{addr:0digits$x}]={} m{mask:0w$b}",
                format_args!("{data:0digits$x}"),
                w = bytes
            ));
        }
        if self.get(words, "mem_fault") == Some(1) {
            effects.push("fault".to_string());
        }
        if self.get(words, "halt") == Some(1) {
            effects.push("halt".to_string());
        }
        for field in &self.trace.fields {
            if !RVFI_SHOWN.contains(&field.name.as_str()) {
                effects.push(format!(
                    "{}={}",
                    field.name,
                    hex(words, field.lsb, field.width)
                ));
            }
        }

        format!(
            "{mode}{pc:0digits$x}  {raw}  {asm:<32}  {}",
            effects.join("  ")
        )
        .trim_end()
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hns_regs::TraceField;

    fn field(name: &str, lsb: usize, width: usize) -> TraceField {
        TraceField {
            name: name.to_string(),
            lsb,
            width,
        }
    }

    /// Puts `value` at `[lsb, lsb + width)` of a record.
    fn put(words: &mut [u32], lsb: usize, width: usize, value: u64) {
        for i in 0..width {
            if value >> i & 1 == 1 {
                let bit = lsb + i;
                words[bit / 32] |= 1 << (bit % 32);
            }
        }
    }

    #[test]
    fn a_short_ring_starts_at_entry_0_and_a_full_one_after_the_newest() {
        assert_eq!(entries(3, false, 8), vec![0, 1, 2]);
        assert_eq!(entries(0, false, 8), Vec::<u32>::new());
        assert_eq!(entries(10, true, 8), vec![2, 3, 4, 5, 6, 7, 0, 1]);
        // The count wraps at 2^32; the ring position does not care.
        assert_eq!(entries(u32::MAX, true, 4)[0], 3);
    }

    #[test]
    fn a_field_may_cross_a_word() {
        let mut words = [0u32; 3];
        put(&mut words, 30, 36, 0xa_bcde_f012);
        assert_eq!(value(&words, 30, 36), 0xa_bcde_f012);
        assert_eq!(hex(&words, 30, 36), "0xabcdef012");
        assert_eq!(hex(&words, 30, 1), "0x0");
    }

    #[test]
    fn an_rvfi_record_prints_as_an_instruction_and_what_it_did() {
        let trace = Trace {
            name: "rvfi".to_string(),
            format: Some("rvfi".to_string()),
            record_bits: 32 + 32 + 32 + 2 + 5 + 32,
            fields: vec![
                field("cycle", 0, 32),
                field("pc_rdata", 32, 32),
                field("insn", 64, 32),
                field("mode", 96, 2),
                field("rd_addr", 98, 5),
                field("rd_wdata", 103, 32),
            ],
        };
        let mut printer = Printer::new(&trace);

        // addi a0, a0, 1 at 0x8000_0000, in M mode, at cycle 5.
        let mut words = [0u32; 5];
        put(&mut words, 0, 32, 5);
        put(&mut words, 32, 32, 0x8000_0000);
        put(&mut words, 64, 32, 0x0015_0513);
        put(&mut words, 96, 2, 3);
        put(&mut words, 98, 5, 10);
        put(&mut words, 103, 32, 0x2a);
        let line = printer.line(&words);
        assert!(line.starts_with("         5"), "{line}");
        assert!(line.contains("M 80000000  00150513  addi"), "{line}");
        assert!(line.contains("a0=0000002a"), "{line}");

        // The next record says how many cycles passed.
        words[0] = 9;
        let line = printer.line(&words);
        assert!(line.contains("     9     +4  "), "{line}");
    }

    #[test]
    fn a_plain_record_prints_its_fields() {
        let trace = Trace {
            name: "ev".to_string(),
            format: None,
            record_bits: 44,
            fields: vec![field("cycle", 0, 32), field("code", 32, 12)],
        };
        let mut words = [0u32; 2];
        put(&mut words, 0, 32, 7);
        put(&mut words, 32, 12, 0xabc);
        assert_eq!(
            Printer::new(&trace).line(&words),
            "         7         code=0xabc"
        );
    }
}

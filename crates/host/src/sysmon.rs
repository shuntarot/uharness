//! The system monitor (XADC on 7-series, SYSMON on UltraScale) over JTAG:
//! die temperature and supply voltages.
//!
//! The monitor has its own JTAG instruction (`XADC_DRP` or `SYSMON_DRP` in
//! the BSDL). It needs no bitstream and no monitor instance in the design, so
//! a blank board can be checked before anything is loaded onto it.
//!
//! With that instruction in the IR, the DR is 32 bits:
//! `[29:26]` command, `[25:16]` register address, `[15:0]` data.
//! A read returns its data on the next scan, so each read is followed by
//! another scan.

use crate::bridge::JtagIo;

/// Which monitor, and so which conversion. Set by the device family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 7-series and Zynq-7000.
    Xadc,
    /// UltraScale (SYSMONE1).
    SysmonE1,
    /// UltraScale+ (SYSMONE4).
    SysmonE4,
}

impl Kind {
    /// From `family` in the target's `[device]`.
    pub fn of_family(family: &str) -> Option<Kind> {
        match family {
            "artix7" | "kintex7" | "virtex7" | "spartan7" | "zynq" => Some(Kind::Xadc),
            "kintexu" | "virtexu" => Some(Kind::SysmonE1),
            "kintexuplus" | "virtexuplus" | "zynquplus" | "artixuplus" => Some(Kind::SysmonE4),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Xadc => "XADC",
            Kind::SysmonE1 => "SYSMONE1",
            Kind::SysmonE4 => "SYSMONE4",
        }
    }
}

const CMD_READ: u32 = 1;

/// The flag register. Bit 9 says the internal reference is in use.
const FLAG: u16 = 0x3f;
const FLAG_INTERNAL_REF: u16 = 1 << 9;
/// Over-temperature alarm.
const FLAG_OT: u16 = 1 << 3;

/// One measured value and the extremes the monitor kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sensor {
    pub name: &'static str,
    pub temperature: bool,
    now: u16,
    max: u16,
    min: u16,
}

/// (name, now, max, min). The max and min registers hold the extremes since
/// the monitor last started: at power-up, and again when the device is erased
/// or configured.
const SENSORS: &[(&str, u16, u16, u16)] = &[
    ("temp", 0x00, 0x20, 0x24),
    ("vccint", 0x01, 0x21, 0x25),
    ("vccaux", 0x02, 0x22, 0x26),
    ("vccbram", 0x06, 0x23, 0x27),
];

/// The Zynq-7000 PS supplies.
const PS_SENSORS: &[(&str, u16, u16, u16)] = &[
    ("vccpint", 0x0d, 0x28, 0x2c),
    ("vccpaux", 0x0e, 0x29, 0x2d),
    ("vcco_ddr", 0x0f, 0x2a, 0x2e),
];

/// What one `read_all` returns.
#[derive(Debug, Clone, PartialEq)]
pub struct Readings {
    pub kind: Kind,
    pub internal_ref: bool,
    pub over_temperature: bool,
    pub sensors: Vec<Sensor>,
}

impl Readings {
    /// Whether the monitor has converted since it started. Until then every
    /// value reads 0, which as a temperature would be about -280 C.
    pub fn has_reading(&self) -> bool {
        self.sensors.iter().any(|s| s.now != 0)
    }

    /// Converts a raw code. `None` when the temperature conversion of this
    /// monitor is not known.
    pub fn value(&self, sensor: &Sensor, code: u16) -> Option<f64> {
        if sensor.temperature {
            celsius(self.kind, self.internal_ref, code)
        } else {
            Some(volts(code))
        }
    }

    /// (now, max, min) converted.
    pub fn values(&self, sensor: &Sensor) -> (Option<f64>, Option<f64>, Option<f64>) {
        (
            self.value(sensor, sensor.now),
            self.value(sensor, sensor.max),
            self.value(sensor, sensor.min),
        )
    }

    /// (now, max, min) as read.
    pub fn codes(sensor: &Sensor) -> (u16, u16, u16) {
        (sensor.now, sensor.max, sensor.min)
    }
}

/// The DR word for reading `addr`.
pub fn read_word(addr: u16) -> u32 {
    (CMD_READ << 26) | ((addr as u32 & 0x3ff) << 16)
}

fn bits(word: u32) -> Vec<bool> {
    (0..32).map(|i| (word >> i) & 1 == 1).collect()
}

fn data(bits: &[bool]) -> u16 {
    bits.iter()
        .take(16)
        .enumerate()
        .fold(0, |a, (i, &b)| if b { a | 1 << i } else { a })
}

/// Reads registers. The IR must already hold the monitor's instruction.
///
/// Each scan returns the data of the read before it, so the last read is
/// followed by one more scan, a read of the flag register (reads have no side
/// effects).
pub fn read<J: JtagIo>(io: &mut J, addrs: &[u16]) -> Result<Vec<u16>, J::Error> {
    let mut out = Vec::with_capacity(addrs.len());
    io.scan_dr(&bits(read_word(addrs[0])))?;
    for &next in addrs[1..].iter().chain(std::iter::once(&FLAG)) {
        out.push(data(&io.scan_dr(&bits(read_word(next)))?));
    }
    Ok(out)
}

/// Reads temperature, the supplies and the flags. `ps` adds the Zynq-7000
/// PS supplies.
pub fn read_all<J: JtagIo>(io: &mut J, kind: Kind, ps: bool) -> Result<Readings, J::Error> {
    let list: Vec<_> = SENSORS
        .iter()
        .chain(if ps { PS_SENSORS } else { &[] })
        .collect();
    let mut addrs = vec![FLAG];
    for (_, now, max, min) in &list {
        addrs.extend([*now, *max, *min]);
    }
    let got = read(io, &addrs)?;
    let flag = got[0];
    let sensors = list
        .iter()
        .zip(got[1..].chunks(3))
        .map(|((name, ..), v)| Sensor {
            name,
            temperature: *name == "temp",
            now: v[0],
            max: v[1],
            min: v[2],
        })
        .collect();
    Ok(Readings {
        kind,
        internal_ref: flag & FLAG_INTERNAL_REF != 0,
        over_temperature: flag & FLAG_OT != 0,
        sensors,
    })
}

/// Supply sensors measure 0 to 3 V over the full code.
pub fn volts(code: u16) -> f64 {
    code as f64 * 3.0 / 65536.0
}

/// The transfer functions of Xilinx's drivers (embeddedsw `xsysmon.h`,
/// `xsysmonpsu.h`). SYSMONE4 has one per reference. SYSMONE1 is left out:
/// the sources disagree on it, and a wrong temperature is worse than none.
pub fn celsius(kind: Kind, internal_ref: bool, code: u16) -> Option<f64> {
    let (gain, offset) = match (kind, internal_ref) {
        (Kind::Xadc, _) => (503.975, 273.15),
        (Kind::SysmonE4, true) => (509.314_006_4, 280.230_878_7),
        (Kind::SysmonE4, false) => (507.592_131_0, 279.426_576_8),
        (Kind::SysmonE1, _) => return None,
    };
    Some(code as f64 * gain / 65536.0 - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers each scan with the register named by the scan before it.
    struct Fake {
        regs: std::collections::HashMap<u16, u16>,
        pending: u16,
    }

    impl JtagIo for Fake {
        type Error = ();
        fn scan_dr(&mut self, bits: &[bool]) -> Result<Vec<bool>, ()> {
            assert_eq!(bits.len(), 32);
            let word = bits
                .iter()
                .enumerate()
                .fold(0u32, |a, (i, &b)| if b { a | 1 << i } else { a });
            assert_eq!(word >> 26, CMD_READ, "only reads are sent");
            let out = self.regs.get(&self.pending).copied().unwrap_or(0);
            self.pending = ((word >> 16) & 0x3ff) as u16;
            Ok((0..32).map(|i| i < 16 && (out >> i) & 1 == 1).collect())
        }
    }

    #[test]
    fn the_word_carries_the_command_at_26_and_the_address_at_16() {
        assert_eq!(read_word(0x00), 0x0400_0000);
        assert_eq!(read_word(0x3f), 0x043f_0000);
    }

    #[test]
    fn each_read_lands_on_its_own_register_despite_the_one_scan_delay() {
        let mut regs = std::collections::HashMap::new();
        for addr in 0..0x40u16 {
            regs.insert(addr, 0x1000 + addr);
        }
        regs.insert(FLAG, FLAG_INTERNAL_REF);
        let mut io = Fake { regs, pending: 0 };
        let r = read_all(&mut io, Kind::SysmonE4, false).unwrap();
        assert!(r.internal_ref);
        assert!(!r.over_temperature);
        let vccbram = r.sensors.iter().find(|s| s.name == "vccbram").unwrap();
        assert_eq!(Readings::codes(vccbram), (0x1006, 0x1023, 0x1027));
        assert_eq!(r.sensors.len(), 4);
        assert!(r.has_reading());
    }

    #[test]
    fn a_monitor_that_just_restarted_has_no_reading() {
        let mut regs = std::collections::HashMap::new();
        for addr in 0x24..0x28u16 {
            regs.insert(addr, 0xffff);
        }
        let mut io = Fake { regs, pending: 0 };
        let r = read_all(&mut io, Kind::Xadc, false).unwrap();
        assert!(!r.has_reading());
    }

    #[test]
    fn codes_convert_as_the_drivers_do() {
        assert!((volts(0x48b4) - 0.852).abs() < 0.001);
        let t = celsius(Kind::SysmonE4, true, 0x9e80).unwrap();
        assert!((t - 35.106).abs() < 0.001, "{t}");
        let t = celsius(Kind::SysmonE4, false, 0x9e80).unwrap();
        assert!((t - 34.844).abs() < 0.001, "{t}");
        // 7-series: code 0 is absolute zero.
        assert_eq!(celsius(Kind::Xadc, false, 0), Some(-273.15));
        assert_eq!(celsius(Kind::SysmonE1, true, 0x9e80), None);
    }
}

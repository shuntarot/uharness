//! Borrowed Verilog, embedded in the generator.
//!
//! `rtl/hns` is a Veryl package, so Veryl resolves it as a dependency.
//! Borrowed Verilog is not. The generated project must synthesize on its own,
//! so these files are built into the binary and written to `hns/vendor/` when
//! needed.
//!
//! Do not edit them by hand. To update one, fetch it from upstream again,
//! together with `COPYING` and `AUTHORS`. `rtl/pcie/README.md` says where they
//! come from.

pub struct VendorFile {
    pub name: &'static str,
    pub text: &'static str,
}

/// The files a PCIe build needs.
pub const PCIE: &[VendorFile] = &[
    VendorFile {
        name: "pcie_us_axil_master.v",
        text: include_str!("../rtl/pcie/rtl/pcie_us_axil_master.v"),
    },
    // The path where the card issues requests. The write engine reads AXI and
    // writes PCIe; the read engine reads PCIe and writes AXI. The read
    // engine's RQ output feeds the write engine's `s_axis_rq_*`, so one RQ
    // stream comes out without an extra arbiter.
    VendorFile {
        name: "pcie_us_axi_dma_wr.v",
        text: include_str!("../rtl/pcie/rtl/pcie_us_axi_dma_wr.v"),
    },
    VendorFile {
        name: "pcie_us_axi_dma_rd.v",
        text: include_str!("../rtl/pcie/rtl/pcie_us_axi_dma_rd.v"),
    },
    VendorFile {
        name: "axil_cdc.v",
        text: include_str!("../rtl/pcie/rtl/axil_cdc.v"),
    },
    VendorFile {
        name: "axil_cdc_rd.v",
        text: include_str!("../rtl/pcie/rtl/axil_cdc_rd.v"),
    },
    VendorFile {
        name: "axil_cdc_wr.v",
        text: include_str!("../rtl/pcie/rtl/axil_cdc_wr.v"),
    },
];

/// The wrapper around the hard block. Our own code, not borrowed. It combines
/// the borrowed modules and folds the hard block's ports into four wires and
/// one AXI4-Lite port. It sits here so that the output synthesizes on its own.
///
/// Both files define `hns_pcie_wrap` with the same ports, and both are written
/// as `hns_pcie_wrap.v`, so a regenerated directory never keeps the other one.
pub fn pcie_wrap(block: crate::emit::PcieBlock) -> VendorFile {
    VendorFile {
        name: "hns_pcie_wrap.v",
        text: match block {
            crate::emit::PcieBlock::Pcie4 => include_str!("../rtl/pcie/rtl/hns_pcie_wrap.v"),
            crate::emit::PcieBlock::Pcie3 => include_str!("../rtl/pcie/rtl/hns_pcie3_wrap.v"),
        },
    }
}

/// The licence travels with the code. MIT requires the copyright and
/// permission notices to be kept, also for someone who only gets the output.
pub const LICENSE: &[VendorFile] = &[
    VendorFile {
        name: "COPYING",
        text: include_str!("../rtl/pcie/COPYING"),
    },
    VendorFile {
        name: "AUTHORS",
        text: include_str!("../rtl/pcie/AUTHORS"),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A wrong path fails to compile in `include_str!`, but a wrong file does
    /// not. This checks the content.
    #[test]
    fn every_borrowed_file_carries_its_module_and_its_licence() {
        for file in PCIE {
            let module = file.name.trim_end_matches(".v");
            assert!(
                file.text.contains(&format!("module {module}")),
                "{} does not define {module}",
                file.name
            );
            assert!(file.text.contains("Alex Forencich"), "{}", file.name);
        }
        // Our own files: no borrowed copyright notice to check.
        for block in [crate::emit::PcieBlock::Pcie4, crate::emit::PcieBlock::Pcie3] {
            let wrap = pcie_wrap(block);
            assert!(wrap.text.contains("module hns_pcie_wrap"), "{block:?}");
            assert!(
                wrap.text.contains(&format!("{}_0 ", block.ip())),
                "{block:?} wraps another block"
            );
        }
        assert!(LICENSE.iter().any(|f| f.name == "COPYING"));
        assert!(
            LICENSE[0].text.contains("Permission is hereby granted"),
            "the licence text has to travel, not just its name"
        );
    }
}

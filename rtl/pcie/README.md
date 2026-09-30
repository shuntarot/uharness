# rtl/pcie — borrowed PCIe parts

**Source**: [verilog-pcie](https://github.com/alexforencich/verilog-pcie) and
[verilog-axi](https://github.com/alexforencich/verilog-axi) by Alex Forencich,
under the MIT license. `COPYING` and `AUTHORS` come with the files and must stay
with them.

## Contents

| File | Purpose |
|---|---|
| `rtl/pcie_us_axil_master.v` | CQ/CC to AXI-Lite: turns host BAR accesses into window requests |
| `rtl/axil_cdc.v`, `_rd.v`, `_wr.v` | AXI-Lite clock domain crossing, PCIe user clock to harness clock |
| `rtl/pcie_us_axi_dma_wr.v` | reads from AXI and writes to PCIe (the card is the requester) |
| `rtl/pcie_us_axi_dma_rd.v` | reads from PCIe and writes to AXI |
| `rtl/hns_pcie_wrap.v` | not borrowed: the UltraScale+ (`pcie4_uscale_plus`) wrapper |
| `rtl/hns_pcie3_wrap.v` | not borrowed: the UltraScale (`pcie3_ultrascale`) wrapper |

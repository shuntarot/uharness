# veryl-harness

A hobby tool for running a single Veryl/SystemVerilog block on a real FPGA,
without writing the boring parts every time.

## harness — build the board around your block

Tell it how the ports connect, pick a board, and it writes the rest: clocks,
the memory controller, a JTAG or PCIe link, constraints, a Makefile and a
register map. This is `examples/hw/dram`, a small AXI4 master on the Arty's
DDR3:

```toml
[dut]
module = "dut_top"

[clock.i_clk]
freq_mhz = 100

[bundle.ctl]
backing = "reg"          # host-visible registers
ports   = ["i_addr", "i_wdata", "i_start_wr", "i_start_rd", "i_sweep",
           "o_rdata", "o_busy", "o_reads", "o_writes", "o_resp"]

[bundle.mem]
backing  = "dram"        # or bram, slave, host_poll_fifo ...
aperture = "32M"
ports    = ["axi"]
```

```bash
veryl harness check --target digilent/arty-a7-35   # checks only, writes nothing
veryl harness gen   --target digilent/arty-a7-35   # writes hns/
make -C hns/syn                                    # bitstream
```

If something cannot work, it says so before you wait an hour for Vivado.

## hio — talk to it

```bash
hio program               # load the bitstream over JTAG
hio id                    # is this the design I think it is?
hio dump                  # every register
hio read  mem_calib       # has the DDR3 calibrated?
hio write mem 0 0xdeadbeef
hio read  mem 0 1
hio memtest mem --size 1024
hio reset --hold          # hold the DUT in reset, memory stays
hio run bringup.hio       # or put it all in a file
```

hio (HostIO) works over JTAG (an FTDI cable) or PCIe, and needs no Vivado.

## Boards

Arty A7-35 and A7-100, VCU118, KCU105, KC705. Adding one is a TOML file.

## Build

```bash
cargo build --release   # gives veryl-harness and hio
```

Needs `veryl` on `PATH` (the version pinned in `Cargo.toml`), and Vivado for
synthesis. More examples are in `examples/hw/`. Details:
[doc/guide.md](doc/guide.md).

## License

MIT or Apache-2.0, at your option. Borrowed files keep their own licenses:
`rtl/pcie` (MIT), the bundled `mig.prj` files (Apache-2.0), and the generated
rocket-chip core in `examples/hw/rocket` (Apache-2.0 and BSD-3-Clause).

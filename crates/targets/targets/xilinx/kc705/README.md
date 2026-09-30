# xilinx/kc705

`mig.prj` is a **changed** copy of the board file in Xilinx's board store:

- repository: https://github.com/Xilinx/XilinxBoardStore
- branch `2022.2`, commit `40fad97cf36f3d856c6759d83c2390de1566ffa7`
- path: `boards/Xilinx/kc705/1.6/mig.prj`

It is licensed under the Apache License 2.0; see `LICENSE.XilinxBoardStore`.

Changes, and why:

- `SystemClock` `Differential` -> `No Buffer`, and `ReferenceClock`
  `Use System Clock` -> `No Buffer`. The 200 MHz pair is the board's only
  general clock. The harness MMCM takes it and gives the MIG both clocks, as on
  the Arty. The `sys_clk_p/n` pins and the `System_Clock` section went with it.
- `C0_S_AXI_ADDR_WIDTH` `32` -> `30`. The SODIMM is 1 GB. With 32 bits,
  addresses above 1 GB would wrap without an error, and the harness checks
  regions against this width.

Everything else is unchanged.

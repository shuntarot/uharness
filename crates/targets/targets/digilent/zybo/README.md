# digilent/zybo

`ps7_preset.tcl` holds the `CONFIG.PCW_*` settings of the `processing_system7`
IP, taken from Xilinx's board store:

- repository: https://github.com/Xilinx/XilinxBoardStore
- branch `2022.2`, commit `40fad97cf36f3d856c6759d83c2390de1566ffa7`
- path: `boards/Digilent/zybo/B.3/preset.xml`, the `processing_system7` part of
  `ps7_preset`

The names and values are unchanged; only the form changed, from XML to a Tcl
list. They describe the board's PS: its DDR3 (MT41K128M16 JT-125, 525 MHz),
MIO and clocks. Vivado turns them into the `ps7_init.tcl` that `hio` runs to
set the PS up.

It is licensed under the Apache License 2.0; see `LICENSE.XilinxBoardStore`.
Do not edit the settings: a change is a different board.

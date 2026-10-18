# digilent/pynq-z1

`ps7_preset.tcl` holds the `CONFIG.PCW_*` settings of the `processing_system7`
cell `ps7`, copied unchanged from the PYNQ project:

- repository: https://github.com/Xilinx/PYNQ
- commit `de6b6fc3a803945d59f8f06523addfe0d9b60a1c`
- path: `boards/Pynq-Z1/petalinux_bsp/hardware_project/pynqz1.tcl`

They describe the board's PS: its DDR3 (MT41J256M16 RE-125, 16 bits, 525 MHz),
MIO and clocks. Vivado turns them into the `ps7_init.tcl` that `hio` runs to set
the PS up. The project is licensed under the BSD 3-Clause License; see
`LICENSE.PYNQ`. Do not edit the settings: a change is a different board.

// Smoke test for the HNS core on the FPGA harness.
// Runs from DRAM (0x8000_0000) and writes to MMIO (0x0002_0000):
//   +0x00: 0x21534e48 ("HNS!")
//   +0x08: misa
//   +0x10: mhartid
//   +0x18: counter, incremented forever
#define MMIO_BASE 0x20000

.section .text.start, "ax", @progbits
.globl _start
_start:
  li   t0, MMIO_BASE
  li   t1, 0x21534e48
  sw   t1, 0x00(t0)
  csrr t1, misa
  sd   t1, 0x08(t0)
  csrr t1, mhartid
  sd   t1, 0x10(t0)
  li   t1, 0
1:
  addi t1, t1, 1
  sd   t1, 0x18(t0)
  j    1b

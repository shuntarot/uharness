# Guide: hio and the CPU

`hio cpu` runs firmware on a RISC-V core in the harness, debugs it through
its Debug Module, and saves and restores its state. `hio trace` and
`hio count` watch what the core runs. The rest of `hio` (the window, the
prompt, PCIe, `serve`) is in [guide-hio.md](guide-hio.md); the `dmi`,
`trace` and `count` bundles are in [guide-harness.md](guide-harness.md).

- [`hio cpu boot` and `cpu console`: run firmware](#hio-cpu-boot-and-cpu-console-run-firmware)
- [`hio cpu halt`, `step`, `regs`: debug the CPU](#hio-cpu-halt-step-regs-debug-the-cpu)
- [`hio trace`: the last instructions](#hio-trace-the-last-instructions)
- [`hio count`: count instructions and mark two counts](#hio-count-count-instructions-and-mark-two-counts)
- [`hio cpu save` and `restore`: keep a state, go back to it](#hio-cpu-save-and-restore-keep-a-state-go-back-to-it)

## `hio cpu boot` and `cpu console`: run firmware

`hio cpu boot` loads an ELF, runs it, and connects to its RTT console. It is
meant as a cargo runner:

```toml
# .cargo/config.toml of the firmware
[target.riscv32imac-unknown-none-elf]
runner = "hio cpu boot"

[env]
HIO_REGS = { value = "../hns/regs.json", relative = true }
```

It holds the DUT in reset, puts each segment into every memory that covers its
address (`dut_base`), clears the `_SEGGER_RTT` block, and releases the DUT.
The zeros after a segment's file bytes (`.bss` after `.data`) are written
too; a segment with no file bytes, usually the stack, is left to the startup
code. Over PCIe, a large ELF goes the way `load` takes it: the card writes
the memory it reaches, and only a part of an entry at either end goes through
the window. `--verify` reads it all back and compares it before the DUT is
released. `--zero` fills every memory the ELF goes to with zeros first, so a
later `cpu save` leaves out the pages the program never wrote. Up
channel 0 goes to the terminal and keys go to down channel 0; Ctrl-A x
leaves. `hio cpu console <elf>` connects without loading, so leaving and
coming back is harmless.

The exit status is the program's: from `tohost` (1 passes, `(code << 1) | 1`
fails with `code`), or from the first byte on an up channel named `hio-exit`.
`--timeout <seconds>` gives up on a program that does not exit, with status
124. That is enough to run [riscv-tests](https://github.com/riscv-software-src/riscv-tests)'
`isa` tests one by one:

```bash
hio cpu boot --timeout 60 rv64ui-p-add   # 0 passes
```

Their `benchmarks` print through HTIF system calls (`tohost` and `fromhost`);
`hio` answers `write`, so their output, `mcycle` and `minstret` included,
reaches the terminal. Other calls stop `hio` with an error.

`hio cpu boot --halt` stops at the first instruction instead, at the debug
prompt ([`hio` with no command: the prompt](guide-hio.md#hio-with-no-command-the-prompt)), so breakpoints can go in before anything runs; `resume` lets
it go, and Ctrl-A c shows the console once the program has set up RTT. It
needs the Debug Module on its own reset (`reset` in the `dmi` bundle).

```
hart 0 halted at pc 0x80000000 <_start> (halt request)
(hio cpu) break main
(hio cpu) resume
hart 0 halted at pc 0x800001f4 <main> (breakpoint)
```

The host reads memory from outside the CPU. With a write-back data cache, put
the RTT block where the CPU does not cache, or the host never sees it.

**Semihosting.** A program may also call the host the way it would under QEMU
(`slli x0, x0, 0x1f; ebreak; srai x0, x0, 7`). When the ELF holds that
sequence, `cpu boot` sets the hart to halt on `ebreak` before it runs (as
`--catch` does, so the Debug Module needs its own reset) and answers: writes
to the terminal (`:tt`), the exit status, and the time. Host files and
keyboard input are not offered. Each call halts the core until the answer
comes, so RTT stays the fast way to print; semihosting is for a program that
also runs on QEMU, or that prints before RTT is up.

## `hio cpu halt`, `step`, `regs`: debug the CPU

With a `dmi` bundle, `hio cpu` also drives the Debug Module:

```bash
hio cpu info              # the Debug Module, the hart, and (halted) its ISA and triggers
hio cpu halt              # halt and show the PC and why it stopped
hio cpu step 3            # three instructions, one PC per line
hio cpu regs              # GPRs and the usual CSRs; `regs a0`, `regs pc 0x80000000`
hio cpu resume
hio cpu reset --halt      # reset through the Debug Module, stop at the first instruction
hio cpu break main --elf app.elf   # a hardware breakpoint; `break` lists, `--clear` removes
hio cpu mem COUNTER --elf app.elf  # memory at a DUT address or symbol; `--write` stores
hio cpu disasm --elf app.elf       # the code around the halted PC; `disasm main`, `disasm 0x80000000 8`
hio cpu catch             # stop on a fault (illegal instruction, access fault, ...)
hio cpu ipc --time 3      # cycles, instructions and IPC of the running program
hio cpu dmi dmstatus      # one DMI register, raw; give a value to write it
```

`--hart N` selects a hart, and `--elf` gives symbols: places by name and PCs
inside functions. These work in a procedure file too; `cpu boot` and `cpu
console` do not, because they take over the terminal.

`mem` goes through the window to the memories `dut_base` places at that
address, so it needs no Debug Module and no halt. A CPU with a data cache may
hold newer data than it shows. A symbol is read whole unless a count is given.
`mem --sba` goes through the Debug Module's System Bus Access instead. It is
slower, but on a system whose caches stay coherent with that bus it shows
what the harts see, while they run.

`disasm` reads the same way, so with a place and `--elf` it needs no Debug
Module either. Where two memories hold the address, it reads the one the CPU
fetches from (the one the DUT does not write). It covers RV32 and RV64 with
I, M, A, C, F, D, Zicsr, Zifencei, Zba, Zbb, Zbs, Zicond and Zicbom, and
prints them as GNU objdump does. A call through `auipc` + `jalr` shows where
it goes. A stop (`halt`, `step`, a breakpoint) shows the instruction at the
PC too:

```
hart 0 halted at pc 0x800001f4 <main> (breakpoint)
=> 0x800001f4:  1141      addi    sp,sp,-16
```

A line ends with `(the ELF has ... here)` where the memory differs from the
ELF: the program changed it, or another program is loaded.

`catch` uses the hart's exception trigger, an optional part of the trigger
module many cores leave out (`hio` then says so); breakpoints use its
address triggers. The hart stops at the trap handler, and the stop says where
the fault was:

```
hart 0 trapped: illegal instruction at 0x80000508 <hazard3_bsp::getc> (mtval 0x00000000); halted at the handler, pc 0x800000f8 <_default_start_trap>
```

Triggers live in the core, so a reset clears them; `cpu boot --catch` sets it
at the first instruction. Without a terminal (a test run), a halt in the
console ends `hio` with an error instead of waiting at a prompt.

`ipc` halts the hart to read `mcycle` and `minstret`, lets it run for the
time (1 s by default), and reads them again; the hart is then left running or
halted, as it was. If `mcountinhibit` stops the counters, it is lifted for the
run and put back. Whether they also count while halted depends on the core,
so keep the time long against the two halts.

A breakpoint takes one of the hart's address triggers. Resuming or stepping
from one first runs its instruction with the trigger off, since the trigger
fires before the instruction runs.

A halt also makes `ebreak` in the program halt into the debugger. CSRs the
Debug Module cannot reach directly go through its program buffer, with `s0`
saved and put back.

## `hio trace`: the last instructions

A `trace` bundle is a ring in the FPGA. While it runs, it records every beat
and overwrites the oldest record once full, so it holds what led up to the
moment it stopped. With a CPU, that moment is usually a breakpoint:

```bash
hio cpu break 0x80001234    # or `hio cpu catch` for a fault
hio trace start             # empty the ring and record
hio cpu resume
hio trace dump --last 50    # once the hart has halted; oldest first
```

A halted hart retires nothing, so the ring stops by itself. `hio cpu` also
pauses an instruction trace (`format = "rvfi"`) while the hart is halted: a
debugger reads registers by running instructions on the halted hart, and
those would push the program's own out of the ring. `resume` and `step`
record again, so a step shows up as one record.

`start` always starts over; `stop` stops. `dump` also works while the trace
runs; if records arrive meanwhile, it says so.

A trigger stops it without a CPU halting, as on a logic analyzer: the ring
keeps `--after` records past the first one that matches, then freezes, and
`dump` marks that record. Only the fields of the ports in `trigger` (in
`Harness.toml`) can be tested. Several `--when` must all hold; `value/mask`
tests some bits, and `field!=0` that a field is not 0.

```bash
hio trace start --when trap=1 --after 100
hio trace start --when pc_rdata=0x80038000/0xfffff000     # anywhere in a 4 KB page
hio trace start --when mem_addr=0x80011b94 --when mem_wmask!=0   # a store there
hio trace start --now --after 1023                        # the first 1024 records
hio trace status                                          # waiting, or where it triggered
```

An RVFI `mem_addr` means something only when `mem_rmask` or `mem_wmask` is
not 0; on other instructions a core may leave the last address there. So
test it together with one of them, and list that one in `trigger` too.

`--now` triggers on the first beat, on no condition, so it needs no
`trigger`. At the `(hio cpu)` prompt, `trace …` works without `hio`. The bundle name may be left out when the design
has one trace, or one instruction trace.

Each line starts with the cycle of the record, counted from `start`, and the
cycles since the record before it. An instruction trace then shows the mode,
the pc, the instruction, and what it did:

```
  52050803    +12  M 8001ffa8  00fa2423  sw      a5,8(s4)                  st [8003831c]=00000e89 m1111
  52050863    +60  M 8001ffac  00da2023  sw      a3,0(s4)                  st [80038314]=00000004 m1111
  52050947    +72  M 8001ffb4  00caa783  lw      a5,12(s5)                 a5=00000000  ld [80038310]=00000000 m1111
```

`hio reset` leaves the trace running, so it also records the reset and what
follows.

## `hio count`: count instructions and mark two counts

A `count` bundle adds up a number the DUT reports every cycle, such as the
instructions it retired, and counts the cycles beside it. It keeps the cycle
in which the count first reaches each of two values, so the cycles between
two instruction counts are exact, without stopping the core:

```bash
hio count start --start 1000 --end 4000
hio count status
```

```text
retire: counting
count     597312
cycles    398220, 1.50 per cycle
start     1000: reached at cycle 667, count 1001
end       4000: reached at cycle 2667, count 4001
between   2000 cycles, 3000 counted, 1.50 per cycle
```

`start` starts over: both counts and both marks return to 0. The count can
pass a value when several come in one cycle, so the count at each mark is
shown too. Nothing is counted while the DUT is held in reset. `stop` stops
counting and keeps the values.

With `halt` in the bundle, `--halt start`, `--halt end` or `--halt both` also
halts a hart (`--hart`, default 0) at that mark: the harness writes
`dmcontrol` itself in that cycle. `--hart all` halts every hart with that one
write, through the Debug Module's hart array window (`hasel`); a Debug
Module without one is refused. `start` wakes the Debug Module first: an
inactive one takes only `dmactive` from the harness's write. Give it its own
`reset` in the `dmi` bundle, so `hio reset` does not put it back to sleep. A
hart stops a few instructions after the
mark; `minstret` says how many. `hio cpu` then works on the halted harts as
usual.

## `hio cpu save` and `restore`: keep a state, go back to it

`hio cpu save <dir>` halts every hart and writes what the CPU holds:

```text
$ hio cpu save ck --range tcm --range dmem:0xf0000:64K
  hart 0: pc 0x80000068, 101 CSRs
1 hart(s) halted and saved, 2 device register(s), 0 region(s)
  0x0080000000 <- tcm  65536 bytes in 0.16 s, 416.6 KB/s (through the window)
  0x0080100000 <- dmem  65536 bytes in 0.47 s, 138.7 KB/s (through the window)
saved to ck in 1.10 s: 77824 bytes of memory in 2 segment(s), the rest zero
```

- `checkpoint.json`: per hart the GPRs, the FPRs, the PC, the privilege
  level, and every standard CSR the hart has (the hart is asked; one it
  refuses is skipped); the device registers of `checkpoint.mmio`; and which
  memory ranges were saved. Values are hex strings. The fields are
  [below](#checkpointjson-and-a-checkpoint-made-elsewhere).
- `memory.elf`: the harness memories as an ELF image, with all-zero pages
  left out. A simulator's loader can read it.
- `<name>.bin` for each `checkpoint.region`.

Without `--range` every memory is saved whole: seconds for gigabytes over
PCIe, but slow over JTAG (about 140 KB/s), so name the part the program uses
there. DRAM holds noise after power-up, so a whole DRAM saves as a whole DRAM;
`hio cpu boot --zero` fills the memories with zeros before loading, and then
the pages the program never wrote are left out. The harts stay halted unless
`--resume`. A core with a write-back data cache needs `checkpoint.flush` in
its `dmi` bundle ([Harness.toml](guide-harness.md)); `--sba` reads memory
through the Debug Module's system bus instead, which is slow, but sees what a
coherent system bus sees.

`hio cpu restore <dir>` halts every hart, flushes the cache, writes the
memories (zeros where the image left pages out), the regions, the device
registers and the registers, and leaves the harts halted unless `--resume`.
It refuses a checkpoint from another harness (another map hash).

Where the core lets the counters stop while a hart is halted
(`dcsr.stopcount`), `mcycle` and `minstret` come back as the program left
them; otherwise they include the debugger's own work, and `cpu save` says
so. Caches, branch predictors and other state the program cannot see are
not saved; a run that is measured after a restore needs a warm-up. Device state outside
`checkpoint.mmio` (a UART's FIFO, an interrupt being claimed) is not saved
either.

### `checkpoint.json`, and a checkpoint made elsewhere

Numbers are hex strings (`"0x80000000"`); a plain JSON number is read too.

| field | |
|---|---|
| `format` | `1` |
| `dut`, `map_hash` | the DUT and the map hash of the harness, as in its regs.json. `restore` refuses another harness |
| `dmi` | the `dmi` bundle. `--dmi` overrides it; left out, the only one |
| `harts` | one entry per hart (below) |
| `mmio` | `name`, `addr`, `width`, `value`: device registers, written in this order |
| `memory` | `name` (the bundle), `addr`, `size`: the ranges written from `memory.elf`, with zeros where the image has nothing |
| `region` | `name`, `addr`, `size`: written from `<name>.bin` |

A hart has `hart` and `xlen`, then `pc` (`dpc`), `prv` (0 U, 1 S, 3 M),
`gpr` (`x0` to `x31`), `fpr` (`flen`, and `regs` for `f0` to `f31`) and
`csr` (`number` and `value`, written in this order; `name` is for people).

A checkpoint can come from somewhere else, such as a simulator stopped at
the same program. It may leave out what it does not know, and `restore`
leaves that part of the board as it is: a hart not listed, `pc`, `prv`,
`gpr`, `fpr`, any CSR, a device register, a memory not in `memory`
(`memory.elf` is needed only when `memory` lists something). `gpr` and
`fpr.regs` are all 32 values or none. Copy `dut` and `map_hash` from the
regs.json of the harness it goes to:

```json
{
  "format": 1,
  "dut": "top",
  "map_hash": 305419896,
  "harts": [{
    "hart": 0, "xlen": 64, "pc": "0x80000100", "prv": 3,
    "csr": [{ "number": "0x300", "value": "0x1800" }]
  }],
  "memory": [{ "name": "dram", "addr": "0x80000000", "size": "0x40000" }]
}
```

The CSRs go back as the hart takes them: a WARL field keeps only the values
the core allows, and a CSR the hart does not have is reported, not written.

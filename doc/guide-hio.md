# Guide: hio

`hio` talks to a generated harness on the board, or in the simulator: it
reads and writes the window, loads memories, runs firmware and debugs a CPU.
Generating the harness is in [guide-harness.md](guide-harness.md).

- [`id`, `read`, `write`, `dump`: the basics](#id-read-write-dump-the-basics)
- [`hio` with no command: the prompt](#hio-with-no-command-the-prompt)
- [`-p`: over PCIe](#-p-over-pcie)
- [`hio serve` and `--connect`: from another machine](#hio-serve-and---connect-from-another-machine)
- [`hio reset`](#hio-reset)
- [`hio run`: procedure files](#hio-run-procedure-files)
- [`hio load` and `memtest`](#hio-load-and-memtest)
- [`hio cpu boot` and `cpu console`: run firmware](#hio-cpu-boot-and-cpu-console-run-firmware)
- [`hio cpu halt`, `step`, `regs`: debug the CPU](#hio-cpu-halt-step-regs-debug-the-cpu)
- [`hio trace`: the last instructions](#hio-trace-the-last-instructions)
- [`hio program` and `erase`](#hio-program-and-erase)
- [`hio sysmon`: temperature and voltages](#hio-sysmon-temperature-and-voltages)
- [`hio dap`: the Zynq-7000 PS](#hio-dap-the-zynq-7000-ps)

## `id`, `read`, `write`, `dump`: the basics

`hio` reaches the window over JTAG or PCIe. **Vivado and OpenOCD are not
needed** — it drives the same cable directly.

```bash
hio id                      # is the board running this map?
hio dump                    # every register
hio read  ctl_status
hio write ctl_start 1
hio load  dmem prog.elf --base 0x80000000 --verify
hio memtest dmem
hio drain tx
hio bench mem --both
```

**Arguments are usually unnecessary.** `regs.json` is found at `./regs.json`,
then `hns/regs.json`, then `hns/*/regs.json` when there is only one (with
several, pass `--regs`). The target is recorded inside it. A map generated with
`--target-file` records only that it came from a file, so pass the same file to
`hio` with `--target-file`. Probe details are
discovered from the FTDI device, and whatever is guessed is **verified against
magic and the map hash before anything else happens** (`--no-verify` skips it).
`hio probe` shows what would be used. With several probes of one type plugged
in, pick one with `--serial`; a part of the serial that no other probe has is
enough (`--serial 8107`).

| subcommand | |
|---|---|
| `targets` | boards this build knows, and what each can do |
| `probe` | FTDI devices, with IDCODE and IR length measured |
| `program <file.bit>` | configure the device, **without Vivado** |
| `erase` | clear the configuration memory |
| `id` | check magic and map hash; also shows `timeouts` |
| `read` / `write` | a register by name, or a region as `<region> <off> [words]` |
| `load <bundle> <file>` | put a `.bin` or `.elf` in a memory |
| `memtest <bundle>` | write a pattern and read it back; **the contents are lost** |
| `drain <bundle>` | empty a `host_poll_fifo`, oldest first |
| `trace …` | a `trace` ring: `start`, `stop`, `status`, `dump`; see [`hio trace`](#hio-trace-the-last-instructions) |
| `dump` | read every register |
| `reset` | reset the DUT only; `--hold` / `--release` keep it in reset |
| `bench` | how fast the window is; `<region> --both` measures writes too |
| `check` | whether the board and this machine are ready, and what to fix |
| `dma-fire` | the card moves one descriptor itself, either way (PCIe + `dram`); `--pcie-addr` aims it at another card's BAR |
| `run <file>` | a file of commands, holding the link open (`-` for standard input) |
| `serve` | serve the window to `hio --connect` on other machines |
| `sysmon` | the FPGA's temperature and supply voltages |
| `dap …` | the Zynq-7000 PS's memory, through the ARM DAP: `info`, `read`, `write`, `ps7-init` |
| `completions <shell>` | a shell completion script |
| `cpu …` | firmware and the CPU: `boot`, `console`, and the Debug Module (`halt`, `step`, `regs`, `break`, `mem`, `catch`, …); see [`hio cpu boot` and `cpu console`: run firmware](#hio-cpu-boot-and-cpu-console-run-firmware) and [`hio cpu halt`, `step`, `regs`: debug the CPU](#hio-cpu-halt-step-regs-debug-the-cpu) |

`hio --help` shows the common commands and options. `hio --list` lists every
command, and `hio help options` explains every option, the probe settings
included.

## `hio` with no command: the prompt

`hio` alone opens a prompt that keeps the link open, so each command
answers without reconnecting. A line is a line of a procedure file; `cpu`
goes to the CPU commands, where `halt` or `step 3` need no `cpu`, and `hio`
comes back. A line that starts with `cpu` or `hio` runs there once, without
moving. `help` lists the commands; Ctrl-D or `exit` leaves. From a pipe
(`hio < steps.hio`) the lines just run, and the first error stops them.

```
$ hio
(hio) read ctl_status
(hio) reset --hold
(hio) cpu
(hio cpu) halt
(hio cpu) hio read ctl_status
(hio cpu) hio
(hio)
```

Up and down recall earlier lines; Ctrl-C drops the line.

In the console, Ctrl-A c switches to the prompt and back, so there is no
need to leave. It opens at the CPU commands; `hio` reaches the others. The
PC is shown by symbol.

```
(hio cpu) halt
hart 0 halted at pc 0x80000b72 <main+0x1c> (halt request)
(hio cpu) step 2
(hio cpu) hio read dmem 0x100
(hio cpu) resume
```

When the hart halts by itself, at a breakpoint, the console and the prompt
say where and stay at `(hio cpu)`:

```
(hio cpu) break hazard3_bsp::getc
breakpoint 0 at 0x80000508 <hazard3_bsp::getc>
hart 0 halted at pc 0x80000508 <hazard3_bsp::getc> (breakpoint)
```

The target's output waits while the prompt is up and shows when you go back.

## `-p`: over PCIe

| | |
|---|---|
| default | JTAG. A PCIe design has both, so this reaches it too |
| `-p` (= `-t pcie` = `--transport pcie`) | over the BAR; needs root |
| `--bdf <bdf>` | only when several cards answer to the same ID; the first column of `lspci` |

```bash
sudo "$(command -v hio)" -p id
```

`sudo` replaces `PATH`, hence the absolute path. JTAG stays the default because
it is what tells "the window is broken" from "PCIe is broken"; `probe`,
`program` and `erase` walk the TAP and are refused over PCIe.

Two things it will stop for: memory decoding turned off (it prints the
`echo 1 | sudo tee .../enable` to run), and a BAR whose size disagrees with
`regs.json` — meaning the board is not running this map.

`timeouts` in `hio id` counts the times the window had to answer for a
terminator that would not. Anything but 0 means a value read then was 0 rather
than data. `hio` itself waits up to 3 s for `<bundle>_calib` before it touches
a `dram`, and does not touch a slave in a DUT held in reset. Clear it with
`hio write harness_timeout 1`; writing 0 does not clear it.

A PCIe design also counts the TLPs that reach its BAR, and the completions it
sends back. Read them with `hio read`, before and after:

| Register | Counts |
|---|---|
| `pcie_mrd`, `pcie_mwr`, `pcie_other` | memory reads, memory writes, other requests |
| `pcie_mwr_dropped` | writes dropped without a trace (longer than one dword) |
| `pcie_cpl_sc`, `pcie_cpl_ur`, `pcie_cpl_ca` | completions sent, by status |

The BAR takes one dword per request. A longer read is answered with Completer
Abort, and a longer write is dropped. The counts saturate and survive a link
reset. A request the PCIe block refuses itself (no BAR hit, memory decoding
off) never reaches them; `lspci -vvv` shows that one as `UnsupReq+` in the
card's `DevSta`. Read them over JTAG: a read over PCIe counts itself.

## `hio serve` and `--connect`: from another machine

`hio serve` on the machine the board is on keeps the window open and passes
batches from `hio --connect` on other machines. It speaks
[HNS2](hns2.md), so other programs can be clients too.

```bash
# on the machine with the board (add -p, under sudo, for PCIe)
hio serve --listen 127.0.0.1:5555
# elsewhere: forward the port, then connect with a copy of its regs.json
ssh -N -L 5555:localhost:5555 board-host &
hio --regs regs.json --connect localhost:5555 id
```

- `HIO_CONNECT=localhost:5555` does the same as `--connect`.
- One client at a time; another is refused until the first is done.
- There is no password. Keep the default `127.0.0.1` and use `ssh -L`.
- `--connect` reaches only the window, with one exception below. `erase`,
  `probe`, `sysmon`, `dap`, `program <file>.svf` and the card's DMA (`--dma`,
  `dma-fire`) run on the server's machine.
- A simulation is served the same way: `veryl harness sim --addr`.

The server also programs the board, so you can change the design without
going to its machine:

```bash
hio --serial <probe> serve --listen 127.0.0.1:5555                       # once, on the board's machine
hio --connect localhost:5555 program                                     # elsewhere, from the project
```

On a board with nothing programmed yet, start the server with
`hio --no-verify serve` from the project; the first `program` checks the board.
Until then, the window answers with meaningless values.

`program` sends the `.bit` and its `regs.json`, and the files the server asks
for from beside the `.bit` (a Zynq's `ps7_init.tcl`). The server checks the device name as `hio program`
does, programs over JTAG, sets a Zynq's PS up, and serves the new map.

Over PCIe (`sudo "$(command -v hio)" -p --serial <probe> serve`), you can
change the design without a reboot. The server writes the card's PCI config
space back after programming. It refuses a design whose ID, class, BAR size or
interrupt pin differs from the card's: the machine would have to find the card
again (remove and rescan, or reboot).

## `hio reset`

```bash
hio reset                   # reset the DUT and let it run again
hio reset --hold            # keep it in reset
hio reset --release         # let it run
```

**Only the DUT is reset.** The window, memory contents and `reg` registers keep
their state, a `trace` keeps recording, and a `host_poll_fifo` keeps what it has queued (`drain` it if the
test needs it empty). So a test can start the same way every time:

```bash
hio check
hio reset --hold
hio load dmem prog.elf --base 0x80000000 --verify
hio reset --release
```

If the DUT is an AXI4 master, the harness first lets it finish the handshake it
has started, then fills the rest of an unfinished write burst with empty strobes
and drops the replies still on their way. The memory stays usable for the host
the whole time. `hio` writes `dut_reset` and then reads `dut_reset_state` until it
changes, so the timing does not depend on the transport. `check` says when the
DUT is still held.

## `hio run`: procedure files

```
# bringup.hio
echo loading
reset --hold
load dmem prog.elf --base 0x80000000 --verify
reset --release
sleep 100
read status
```

```bash
hio run bringup.hio
```

One command per line, written exactly as on the command line; `#` comments;
`echo` and `sleep <ms>` exist only here. **The whole file is parsed before the
board is touched**, and the first error stops it with the line and its number.
`program` and `erase` cannot appear — reconfiguration stays outside a procedure.

## `hio load` and `memtest`

```bash
hio load dmem prog.elf --verify
```

An ELF needs the DUT address of entry 0. It comes from the bundle's `dut_base`,
or from `--base 0x80000000`, which wins. Without either, `load` stops: get it
wrong and the program lands somewhere else. The computed addresses are printed
so you can check them against the linker script. ELF segments are placed by
`p_paddr`. For a raw `.bin`, give `--at <entry>` instead.

`memtest` writes a value derived from the entry and word index, so a swapped
address line always shows. A mismatch is reported with its address, and when the
value belongs to an address one bit away, that bit is named -- the difference
between a broken wire and a broken cell. Bad addresses are usually not alone, so
it keeps going and lists them rather than stopping at the first.

**Testing the whole memory is a different thing from testing the path.** A run
over the first kilobyte never toggles the upper row, bank and bank-group pins,
so it cannot tell a working chip from one with a stuck address line. Covering
the device is what proves those, and it costs minutes over PCIe and about an
hour per 256MB over JTAG. Because of that, `memtest` asks rather than assuming:
above 1MB it refuses to start without `--size`.

```bash
hio memtest mem --size 64k     # the path
hio memtest mem --size 256M    # the pins
```

Over PCIe the read-back is spread over 4 threads, because a read stalls one core
for the whole round trip. More than 4 does not help. `--threads N` sets it;
over JTAG it stays at 1.

A large transfer is done by the card itself rather than one word at a time
through the window: to fill the memory (`load`, and the pattern `memtest` writes)
the card fetches the data from this machine's memory, and to read it back it
writes the memory into this machine's. That
needs what `check` reports (root, huge pages, and the IOMMU passing this
card's addresses through); without it the window is used instead, and the output
says which it was. Only the memory the card is wired to (the `dram` bundle) goes
this way. `--pio` and `--dma` force one or the other, for telling them apart.

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
code. Up
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
prompt ([`hio` with no command: the prompt](#hio-with-no-command-the-prompt)), so breakpoints can go in before anything runs; `resume` lets
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

## `hio program` and `erase`

```bash
hio program                    # the one beside the register map
hio program path/to/<top>.bit  # or name it
```

Without a file, `syn/output/*.bit` under the directory `regs.json` was found in is
used -- that is where `gen` puts it. If there is more than one there, it says so
rather than picking.

The part number in the `.bit` header is checked against the target before
anything is written (`--allow-any-device` skips it). `.bin` is refused: it has
no header. JTAG configuration is volatile, so a failure costs a power cycle and
nothing else — nothing is written to flash. Devices with several SLRs are
handled from the target description. `hio program <file>.svf` replays an SVF,
which works on devices this build does not know.

After DONE, `program` reads the magic through the window, when it finds the
register map. A bitstream that is not a harness has no map: `--no-verify`
stops at DONE without a note.

Stop Vivado's `hw_server` first (`pkill hw_server`); it takes the same channel.

When the board is on a machine without Vivado, `make -C hns/syn zip` packs
`regs.json`, the bitstream, the SVF (if `make svf` wrote one) and the reports
into `syn/output/<top>.zip`. The paths inside start at the harness directory,
so on the other machine:

```bash
unzip <top>.zip && cd hns
hio program
hio id
```

A chain with other devices on it (the ARM DAP of a Zynq-7000) is handled from
the target description: `hio` holds them in BYPASS. `hio probe` lists the chain
and marks the device it talks to.

## `hio sysmon`: temperature and voltages

```bash
hio --target xilinx/vcu118 sysmon
```

```text
monitor   SYSMONE4 (internal reference)
                now      min      max     codes (now min max)
temp           37.5     35.1     44.0  C  0x9fb6 0x9e7f 0xa2f4
vccint        0.721    0.716    0.724  V  0x3d86 0x3d1b 0x3dc4
vccaux        1.792    1.786    1.798  V  0x98e4 0x9868 0x9973
vccbram       0.850    0.846    0.854  V  0x4884 0x4839 0x48d7
```

It reads the FPGA's system monitor (XADC on 7-series, SYSMON on UltraScale)
through the monitor's own JTAG instruction, `sysmon_ir` in the target's
`[jtag]`. No bitstream is needed, so a new board can be checked before
anything is loaded onto it, and a running design is not disturbed. `min` and
`max` are what the monitor kept since it started: at power-up, and again at
each `erase` or `program`. A device with several SLRs shows the master SLR. A
Zynq-7000 also lists the PS supplies.

The temperature uses the formulas of Xilinx's drivers. On UltraScale+ with the
internal reference, Vivado's hardware manager can show a few degrees less for
the same codes. On UltraScale (SYSMONE1) only the codes are shown. Stop
`hw_server` first, as for `program`.

## `hio dap`: the Zynq-7000 PS

A Zynq-7000 has an ARM DAP on the JTAG chain beside the PL. `hio dap` reaches
the PS's memory and registers through it, with no program on the CPUs; `dap`
in the target's `[jtag]` says where the DAP is.

```bash
hio dap info                      # the DAP's IDCODE and its access ports
hio dap read 0xf8000530           # SLCR PSS_IDCODE
hio dap write 0x00100000 0x1234   # DDR, once the PS is set up
```

`read` and `write` go through the access port `mem_ap` in the target's `dap`,
or the one `--ap` names.

The PS's DDR, PLLs and the level shifters to the PL are set by software after
every power-up. `hio dap ps7-init ps7_init.tcl` does it from the file Vivado
writes with the PS7 IP. When the harness's `dram` is the PS's, `hio program`
runs it after the bitstream, from the `ps7_init.tcl` beside it. A design
without a `dram` bundle does not use the PS, and `hio program` leaves it
alone.

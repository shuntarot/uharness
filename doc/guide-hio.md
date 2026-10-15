# Guide: hio

`hio` talks to a generated harness on the board, or in the simulator: it
reads and writes the window, loads memories, runs firmware and debugs a CPU.
Generating the harness is in [guide-harness.md](guide-harness.md).

- [Basics](#basics)
- [The prompt](#the-prompt)
- [Over PCIe](#over-pcie)
- [Resetting the DUT](#resetting-the-dut)
- [Writing a procedure down](#writing-a-procedure-down)
- [Loading a memory](#loading-a-memory)
- [Running firmware](#running-firmware)
- [Debugging the CPU](#debugging-the-cpu)
- [Programming](#programming)

## Basics

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
`hio probe` shows what would be used.

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
| `dump` | read every register |
| `reset` | reset the DUT only; `--hold` / `--release` keep it in reset |
| `bench` | how fast the window is; `<region> --both` measures writes too |
| `check` | whether the board and this machine are ready, and what to fix |
| `dma-fire` | the card moves one descriptor itself, either way (PCIe + `dram`); `--pcie-addr` aims it at another card's BAR |
| `run <file>` | a file of commands, holding the link open |
| `cpu …` | firmware and the CPU: `boot`, `console`, and the Debug Module (`halt`, `step`, `regs`, `break`, `mem`, `catch`, …); see [Running firmware](#running-firmware) and [Debugging the CPU](#debugging-the-cpu) |

`hio --help` shows the common commands and options. `hio --list` lists every
command, and `hio help options` explains every option, the probe settings
included.

## The prompt

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

## Over PCIe

| | |
|---|---|
| default | JTAG. A PCIe design has both, so this reaches it too |
| `-p` (= `-t pcie` = `--transport pcie`) | over the BAR; needs root |
| `--bdf 0000:04:00.0` | only when several cards answer to the same ID |

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
than data — with `dram`, read `<bundle>_calib` first. Clear it with
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

## Resetting the DUT

```bash
hio reset                   # reset the DUT and let it run again
hio reset --hold            # keep it in reset
hio reset --release         # let it run
```

**Only the DUT is reset.** The window, memory contents and `reg` registers keep
their state, and a `host_poll_fifo` keeps what it has queued (`drain` it if the
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

## Writing a procedure down

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

## Loading a memory

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
for the whole round trip while the link sits idle. It stops helping past 4 --
about a third of the round trip is serial somewhere the host cannot overlap.
`--threads N` sets it; over JTAG there is one cable to walk, so it stays at 1.

A large transfer is done by the card itself rather than one word at a time
through the window: to fill the memory (`load`, and the pattern `memtest` writes)
the card fetches the data from this machine's memory, and to read it back it
writes the memory into this machine's. That
needs what `check` reports (root, huge pages, and the IOMMU passing this
card's addresses through); without it the window is used instead, and the output
says which it was. Only the memory the card is wired to (the `dram` bundle) goes
this way. `--pio` and `--dma` force one or the other, for telling them apart.

## Running firmware

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
address (`dut_base`), clears the `_SEGGER_RTT` block, and releases the DUT. Up
channel 0 goes to the terminal and keys go to down channel 0; Ctrl-A x
leaves. `hio cpu console <elf>` connects without loading, so leaving and
coming back is harmless.

The exit status is the program's: from `tohost` (1 passes, `(code << 1) | 1`
fails with `code`), or from the first byte on an up channel named `hio-exit`.

`hio cpu boot --halt` stops at the first instruction instead, at the debug
prompt ([The prompt](#the-prompt)), so breakpoints can go in before anything runs; `resume` lets
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

## Debugging the CPU

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
hio cpu catch             # stop on a fault (illegal instruction, access fault, ...)
hio cpu dmi dmstatus      # one DMI register, raw; give a value to write it
```

`--hart N` selects a hart, and `--elf` gives symbols: places by name and PCs
inside functions. These work in a procedure file too; `cpu boot` and `cpu
console` do not, because they take over the terminal.

`mem` goes through the window to the memories `dut_base` places at that
address, so it needs no Debug Module and no halt. A CPU with a data cache may
hold newer data than it shows. A symbol is read whole unless a count is given.

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

A breakpoint takes one of the hart's address triggers. Resuming or stepping
from one first runs its instruction with the trigger off, since the trigger
fires before the instruction runs.

A halt also makes `ebreak` in the program halt into the debugger. CSRs the
Debug Module cannot reach directly go through its program buffer, with `s0`
saved and put back.

## Programming

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

Stop Vivado's `hw_server` first (`pkill hw_server`); it takes the same channel.

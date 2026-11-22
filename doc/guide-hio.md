# Guide: hio

`hio` talks to a generated harness on the board, or in the simulator: it
reads and writes the window, loads memories, runs firmware and debugs a CPU.
Generating the harness is in [guide-harness.md](guide-harness.md). Running,
debugging, saving and restoring a CPU (`hio cpu`, `trace`, `count`) is in
[guide-cpu.md](guide-cpu.md).

- [`id`, `read`, `write`, `dump`: the basics](#id-read-write-dump-the-basics)
- [`hio` with no command: the prompt](#hio-with-no-command-the-prompt)
- [`-p`: over PCIe](#-p-over-pcie)
- [`hio serve` and `--connect`: from another machine](#hio-serve-and---connect-from-another-machine)
- [`hio reset`](#hio-reset)
- [`hio run`: procedure files](#hio-run-procedure-files)
- [`hio load` and `memtest`](#hio-load-and-memtest)
- [`hio program` and `erase`](#hio-program-and-erase)
- [`hio sysmon`: temperature and voltages](#hio-sysmon-temperature-and-voltages)
- [`hio dna`: the FPGA's unique ID](#hio-dna-the-fpgas-unique-id)
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
| `drain <bundle>` | empty a `host_poll_fifo`, oldest first; `--text` prints 8-bit entries as text, such as a serial output |
| `trace …` | a `trace` ring: `start`, `stop`, `status`, `dump`; see [`hio trace`](guide-cpu.md#hio-trace-the-last-instructions) |
| `count …` | a `count`: `start`, `stop`, `status`; see [`hio count`](guide-cpu.md#hio-count-count-instructions-and-mark-two-counts) |
| `dump` | read every register |
| `reset` | reset the DUT only; `--hold` / `--release` keep it in reset |
| `bench` | how fast the window is; `<region> --both` measures writes too; `--suite` measures everything; see [`hio bench`](#hio-bench-how-fast) |
| `check` | whether the board and this machine are ready, and what to fix |
| `dma-fire` | the card moves one descriptor itself, either way (PCIe + `dram`); `--pcie-addr` aims it at another card's BAR |
| `run <file>` | a file of commands, holding the link open (`-` for standard input) |
| `serve` | serve the window to `hio --connect` on other machines |
| `sysmon` | the FPGA's temperature and supply voltages |
| `dna` | the FPGA's DNA, its unique ID |
| `dap …` | the Zynq-7000 PS's memory, through the ARM DAP: `info`, `read`, `write`, `ps7-init` |
| `completions <shell>` | a shell completion script |
| `cpu …` | firmware and the CPU: `boot`, `console`, the Debug Module (`halt`, `step`, `regs`, `break`, `mem`, `catch`, …), `save` and `restore`; see [guide-cpu.md](guide-cpu.md) |

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
- `--connect` reaches only the window, with three exceptions: `program`
  (below), [`hostmem`](#hio-hostmem-host-memory-for-the-dut), and the card's
  DMA for `load` and `memtest` over PCIe: the server holds the page, and the
  data crosses the connection once. `erase`, `probe`, `sysmon`, `dna`, `dap`,
  `program <file>.svf` and `dma-fire` run on the server's machine.
- Over PCIe, the registers that point the card at host memory
  (`<bundle>_base`, `<bundle>_on`, `dma_base`, `dma_size`) belong to the
  server. A batch that writes one is refused: the card would write any
  physical address it is given, the kernel's memory included.
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
Through `hio serve` over PCIe, the page is in the server's machine, so the
client needs no root.

## `hio bench`: how fast

```bash
hio bench --suite --json perf.jsonl
```

`--suite` measures what this design and this transport can do, one after
another, and prints a table: a round trip of the window, batched reads, writes
and reads of each memory region, the card's DMA both ways at a few descriptor
sizes (PCIe with `dram`), and through `hio serve` the copies to and from the
server's host memory. **What is in the memories is lost.** `--json` also adds
the results to a file as one line, with the board, the transport and the `hio`
version, so runs can be compared later.

How fast the DUT itself reaches host memory depends on the DUT, so it is not
part of the suite. `examples/hw/axi/bench.hostmem.hio` shows how to measure it.

## `hio hostmem`: host memory for the DUT

A `host_mem` bundle reaches this machine's memory over PCIe. `hostmem attach`
gives it one huge page and says where it is; the DUT then addresses it from 0
(from `dut_base` on AXI4 with one). Offsets in `hostmem` are bytes in the page.

```
# in a procedure file, run with: sudo hio -p run copy.hio
hostmem attach hmem
hostmem load hmem pattern.bin            # --at <offset>
hostmem write hmem 0x100 1 2             # offset, words
write i_start 1
sleep 10
hostmem read hmem 64 8                   # offset, words
hostmem save hmem out.bin --at 0 --len 4096
```

**The page lives only while the procedure file or the prompt runs**, so
`hostmem` works there and not as a command of its own. When it ends, however it
ends, the bundle is switched off (`<bundle>_on`), its last command is allowed
to finish (`<bundle>_busy`), and only then is the page freed. `hostmem detach`
does the same earlier. It needs what `check` reports for the card's DMA: root,
a free huge page, and the IOMMU passing this card's addresses through. Bus
mastering, which a reboot turns off, is turned on by `attach`.

Through `hio serve` over PCIe, `hostmem` works over `--connect` as well. The
page is then in the server's machine, and the server takes it back the same
way when the client leaves.

What the DUT could not get is counted: `<bundle>_oor` for commands outside the
aperture (or while switched off) and writes with a gap in their strobes,
`<bundle>_failed` for pages the card could not move, with the last code in
`<bundle>_rd_code` / `_wr_code`, and on AXI4 `<bundle>_refused` for bursts it
does not serve (FIXED, narrow bursts).

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

## `hio dna`: the FPGA's unique ID

```bash
hio --target xilinx/vcu118 dna
```

```text
dna       0x4000000001161a603c6122c5
note      bit 0 is the first bit out of TDO
```

It reads the device DNA through its own JTAG instruction, `dna_ir` and
`dna_bits` in the target's `[jtag]` (`FUSE_DNA` and `DNA[<bits>]` in the
BSDL). Like `sysmon`, it needs no bitstream and does not disturb a running
design. A device with several SLRs shows the master SLR. Each read also checks
that the register is `dna_bits` long, so a wrong IR is an error, not a wrong
number.

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

## `hns-host` as a library

`hio` is a thin CLI over the `hns-host` crate, and another Rust program can
use the same parts. Turn the CLI dependencies off:

```toml
[dependencies]
hns-host = { git = "https://github.com/shuntarot/uharness", rev = "<commit>", default-features = false }
```

| Module | What it gives |
|---|---|
| `bus` | `Bus`: open the window over JTAG, PCIe or TCP, run `Batch`es, check the identity |
| `probe` | find the FTDI probe and fill its `ProbeConfig` from a target description |
| `region` | reach a `regs.json` region; `Pager` moves a moving window only when the page changes |
| `program` | send any bitstream over JTAG; reprogram a PCIe card while the machine keeps it |

```rust
use hns_host::{Batch, Bus};

let map = hns_regs::RegisterMap::from_json(&std::fs::read_to_string("regs.json")?)?;
let target = hns_targets::load_file("board.toml".as_ref(), &[])?;
let cfg = hns_host::probe::resolve(&Default::default(), &hns_targets::jtag(&target))?;
let mut bus = Bus::jtag(&cfg, &map, None)?;
bus.check_identity(&map)?;
let mut b = Batch::new();
let magic = b.read(map.get("harness_magic")?.offset as u32);
println!("{:#x}", bus.run(&b)?[magic]);
```

Every error is `hns_host::Error`; its text says what went wrong and what to
change.

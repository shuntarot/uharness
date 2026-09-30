# Guide

How to put a DUT on a board with this tool.

- [The flow](#the-flow)
- [Harness.toml](#harnesstoml)
- [Commands](#commands)
- [What is generated](#what-is-generated)
- [Talking to the board](#talking-to-the-board)
- [Simulation](#simulation)
- [Limits](#limits)

## The flow

```
your Veryl project              generated
  Veryl.toml
  Harness.toml                    hns/Veryl.toml   a project of its own
  src/dut_top.veryl               hns/regs.json    the register map
                                  hns/regs.md      the same map, for reading
                                  hns/src/*.veryl  harness RTL
                                  hns/syn/*        Tcl, XDC, Makefile
```

1. Point `[dut]` at a module **without parameters**. Wrap a parameterised module
   and point at the wrapper.
2. Write `Harness.toml`.
3. `veryl harness check --target <board>` — matches the manifest against the DUT
   and generates nothing.
4. `veryl harness gen --target <board>` — writes `hns/`.
5. `make -C hns/syn` for a bitstream, then `hio program` and `hio id`.

Your DUT source is never modified.

## Harness.toml

Searched as `--config <path>`, then `Harness.toml` upwards from the current
directory. A lowercase `harness.toml` is an error asking you to rename it.

### `[dut]` and `[clock.<port>]`

```toml
[dut]
module = "dut_top"

[clock.i_clk]
freq_mhz = 100
```

One `[clock.<port>]` per clock port of the DUT — the key is the **port** name,
not the domain name. An MMCM is generated to make that frequency from the
board's clock; you do not give divider ratios. Ports in one clock domain must
ask for the same frequency.

**Verilog inside the DUT.** If the DUT instantiates Verilog through
`$sv::<module>`, the DUT project has to bring that file in. `veryl build` lists
only Veryl sources, so paste the file into a Veryl source with `include`:

```veryl
include(inline, "../outputs/Core.v");   // relative to this .veryl file

module dut_top (...) {
    inst u_core: $sv::Core (...);
}
```

The harness does not read the Verilog. `check` names the `$sv::` modules under
"not checked", and a missing file shows up only in synthesis. Do not put files
in the generated `vendor/`: the generator owns that directory.

### `[bundle.<name>]`

A bundle is a group of ports terminated as one thing. **You choose where the
data lives (`backing`); the shape of the port is read from the DUT.**

| key | type | default |
|---|---|---|
| `backing` | see below | **required** |
| `ports` | array, or a table of roles | matched by name |
| `contract` | see below | **follows from the ports**; if written, it is checked |
| `latency` | integer | required for a fixed-latency memory port and for `slave`; only for ports without a handshake |
| `depth` | power of two | 256 for `host_poll_fifo`; for memory, from the address width |
| `addressing` | `"byte"` / `"word"` | fixed-latency memory port only; required when a word is wider than 8 bits |
| `access` | `"indirect"` / `"region"` | fixed-latency memory port only; default `region` |
| `aperture` | size, power of two | a region or an AXI4 port; default is the whole memory. Required for `dram` |

A key the bundle's shape does not use is an error, not ignored.

Sizes accept a suffix: `4k`, `256M` (1024-based). `depth`, `aperture` and
`bar_bytes` all take one.

#### backing — where the data lives

| value | where | |
|---|---|---|
| `reg` | a harness register per port | works |
| `slave` | **inside the DUT**: it offers an addressable interface, the host drives it | works |
| `host_poll_fifo` | a harness FIFO the host polls | works |
| `bram` | FPGA memory | works |
| `bram_preload` | FPGA memory the host fills and the DUT only reads | works |
| `dram` | the board's DRAM | works |
| `host_mem` | host memory | not yet (needs the PCIe requester) |
| `host_irq`, `observe` | | not yet |

#### contract — how refined the port is

**You do not write this.** It follows from the roles the ports carry; writing it
means "check that this is what I think", and a mismatch is an error.

| value | plain ports | addressable ports |
|---|---|---|
| `fixed_latency` | no handshake (needs `latency`) | `addr` `rdata` `wdata` `we` `wstrb` `re` |
| `valid_ready` | `valid` + `ready` | `rd_cmd_*` / `rd_*`, `wr_*` — transfer level |
| `valid_only` | `valid` alone; **beats can be dropped** | — |
| `axi` | — | `modport $std::axi4_if::<..>::master` |

A `valid_only` sink cannot be stalled, so the harness counts what it drops in
`<bundle>_drops`.

Which combinations exist today:

| | `bram` / `bram_preload` | `dram` | `slave` |
|---|---|---|---|
| `fixed_latency` | ✅ | needs an adapter | ✅ |
| `valid_ready` (transfer level) | ✅ | needs an adapter | — |
| `axi` | ✅ | ✅ | — |

`reg` takes `fixed_latency` / `valid_ready` / `valid_only`; `host_poll_fifo`
takes `valid_ready` / `valid_only`.

**Moving from `bram` to `dram` is one word in the manifest** — the DUT does not
change.

#### ports and roles

An array names the ports; a table also fixes their roles.

```toml
ports = ["i_push", "i_data", "o_full"]
ports = { valid = "i_push", ready = "!o_full", data = "i_data" }
```

- A leading `!` means inverted (`ready = "!o_full"`).
- Without `ports`, ports are matched to the bundle by name: strip the direction
  prefix (`i_` / `o_`, per `[lint.naming]`) and match the front of the name.
  Longest match wins; a tie is an error.
- Roles are guessed only from an **exact suffix**: `valid` / `vld`, `ready` /
  `rdy`, `data`, and for memory `addr` / `rdata` / `wdata` / `we` / `wstrb` /
  `be` / `re` / `ren`. Names like `full`, `empty` and `en` are deliberately not
  in the dictionary — a handshake wired the wrong way round drops beats silently.
  When a role is missing, the error names the likely port and shows the `!` form.
- Transfer-level roles are **never guessed**: name all of them.
- A port that matches nothing becomes `data`.

Every port must belong to exactly one bundle. `check` prints how each one was
decided.

### Common shapes

**Registers.** One register per port, in the window.

```toml
[bundle.ctl]
backing = "reg"
ports   = ["i_start", "i_addr", "o_busy", "o_done"]
```

**A stream the DUT pushes.** The host polls it out.

```toml
[bundle.tx]
backing = "host_poll_fifo"
depth   = 16
ports   = { valid = "o_tx_valid", ready = "i_tx_ready", data = "o_tx_data" }
```

**Memory the DUT reads and writes.**

```toml
[bundle.dmem]
backing    = "bram"
latency    = 1
depth      = 1024
addressing = "word"
ports      = { addr = "o_addr", re = "o_en", rdata = "i_rdata",
               wdata = "o_wdata", we = "o_we" }
```

- An entry is as wide as the write channel (`wdata`, or `rdata` if there is
  none). `wdata` may be a power-of-two multiple of `rdata`; a read then selects
  a slice with the middle address bits.
- `wstrb` writes per byte. `re` is a read enable, independent of `we`.
- **Accesses past the end do not wrap**: reads answer 0, writes are dropped, and
  `<bundle>_oor` counts them.
- `bram_preload` must not have `wdata` / `we`.

**An AXI4 master.** Name the interface and nothing else; only `::master`.

```toml
[bundle.mem]
backing = "bram"      # or "dram"
depth   = 256
ports   = ["axi"]
```

For `dram`, the DUT side is 32, 64 or 128 bits wide and the address must be no
wider than the controller's. Accesses are held off until calibration finishes
(tens of ms); `<bundle>_calib` says when. The memory controller's settings
ship with the target, so no Vivado board files are needed.

**A transfer-level port.** "Read N bytes from A", data comes back as beats, no
latency assumed.

| | command | data | done |
|---|---|---|---|
| read | `rd_cmd_valid` `rd_cmd_ready` `rd_cmd_addr` `rd_cmd_size` | `rd_valid` `rd_ready` `rd_data` `rd_last` | the last beat |
| write | `wr_cmd_valid` `wr_cmd_ready` `wr_cmd_addr` `wr_cmd_size` | `wr_valid` `wr_ready` `wr_data` `wr_last` (+ optional `wr_strb`) | `wr_done_valid` |

Name every role of whichever half the DUT has. `*_cmd_size` is a byte count.
Write `<bundle>_delay` (0-255) to stall every transfer that many cycles, and
`<bundle>_jitter` to vary it — FPGA memory is fast and regular, so without this
"the DUT survives a slow responder" is never actually tested.

**A register file inside the DUT.** Same roles as memory; the difference is
direction — `addr` is a DUT input.

```toml
[bundle.csr]
backing = "slave"
latency = 1
ports   = { addr = "i_csr_addr", wdata = "i_csr_wdata",
            we = "i_csr_we", rdata = "o_csr_rdata" }
```

The window it occupies follows from the width of `addr` (`2^width` words), and
the address arrives as a word index.

### How the host sees a memory

Independent of what the DUT's port looks like:

- `region` (default) — a contiguous range in the window; the offset is the
  address. One round trip per word, and no shared state.
- `indirect` — two registers, `<bundle>_maddr` and `_mdata`. Uses two words of
  the window whatever the memory's size, at two round trips per word.

A region is faster at any realistic size. It makes the window as large as the
memory, but that only costs JTAG chain length — a 1 MB window carries 54 bits
per scan against a minimum of 41, where `indirect` costs a whole second round
trip. Over PCIe a large window costs nothing per access; it only has to fit
inside `bar_bytes`, and `check` says so when it does not.

```toml
[bundle.imem]
backing = "bram_preload"
depth   = 256          # a 1 KB region in the window
```

Entry widths must be a multiple of 32 bits. Add `aperture` to show only part of
the memory at a time: the window stays that size and `<bundle>_base_*` chooses
the page. `hio` moves it for you, so the commands do not change.

```toml
[bundle.mem]
backing  = "dram"
aperture = "1M"        # required for dram: 256 MB will not fit a window
ports    = ["axi"]
```

```bash
hio write imem 0x0 0xc0de0000 0xc0de0001
hio read  imem 0x0 2
```

Addresses count from 0 within the region. Anything past the end is refused
before it is sent.

### `[tie_off]`, `[leave_open]`, `[pin]`, `[heartbeat]`, `[pcie]`

```toml
[tie_off]                 # inputs only, value must fit the port
i_mode = 0

[leave_open]              # outputs only
ports = ["o_debug"]

[pin]                     # 1-bit outputs only; the name is a board resource
o_uart_tx = "uart_tx"

[heartbeat]               # a UART the harness itself drives, 8N1
pin  = "uart_tx"
baud = 115200

[pcie]                    # what the endpoint reports, and its BAR
vendor_id = 0x1234
device_id = 0x0001
bar_bytes = 4096
```

- Clock and reset ports go in neither `[tie_off]` nor `[leave_open]`.
- Pin numbers and IOSTANDARD come from the **target description**, never from
  the manifest — otherwise the manifest is tied to one board. See what a board
  offers in `target.description` of `veryl harness check --target <name> --json`.
- The heartbeat repeats one line a second, and **the counter at the end
  increases** — a stopped harness cannot look alive. It works even when no
  transport does.
- `[heartbeat]` and `[pin]` cannot share a resource, and the heartbeat's top
  port `o_<resource>` cannot share a name with a `[pin]` port.

  ```
  hns 5648524e 8ccd6b5e dut_top 07f
      magic    map hash  DUT     line number
  ```

  Read it at the stated baud (`minicom -D /dev/ttyUSB1 -b 115200 -o`); a plain
  `cat` uses whatever the port was set to. On boards where JTAG and the UART are
  two channels of one FTDI, address it as `/dev/serial/by-id/*if01-port0`,
  because `hio` detaching the JTAG channel renumbers `ttyUSB*`.
- The default PCIe IDs are borrowed from an example and are not ours: **choose
  your own before putting a card in a machine**. The BAR must be a power of two,
  at least 4096 bytes, and larger than the window.

## Commands

```
veryl harness check [options]     # match only
veryl harness gen   [options]     # generate
veryl harness update [-o <dir>]   # generate again, with the recorded options
veryl harness targets             # what ships with this build
```

| option | `check` | `gen` | |
|---|---|---|---|
| `--target <provider>/<board>[:<config>]` | ✓ | ✓ | required for `gen`; the start of the name is enough while only one board starts that way (`--target xilinx/vcu`) |
| `--target-file <path>` | ✓ | ✓ | your own description; reported as unverified |
| `--target-patch <path>` | ✓ | ✓ | a TOML patch over it, repeatable |
| `--transport <name>` | ✓ | ✓ | `jtag` (default) or `pcie`; a board without `jtag` defaults to its only transport |
| `--config <path>` | ✓ | ✓ | where `Harness.toml` is |
| `--emit-regs <path>` | ✓ | ✓ | also write the register map there |
| `-o`, `--out-dir <path>` | — | ✓ | default `hns/` beside `Veryl.toml` |
| `--json` | ✓ | ✓ | one JSON document on stdout |
| `-v`, `--verbose` | ✓ | — | every port and register, and what was checked |

Exit codes: 0 success, 1 a diagnostic, 2 a command-line mistake.

`check` prints one line per part (`ok  dut ...`, `ok  bundle ...`), then
**what it did not check** — every time. A failure prints the error and how
to fix it.

### Boards

| `--target` | transports | `dram` |
|---|---|---|
| `digilent/arty-a7-35` | `jtag` | DDR3L, 256 MB |
| `digilent/arty-a7-100` | `jtag` | DDR3L, 256 MB |
| `xilinx/vcu118` | `jtag`, `pcie` | DDR4 |
| `xilinx/kcu105` | `jtag`, `pcie` | DDR4, 2 GB, with `:dr` or `:062` |
| `xilinx/kc705` | `jtag` | DDR3, 1 GB |

The KCU105 carries one of two DDR4 parts, and the memory controller must know
which. Add the config that matches the chips: `:dr` for EDY4016AABG-DR-F
(older boards) or `:062` for MT40A256M16LY-062E (newer boards). Without one,
the board works over JTAG and PCIe, and a `dram` bundle is refused.

## What is generated

The output directory is **a Veryl project of its own**, depending on yours by
path. Its directory name becomes the project name, and Veryl puts that in front
of every module it emits:

```
-o hns  (default)  →  project <dut>_hns   →  <dut>_hns_top
-o arty            →  project <dut>_arty  →  <dut>_arty_top
```

That is how two boards live side by side. The name must be a Veryl identifier.

```bash
veryl harness gen -o hns/arty   --target digilent/arty-a7-35
veryl harness gen -o hns/vcu118 --target xilinx/vcu118 --transport pcie
veryl harness update -o hns/arty
```

`gen` records how it was called in `harness.json`, and `update` repeats exactly
that — retyping the options would let you overwrite one board's directory with
another board's harness. Both **delete generated files that are no longer
generated** (drop `[heartbeat]` and `uart.veryl` goes away). Only files carrying
the `veryl-harness:generated` marker are touched; your own files and the
borrowed Verilog under `vendor/` are left alone.

| file | |
|---|---|
| `Veryl.toml`, `harness.json` | the project, and how it was generated |
| `regs.json`, `regs.md` | the register map, for programs and for people |
| `src/top.veryl` | board pins → clocks → transport → CSR → DUT |
| `src/sim.veryl` | the same core with AXI4-Lite on its ports, for simulation |
| `src/clk.veryl`, `src/csr.veryl`, `src/uart.veryl` | MMCM and reset, registers, heartbeat |
| `syn/*.tcl`, `syn/*.xdc`, `syn/Makefile` | IP, constraints, build flow |
| `vendor/` | borrowed Verilog (PCIe), with `COPYING` and `AUTHORS` |

`make` targets: `bit` (default), `rtl`, `ip`, `program`, `verify`, `clean`.
`VIVADO_BIN` and `VERYL_BIN` override the executables.

For a 7-series DDR3 board (the Arty), the memory controller is generated from
`syn/mig.prj`, a copy of the board file that ships with the target. With
`--target-file`, `[vivado] mig_prj` names a file beside your description; the
board files in `github.com/Xilinx/XilinxBoardStore` have one for each such
board (`boards/<vendor>/<board>/<revision>/mig.prj`).

**No Tcl is generated for reaching the window.** The harness uses its own
BSCANE2 bridge, which Vivado cannot see; use `hio`.

## Talking to the board

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

**Arguments are usually unnecessary.** `regs.json` is found at `./regs.json`
then `hns/regs.json`, and the target is recorded inside it. A map generated with
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
| `dma-fire` | the card moves one descriptor itself, either way (PCIe + `dram`) |
| `run <file>` | a file of commands, holding the link open |

`hio --help` shows the common commands and options. `hio --list` lists every
command, and `hio help options` explains every option, the probe settings
included.

### Over PCIe

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

### Resetting the DUT

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

### Writing a procedure down

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

### Loading a memory

```bash
hio load dmem prog.elf --base 0x80000000 --verify
```

`--base` says which DUT address entry 0 is, and there is **no default**: get it
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

### Programming

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

## Simulation

`sim` is the harness top without transport or clock generation, with AXI4-Lite
on its ports, so a testbench can drive it directly. `top` cannot be simulated
(it holds the MMCM and the JTAG primitive). Everything below the CSR is the same
generated code in both. `cargo test` runs this. See `dev/design-sim.md` for what
it does not cover.

## Limits

A generated harness passing synthesis does not mean **your DUT** does; RTL
written for simulation can fail there on its own.

- The CSR is one clock domain: `reg` ports must share it.
- A `latency` you state is used as given; nothing checks it against the DUT.
- Writes wider than 32 bits are not atomic — the DUT sees intermediate words.
  (`valid_ready` bundles write data before `valid`, and memory windows commit on
  the top word, so neither is affected.)
- `rdata` wider than `wdata` is not supported, nor is a second write channel.
- Unaligned accesses across an entry boundary are not detected.
- Where a memory has no `re`, `<bundle>_oor` counts cycles rather than accesses.
- Veryl simulates two-valued, so X-related mismatches with hardware do not show.

Errors state what to do. Two worth knowing in advance: a leftover broken `hns/`
makes the analyser fail on it rather than on your DUT (delete it and generate
again), and a hash mismatch in `verify` means the bitstream and the map are from
different generations.

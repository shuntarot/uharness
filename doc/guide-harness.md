# Guide: the harness

How to put a DUT on a board with this tool: the manifest, generation, and
what comes out. Talking to the board afterwards is in
[guide-hio.md](guide-hio.md).

- [`gen`, then `make`: the flow](#gen-then-make-the-flow)
- [Harness.toml](#harnesstoml)
- [`veryl harness` commands](#veryl-harness-commands)
- [`gen` output: what is generated](#gen-output-what-is-generated)
- [`sim`: the harness in a testbench](#sim-the-harness-in-a-testbench)
- [Limits](#limits)

## `gen`, then `make`: the flow

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
5. `make -C hns/syn` for a bitstream, then `hio program` and `hio id`
   ([guide-hio.md](guide-hio.md)).

Your DUT source is never modified.

## Harness.toml

`--config <path>` if given, else `Harness.toml` beside the project's
`Veryl.toml` (found upwards from the current directory, as `veryl` does). A
lowercase `harness.toml` is an error asking you to rename it.

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
| `latency` | integer | required for a fixed-latency memory port, for `slave`, and wherever `contract = "fixed_latency"` is written; only for ports without a handshake |
| `depth` | power of two | 256 for `host_poll_fifo`; for memory, from the address width; for `dram`, the whole memory; **required** for `trace` |
| `addressing` | `"byte"` / `"word"` | fixed-latency memory port only; required when a word is wider than 8 bits |
| `access` | `"indirect"` / `"region"` | fixed-latency memory port only; default `region` |
| `aperture` | size, power of two | a region or an AXI4 port; default is the whole memory. Required for `dram` |
| `window_base` | byte offset in the window | a region only (`region` memory, `dram`, `slave`); default is the first free place |
| `dut_base` | byte address | a memory; the DUT address of its first byte, default 0. A multiple of 4 KB on AXI4 |
| `reset` | port name | `dmi` only: the Debug Module's own reset port |
| `format` | `"rvfi"` | `trace` only: the records are RVFI; names and widths are checked |
| `trigger` | array of ports | `trace` only: the ports `hio trace start --when` may test, from `ports`; only these get a comparator. Default: none |

A key the bundle's shape does not use is an error, not ignored.

Sizes accept a suffix: `4k`, `256M` (1024-based). `depth`, `aperture`,
`window_base` and `bar_bytes` all take one.

#### backing — where the data lives

| value | where | |
|---|---|---|
| `reg` | a harness register per port | works |
| `slave` | **inside the DUT**: it offers a RAM port, an APB slave or an AXI4 slave, the host drives it | works |
| `dmi` | **inside the DUT**: a RISC-V Debug Module's DMI, an APB slave the host drives | works |
| `host_poll_fifo` | a harness FIFO the host polls | works |
| `trace` | a ring in FPGA memory that keeps the last records of a stream, such as an execution trace | works |
| `bram` | FPGA memory | works |
| `bram_preload` | FPGA memory the host fills and the DUT only reads | works |
| `dram` | the board's DRAM | works |
| `host_mem` | host memory | not yet (needs a PCIe requester for the DUT) |
| `host_irq` | the DUT's interrupt line, sent to the host as INTA (PCIe) | works; one 1-bit output, at most one bundle |
| `pcie_flr` | the host's Function Level Reset, handed to the DUT (PCIe) | works; one 1-bit input, optionally one 1-bit done output |
| `observe` | | not yet |

#### contract — how refined the port is

**You do not write this.** It follows from the roles the ports carry; writing it
means "check that this is what I think", and a mismatch is an error.

| value | plain ports | addressable ports |
|---|---|---|
| `fixed_latency` | **wires**: no handshake (needs `latency`) | **RAM port**: `addr` `rdata` `wdata` `we` `wstrb` `re`, read data a fixed number of cycles later, as from an FPGA block RAM |
| `valid_ready` | **stream**: `valid` + `ready` | **request / response**: `rd_cmd_*` / `rd_*`, `wr_*`; the answer comes whenever it is ready |
| `valid_only` | **stream, no ready**: `valid` alone; **beats can be dropped** | — |
| `axi` | — | **AXI4**: `modport $std::axi4_if::<..>::master` (`::slave` for `slave`) |
| `apb` | — | **APB**: `psel` `penable` `pwrite` `paddr` `pwdata` `prdata` (`pready`, `pslverr`) — `slave` and `dmi` |

A `valid_only` sink cannot be stalled, so the harness counts what it drops in
`<bundle>_drops`. A `trace` drops nothing: it overwrites its oldest record.

Which combinations exist today, by the shapes above:

| backing | wires<br>`fixed_latency` | RAM port<br>`fixed_latency` | stream<br>`valid_ready` | request / response<br>`valid_ready` | stream, no ready<br>`valid_only` | AXI4<br>`axi` | APB<br>`apb` |
|---|---|---|---|---|---|---|---|
| `reg` | ✅ | | ✅ | | ✅ | | |
| `slave` | | ✅ | | | | ✅ | ✅ |
| `dmi` | | | | | | | ✅ |
| `host_poll_fifo` | | | ✅ | | ✅ | | |
| `trace` | | | | | ✅ | | |
| `bram` | | ✅ | | ✅ | | ✅ | |
| `bram_preload` | | ✅ read only | | | | | |
| `dram` | | | | ✅ | | ✅ | |
| `host_irq` | ✅ | | | | | | |
| `pcie_flr` | ✅ | | | | | | |

Anything not marked is refused when the harness is generated. On a RAM port
the backings differ only in who holds the memory: with `bram` and
`bram_preload` the DUT drives the port and the harness holds the memory; with
`slave` the DUT holds it (a register file) and the host drives the port.

- `slave` on a RAM port: `latency` 0 to 64, and `rdata` / `wdata` up to 32
  bits. An APB slave waits with `pready` instead.
- `dram` answers when it can, so the DUT must be able to wait: AXI4 or
  request / response. A RAM port cannot wait, so it cannot use `dram`.
- `host_irq` and `pcie_flr` need the PCIe transport: one 1-bit port (plus a
  1-bit done output for `pcie_flr`), at most one bundle each.
- `host_mem` and `observe` are accepted by `check` but not generated yet.

**On an AXI4 or a request / response port, moving from `bram` to `dram` is one
word in the manifest** — the DUT does not change (`dram` also needs
`aperture`).

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

### Common `[bundle]` shapes

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

**An execution trace.** The ring keeps the last `depth` instructions. A
record is the ports of the bundle, lowest first, above a cycle count the
harness adds. With `format = "rvfi"` the names are checked against the
RISC-V Formal Interface (`rvfi_valid`, `rvfi_pc_rdata`, `rvfi_insn`, ...), and
`hio trace dump` prints instructions. Name only the ports you need: each bit
costs `depth` bits of block RAM. `trigger` names the ports a trigger may
test, as a logic analyzer fixes its trigger probes before synthesis; the
others are recorded but cannot trigger. The fields are named after the ports,
without the direction prefix and the bundle name (`o_rvfi_pc_rdata` is
`pc_rdata`).

```toml
[bundle.rvfi]
backing = "trace"
format  = "rvfi"
depth   = 1024
ports   = ["o_rvfi_valid", "o_rvfi_pc_rdata", "o_rvfi_insn", "o_rvfi_mode",
           "o_rvfi_rd_addr", "o_rvfi_rd_wdata", "o_rvfi_trap", "o_rvfi_intr"]
trigger = ["o_rvfi_pc_rdata", "o_rvfi_trap"]
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
- `dut_base = 0x8000_0000` lets the DUT use its own addresses: the harness
  subtracts it, and an address below it is out of range. `hio load` reads it
  from `regs.json`. Two memories on different ports may share addresses, as a
  CPU's fetch and data ports do.

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

A core with another bus converts it to AXI4 inside the DUT, with a bridge from
the `hns` package. `hns::ahb2axi` takes an AHB-Lite manager (32 bits, one
transfer at a time); add `hns` to your `Veryl.toml` to use it.

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

The address is cut to a whole beat and the size rounded up to whole beats. A
read that runs off the end returns the beats inside, `rd_last` on the last of
them; a write takes every beat and stores the ones inside. Either is counted
in `<bundle>_oor`. A read with no beat inside returns nothing, so a DUT
waiting for `rd_last` waits forever.

With `backing = "dram"` the same port is turned into AXI4 bursts (at most 256
beats, never across 4 KB) on the board's memory. `wr_done_valid` then comes
once the memory has taken every burst. `<bundle>_delay` is not there: the
memory is slow and uneven by itself.

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
the address arrives as a word index. Name every role in `ports`; none is
guessed for a RAM port.

- `latency` is how many cycles after `addr` the DUT's `rdata` holds the
  answer, 0 to 64. With 0 the harness holds the answer itself; with more, the
  DUT's read pipeline must advance every cycle.
- `rdata` and `wdata` can be narrower than 32 bits: a write gives the low bits,
  and a read comes back with zeros above them.

The same with an APB slave. Ports are found by their `p*` suffix:

```toml
[bundle.csr]
backing = "slave"     # i_csr_psel, i_csr_penable, i_csr_pwrite, i_csr_paddr,
                      # i_csr_pwdata, o_csr_prdata, o_csr_pready, o_csr_pslverr
```

- `paddr` is a byte address of 32-bit registers, 2 to 31 bits; the window
  gives it `2^width` bytes. Without `pready` the slave answers in the access
  phase.
- One host access is one APB transfer; nothing is read ahead or twice.
- A transfer the slave ends with `pslverr` is counted in `<bundle>_slverr`
  (read only, saturating). A write is answered before its transfer ends, so
  the error cannot come back with it: read the count before and after.

And with an AXI4 slave: a `::slave` modport port, found by its type.

```toml
[bundle.csr]
backing = "slave"
ports   = ["csr"]     # csr : modport $std::axi4_if::<..>::slave
```

- Each host access is one single-beat AXI4 transfer of one 32-bit word; on a
  wider bus the other strobes are off and the read takes its part of the beat.
- The window shows the whole address space (`2^width` bytes), up to 31 bits.
  A wider one needs `aperture`, a moving window as for `dram`.
- The bus must be 32 bits or a power-of-two multiple.

While the DUT is held in reset (`hio reset --hold`, or during `hio reset`),
an APB or AXI4 slave is not driven: it may not answer then. `hio` refuses to
touch such a region until `hio reset --release`; `regs.json` marks it
`held_in_dut_reset`. Another host that tries anyway gets no answer, and the
window ends the access after 10 µs with an error, counted in `timeouts`.
Nothing asked then reaches the DUT later.

**A RISC-V Debug Module.** Its DMI, as the APB slave the Debug Module offers
(Hazard3's `hazard3_dm` does). Ports are found by their `p*` suffix.

```toml
[bundle.dmi]
backing = "dmi"       # i_dmi_psel, i_dmi_penable, i_dmi_pwrite, i_dmi_paddr,
                      # i_dmi_pwdata, o_dmi_prdata, o_dmi_pready
reset   = "i_dm_rst"  # the Debug Module's own reset port
```

- `paddr` is a byte address, 9 to 16 bits; the window gives it `2^width`
  bytes. Without `pready` the slave answers in the access phase. `pslverr` is
  not taken: a Debug Module reports errors in its own registers.
- One host access is one APB transfer. Nothing is read ahead or twice, because
  reading a Debug Module register can change its state.
- The region is marked `dmi` in `regs.json`, which is how `hio cpu` finds it.
- `reset` names the Debug Module's reset port. It gets the harness reset, not
  the DUT reset, so `hio reset` leaves a halted hart, triggers and a
  halt-on-reset request in place. Without it the Debug Module is on the DUT
  reset and `hio reset` clears it. Inside the DUT, reset the core with the DUT
  reset *or* the Debug Module's `ndmreset` / hart reset.

### `access`: `region` or `indirect`

Independent of what the DUT's port looks like:

- `region` (default) — a contiguous range in the window; the offset is the
  address. One round trip per word, and no shared state.
- `indirect` — two registers, `<bundle>_maddr` and `_mdata`. Uses two words of
  the window whatever the memory's size, at two round trips per word.

A region is faster at any realistic size: a larger window adds only a few bits
to each JTAG scan, while `indirect` costs a second round trip per word. Over
PCIe the region has to fit inside `bar_bytes`; `check` says so when it does
not.

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

A region goes in the first free place that is a multiple of its size. Set
`window_base` when software expects it at a fixed offset, such as a driver reading its
registers at BAR offset 0; `check` refuses one that is unaligned or overlaps.

```toml
[bundle.nvme_regs]
backing     = "slave"
window_base = 0
```

Addresses count from 0 within the region. Anything past the end is refused
before it is sent.

### `[tie_off]`, `[leave_open]`, `[pin]`, `[heartbeat]`, `[pcie]`, `[vivado]`

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
vendor_id  = 0x1234
device_id  = 0x0001
class_code = 0xff0000     # base / sub / interface, as `lspci -n` shows; default: no class
bar_bytes  = 4096
perst_resets_dut = true   # PERST# holds the DUT in reset (never the harness)

[vivado]                  # your own Tcl for synthesis
tcl = "vivado.tcl"        # relative to Harness.toml
```

- `perst_resets_dut = false` keeps the DUT running through a host reboot.
  Either way the harness is not reset, and JTAG still reaches the window.
- A `pcie_flr` bundle makes the card FLR-capable. Its input is high during
  the FLR; what it resets is up to the DUT. With a done output, the input
  stays high until the DUT raises it (and the next FLR waits for it to fall);
  without one, the input is high for 256 cycles. A hot reset does not reach
  the DUT.
- `class_code` is a warning, not an error, when a host driver binds to that
  class (AHCI, NVMe, USB hosts, a PCI bridge ...): the driver would drive the
  window as that device. `hio -p check` compares it with what the card reports.
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
- `[vivado] tcl` is copied to `syn/user.tcl`. `synth.tcl` sources it just
  before `synth_design`. Each step runs with `-directive` from a variable,
  which is `Default` unless your file sets it:

  ```tcl
  # vivado.tcl: settings for a congested design on a large device
  set SYNTH_DIRECTIVE    AlternateRoutability
  set OPT_DIRECTIVE      Explore
  set PLACE_DIRECTIVE    SSI_SpreadLogic_high
  set PHYS_OPT_DIRECTIVE AggressiveExplore
  set ROUTE_DIRECTIVE    AlternateCLBRouting
  ```

  The file can hold any other Tcl too (`set_param`, more XDC). It is the same
  for every board; check `$part` in it if a board needs other settings.

## `veryl harness` commands

```
veryl harness check [options]     # match only
veryl harness gen   [options]     # generate
veryl harness update [-o <dir>] [--json]   # generate again, with the recorded options
veryl harness sim [-o <dir>]      # run a `--target sim` harness for hio
veryl harness targets             # what ships with this build
```

| option | `check` | `gen` | |
|---|---|---|---|
| `--target <provider>/<board>[:<config>]` | ✓ | ✓ | required for `gen`; the start of the name is enough while only one board starts that way (`--target xilinx/vcu`) |
| `--target-file <path>` | ✓ | ✓ | your own description; reported as unverified |
| `--target-patch <path>` | ✓ | ✓ | a TOML patch over it, repeatable |
| `--transport <name>` | ✓ | ✓ | `jtag` (default) or `pcie`; a board without `jtag` defaults to its only transport (`sim` for `--target sim`) |
| `--config <path>` | ✓ | ✓ | where `Harness.toml` is |
| `--emit-regs <path>` | ✓ | — | also write the register map there (`gen` always writes `regs.json`) |
| `-o`, `--out-dir <path>` | — | ✓ | default `hns/` beside `Veryl.toml` |
| `--json` | ✓ | ✓ | one JSON document on stdout |
| `-v`, `--verbose` | ✓ | — | every port and register, and what was checked |

Exit codes: 0 success, 1 a diagnostic, 2 a command-line mistake.

`check` prints one line per part (`ok  dut ...`, `ok  bundle ...`), then
**what it did not check** — every time. A failure prints the error and how
to fix it.

### `--target`: boards

| `--target` | transports | `dram` | tested |
|---|---|---|---|
| `digilent/arty-a7-35` | `jtag` | DDR3L, 256 MB | ✓ |
| `digilent/arty-a7-100` | `jtag` | DDR3L, 256 MB | not yet |
| `digilent/pynq-z1` | `jtag` | DDR3 through the PS, 256 MB | ✓ |
| `digilent/zybo` | `jtag` | DDR3 through the PS, 256 MB | ✓ |
| `xilinx/vcu118` | `jtag`, `pcie` | DDR4, 2 GB | ✓ |
| `xilinx/kcu105` | `jtag`, `pcie` | DDR4, 2 GB, with `:dr` or `:062` | ✓ |
| `xilinx/kc705` | `jtag` | DDR3, 1 GB | not yet |
| `alibaba/as02mc04` | `jtag`, `pcie` | — (no DRAM) | ✓ |
| `sim` | `sim` | — (no board; see [`--target sim` and `veryl harness sim`](#--target-sim-and-veryl-harness-sim)) | — |

**tested** means the board has run a harness. A board marked "not yet" builds,
but `check` and `gen` print a note that it has not run on hardware.

The AS02MC04 has no USB JTAG of its own. Its description names a Digilent
JTAG-HS2 wired to the card's 6-pin header; another FTDI adapter works through
`hio --vid/--pid/--product/--layout-init`. It has no USB-UART either, so
`[heartbeat]` has no pin there.

On the PYNQ-Z1 and the Zybo the DDR3 belongs to the Zynq's PS. A `dram` bundle
reaches it through the PS's S_AXI_HP0 port: the upper 256 MB, on the harness
clock. No program runs on the CPUs; the PS is set up over JTAG instead, and
`hio program` does that after the bitstream (`ps7_init.tcl`, beside the
bitstream, from the PS settings that ship with the target). The USB-UART is the
PS's too, so `[heartbeat]` has no pin. Set the boot jumper to JTAG, so a
bitstream from the SD card does not replace yours (JP4 on the PYNQ-Z1, JP5 on
the Zybo).

The KCU105 carries one of two DDR4 parts, and the memory controller must know
which. Add the config that matches the chips: `:dr` for EDY4016AABG-DR-F
(older boards) or `:062` for MT40A256M16LY-062E (newer boards). Without one,
the board works over JTAG and PCIe, and a `dram` bundle is refused.

## `gen` output: what is generated

The output directory is **a Veryl project of its own**, depending on yours by
path. Its directory name becomes the project name, and Veryl puts that in front
of every module it emits:

```
-o hns  (default)  →  project <dut>_hns   →  <dut>_hns_top
-o arty            →  project <dut>_arty  →  <dut>_arty_top
```

That is how two boards live side by side. The name must be a Veryl identifier.

The harness also uses the fixed parts in the Veryl package `hns`. If your
`Veryl.toml` does not name it, the output fetches the published version from
GitHub (`shuntarot/uharness`, `rtl/hns`). To use a local checkout instead, add
it yourself; a path needs no publishing:

```toml
[dependencies]
hns = { path = "/path/to/uharness/rtl/hns" }
```

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

`make` targets: `bit` (default), `rtl`, `ip`, `program`, `svf`, `area`, `zip`,
`clean`. `VIVADO_BIN` and `VERYL_BIN` override the executables.

A board whose target description has `[flash]` (the flash the FPGA boots from)
also gets `make mcs`, which makes the flash image from the bitstream, and
`make flash`, which writes it. The bitstream is built to boot from that flash,
compressed so that it starts sooner. `make flash` needs the board on the
machine's JTAG cable, where Vivado Lab is enough
(`make flash VIVADO_BIN=vivado_lab`). **It erases the whole flash first**, so
read it back first if what is on it matters.

For a 7-series DDR3 board (the Arty), the memory controller is generated from
`syn/mig.prj`, a copy of the board file that ships with the target. With
`--target-file`, `[vivado] mig_prj` names a file beside your description; the
board files in `github.com/Xilinx/XilinxBoardStore` have one for each such
board (`boards/<vendor>/<board>/<revision>/mig.prj`). A Zynq board's PS is
built the same way, from the `CONFIG.PCW_*` settings `[vivado] ps7_preset`
names.

**No Tcl is generated for reaching the window.** The harness uses its own
BSCANE2 bridge, which Vivado cannot see; use `hio`.

## `sim`: the harness in a testbench

`sim` is the harness top without transport or clock generation, with AXI4-Lite
on its ports, so a testbench can drive it directly. `top` cannot be simulated
(it holds the MMCM and the JTAG primitive). Everything below the CSR is the same
generated code in both. `cargo test` runs this. It does not cover the MMCM
and the JTAG primitive (the clocks, the reset release and the clock
crossings), timing, or the board's pins; synthesis and the board do.

### `--target sim` and `veryl harness sim`

`--target sim` runs the harness in the Veryl simulator, and `hio` talks to it
over a TCP socket, on this machine unless `--addr` says otherwise. The same
`regs.json`, commands and procedure files work.

```bash
veryl harness gen --target sim     # writes hns/, as for a board
veryl harness sim                  # builds, then waits for hio; Ctrl-C stops it
hio id                             # in another terminal
hio run bringup.hio
```

- The simulation starts at the first `hio` connection, so nothing after the
  reset release is missed. It then keeps running between commands.
- `veryl harness sim` builds a small Rust component with **cargo**; the first
  build fetches `veryl-component` from crates.io. Plain `veryl test` in the
  directory runs it too, but leaves `sim.addr` behind after Ctrl-C, and uses
  whatever `veryl` is on `PATH`.
- Run inside a generated directory, `veryl harness sim` takes that one;
  elsewhere, `-o <dir>` names it (default `hns/`). A sim harness may sit
  inside a board one (`-o hns/sim`): generating the board one again leaves
  it alone.
- It listens on a free port of `127.0.0.1` and writes the address to
  `hns/sim.addr`, where `hio` finds it. `--addr <ip:port>` (or
  `HNS_SIM_ADDR`) listens elsewhere.
- From another machine, start it with `--addr 0.0.0.0:5555`, copy its
  `regs.json`, and run `hio --regs regs.json --connect <host>:5555 ...`
  (or set `HIO_CONNECT`). There is no password: anyone who reaches the
  port can drive the harness. The protocol is [HNS2](hns2.md); one client
  is served at a time.
- `veryl harness sim --watch` prints the simulated clock rate every second
  (`HNS_SIM_WATCH=1` does the same). `--wave` dumps a waveform, as
  `veryl test --wave` does.
- A DUT with several clocks runs with each clock at its `freq_mhz`, so their
  ratio holds; the DUT reset reaches every domain.
- `dram` is a 256 MB memory that holds only the pages written; unwritten
  memory reads as 0. Without `depth`, the region is all 256 MB. It answers at
  once and never needs calibration, so it does not show the timing of a real
  controller.
  Another size is a target patch: `[provides.dram]` with `axi_addr_bits = 32`
  for 4 GB, passed as `--target sim --target-patch <file>`.
- `check` refuses what the simulator cannot run: a DUT with a `$sv::` module.
  `pcie` and DMA are board-only.

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

Errors state what to do. One worth knowing in advance: a hash mismatch in
`hio id` means the bitstream and the map are from different generations.

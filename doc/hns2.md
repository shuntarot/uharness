# HNS2: the window over TCP

HNS2 runs reads and writes on a harness window over a TCP connection. Two
servers speak it:

- `veryl harness sim`, for a harness generated with `--target sim`;
- `hio serve`, on the machine a board is connected to (JTAG or PCIe).

`hio` is a client (`hio --connect <host:port>`). Any other program that
speaks HNS2 can drive a harness too: a test script, a debugger bridge, a
GUI. Addresses are window addresses, as `regs.json` lists them.

HNS2 has no password and no encryption. A server listens on `127.0.0.1` by
default; reach it from another machine through `ssh -L`.

## Bytes

All integers are little endian. `u8`, `u32` are 1 and 4 bytes.

After the greeting, both sides send **messages**:

```text
kind: u8    what the message is
len:  u32   the number of bytes in body
body: len bytes
```

A receiver that does not know `kind` can skip `len` bytes. A server answers an
unknown kind with `ERROR` and closes the connection.

## Greeting

```text
client -> server   "HNS2" (4 bytes)
server -> client   "HNS2" (4 bytes), then one message: INFO or ERROR
```

After `ERROR`, the server closes the connection. A server that does not speak
HNS2 closes it without an answer.

## Messages

### Client to server

| kind | name | body |
|---|---|---|
| `0x01` | `BATCH` | `flags: u8`, `count: u32`, then `count` ops |
| `0x02` | `FINISH` | empty |

An op is 9 bytes: `op: u8`, `addr: u32`, `data: u32`.

- `op` 1 reads the 32-bit word at `addr`. `data` is ignored (send 0).
- `op` 2 writes `data` to `addr`, all four bytes.
- `addr` is a multiple of 4.

`flags` bit 0 is **replayable**: running the batch twice gives the same
result. Set it only for memory (`bram`, `dram`), never for registers, whose
reads can change state. A JTAG server may send a command again when the
window seemed busy; without this bit it does not, and reports an error
instead. Other bits are 0.

The ops run in order. The server answers each `BATCH` with one `READS` or one
`ERROR`, and runs the next batch only after that. The client may send the
next batch before the answer arrives.

`FINISH` ends a simulation, as Ctrl-C would. The server answers `READS` with
no values, then stops. A board server answers `ERROR`.

### Server to client

| kind | name | body |
|---|---|---|
| `0x80` | `INFO` | a JSON object, UTF-8 |
| `0x81` | `READS` | `count: u32`, then `count` x `data: u32` |
| `0x82` | `ERROR` | `op: u32`, `resp: u8`, then a message, UTF-8 |

`READS` has one value per read op of the batch, in order. Writes have none.

`ERROR`:

- `op` is the index of the op that failed in the batch, or `0xffffffff`
  when no single op failed (busy, a lost transport, a bad message).
- `resp` is the AXI response of that op (2 `SLVERR`, 3 `DECERR`), or 0
  when the failure was not an AXI response.
- The ops before `op` ran. The ops after it did not.
- The message is for a person: what went wrong, and what to do.

After an `ERROR` for a batch, the connection stays open. The client decides
whether to go on.

`INFO` keys:

| key | value |
|---|---|
| `server` | the program and its version, e.g. `"hio 0.4.0"` |
| `kind` | `"sim"` or `"board"` |
| `target` | the target name, e.g. `"arty"`; `"sim"` for a simulation |
| `master` | `"jtag"` or `"pcie"`: which master the window sees |
| `map_hash` | the map hash of the `regs.json` the server uses, or `null` |
| `max_ops` | the most ops one `BATCH` may carry |

A client ignores keys it does not know. `master` matters for windows with a
movable base: there is one base register per master.

## One client at a time

A server serves one connection at a time. A command that takes several
batches (loading memory, a debug step, a console) would break if another
client ran batches in between. A second client waits up to a second for the
first to leave (one command often connects right after another), then gets
`ERROR` at the greeting and the connection is closed.

A quiet client keeps its turn: a console may wait for a long time. To free
the server from a client that vanished without closing (a pulled cable),
`hio serve` turns on TCP keepalive and drops a peer that has not answered
for about a minute.

## Versions

A change that old clients or servers would misread gets a new greeting
(`HNS3`). New message kinds and new `INFO` keys are not such a change.

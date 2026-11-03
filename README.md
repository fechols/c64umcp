# c64umcp

A Rust MCP server for controlling a Commodore 64 Ultimate over its LAN REST API.
Runs locally over **stdio**, with 45 typed tools for programs, drives, configuration,
memory, REU uploads, input, and screenshots. One device per process.

**Status:** 79 offline tests pass. A scratch-session campaign on C64 Ultimate
firmware 1.1.0 passed 32 tools through the real stdio MCP server, including program
uploads, cartridge launch, RAM writes, drive/configuration changes, machine controls,
and screenshots. A separate 60-cycle soak completed without failures. Five tools
depend on unavailable input/menu endpoints; seven operations were excluded from
hardware testing. See the [results and coverage](docs/hardware-results.md).

## Build and connect

Requires Rust 1.88 or newer.

```powershell
cargo build --release --locked
$env:C64U_HOST = "192.0.2.64" # replace with your device address
# If a Network Password is set on the device:
$env:C64U_PASSWORD = "your-password"
.\target\release\c64umcp.exe
```

Enable **Web Remote Control Service** in the device's network settings.
The executable speaks MCP on stdin/stdout; it is not an interactive shell.
Diagnostics go to stderr. Nothing connects to the device until a tool is called.
With no host configured, startup and tool discovery work; device calls explain how to configure it.

A generic MCP client's server configuration:

```json
{
  "mcpServers": {
    "c64u": {
      "command": "C:/Work/Projects/GitHub/c64umcp/target/release/c64umcp.exe",
      "args": [],
      "env": {
        "C64U_HOST": "192.0.2.64"
      }
    }
  }
}
```

Use your actual executable path and device address. Supply `C64U_PASSWORD` through
your client's environment if needed. Passwords are not command-line arguments.
Restart the server after changing its environment.

| Option | Environment | Default |
| --- | --- | --- |
| `--host` | `C64U_HOST` | Unconfigured |
| `--timeout` | `C64U_TIMEOUT` | 30 seconds; range 1–300 |
| `--capture-ip` | `C64U_CAPTURE_IP` | Infer this computer's IPv4 interface |
| `--capture-port` | `C64U_CAPTURE_PORT` | 11000 (0 picks a free port per capture) |
| `--capture-timeout` | `C64U_CAPTURE_TIMEOUT` | 5 seconds; range 1–60 |

CLI options override environment values. Host accepts a hostname, IP, optional
port, or HTTP(S) root URL, with optional `/v1`. It does not accept credentials,
query strings, fragments, or arbitrary URL paths. LAN requests bypass system proxies.
`RUST_LOG` controls stderr tracing.

## Tools

| Area | Tools |
| --- | --- |
| Discovery | `get_version`, `get_info` |
| Programs/music | `load_prg`, `run_prg`, `run_crt`, `play_sid`, `play_mod` |
| Configuration | `list_config_categories`, `get_config`, `set_config`, `set_config_batch`, `load_config_from_flash`, `save_config_to_flash`, `reset_config_defaults` |
| Machine | `reset_machine`, `reboot_machine`, `pause_machine`, `resume_machine`, `poweroff_machine`, `toggle_menu` |
| Memory/debug | `read_memory`, `write_memory`, `upload_reu`, `read_debug_register`, `write_debug_register` |
| Drives | `list_drives`, `mount_disk`, `eject_disk`, `unlink_disk`, `reset_drive`, `enable_drive`, `disable_drive`, `load_drive_rom`, `set_drive_mode` |
| Files | `file_info`, `create_disk_image` |
| Input | `get_input_state`, `send_input`, `release_all_inputs`, `type_text` |
| Screens/streams | `read_text_screen`, `read_menu_screen`, `capture_screenshot`, `start_stream`, `stop_stream` |

Tools advertise JSON schemas and read-only/destructive annotations. Results include
structured JSON and readable text. Screenshots return MCP image content.
Machine operations and entire input/capture workflows are serialized per server.
Annotations describe effects; they are not a separate permission system.
On a normal MCP stdin close, the server waits (up to 10 seconds) for active
handlers and bounded keyboard/video cleanup before exiting.

### Program and disk sources

Source kind is explicit; local paths are never mistaken for device paths.
Local paths must be absolute, because the server's working directory is
unrelated to the client's, and must name a regular file.

```json
{"source":{"kind":"device","path":"/Usb0/demo.prg"}}
{"source":{"kind":"local","path":"C:/build/demo.prg"}}
{"source":{"kind":"base64","filename":"demo.prg","data":"AQgAAA=="}}
```

Use these as `run_prg` arguments; `load_prg` loads without running.
PRG uploads require a little-endian load address and must fit in C64 memory.
Local files are read from the **server computer**. Uploads/responses are bounded
to 32 MiB; multipart overhead counts toward that limit.
No compiler or assembler is included.

`play_sid` additionally accepts `songnr` (omitted/zero means default) and
an optional local/base64 `song_lengths` attachment.
`play_mod` may overwrite REU memory and requires sampler support.

Mount an image:

```json
{
  "drive":"a",
  "source":{"kind":"device","path":"/Usb0/game.d64"},
  "mode":"readonly"
}
```

Drive names are `a` and `b`. Mount defaults to `readonly`; explicitly request
`readwrite` to persist disk changes, or `unlinked` for nonpersistent writes.
Optional `image_type`: `d64`, `g64`, `d71`, `g71`, `d81`.
`set_drive_mode` accepts `1541`, `1571`, `1581`.
Uploaded drive ROMs must be 16 or 32 KiB.

### Memory and configuration

```json
{"address":"$0400","length":256}
{"address":"D020","data_hex":"0504"}
{"category":"Drive A Settings","item":"Drive Bus ID"}
{"category":"Drive A Settings","item":"Drive Bus ID","value":8}
{"settings":{"Drive A Settings":{"Drive":"Enabled","Drive Bus ID":8}}}
```

These are arguments for `read_memory`, `write_memory`, `get_config`,
`set_config`, and `set_config_batch`, respectively.
Addresses are hexadecimal strings, with optional `$` or `0x`.
Reads default to 256 bytes; ranges cannot cross $FFFF.
Reads return hex and base64. Writes automatically use PUT up to 128 bytes and
binary POST for larger blocks. Banking affects DMA access; these are not CPU
register or debugger APIs.

Configuration values are strings or integers. Categories and item names are
discovered dynamically, including wildcard queries. Changes do not automatically
save to flash. Batch updates may partially apply before an error.

`create_disk_image` takes an absolute `path`, `format` (`d64/d71/d81/dnp`),
optional `diskname`, and `tracks`: D64 defaults to 35 and accepts 35/40;
DNP requires 1–255; D71/D81 have fixed counts. Creation may overwrite existing files.
There is no general REST directory browser, file transfer, or deletion tool.

### REU uploads

```json
{
  "source":{"kind":"local","path":"C:/build/image.bin"},
  "address":"$560000",
  "guard":{"address":"$560000","length":16},
  "run_prg":{"kind":"local","path":"C:/build/start.prg"}
}
```

`upload_reu` writes a local or base64 file into REU (RAM Expansion Unit) memory.
REST `writemem` only reaches C64 RAM, so this tool uses the Ultimate's **DMA
service**: TCP port 64 on the configured host. Enable that service in the
device's network settings. `C64U_PASSWORD`, if set, authenticates the socket
too, and the HTTP timeout applies to each socket operation.

- `address` is a 24-bit hexadecimal REU address; `address + length` may not
  pass 16 MiB. Optional `offset` and `length` select a slice of the file
  (default: all of it). Files follow the usual 32 MiB upload limit.
- Data goes in writes of at most 32 KiB. The tool returns only after the
  device answers an IDENTIFY sent behind the last write, so all writes are done.
- `pause` (default `true`) pauses the CPU first, so a running program cannot
  race the upload with its own REU DMA, and resumes it afterwards, also when the
  write fails. If the pause fails, nothing is written. If a call is cancelled
  while paused, the server still tries to resume, with a three-second bound.
- `guard` is a range inside the written range. It is zeroed before any other
  write and receives its data last. A program that polls a header or magic
  there never sees a valid one, including a stale one from an earlier upload,
  before the whole image has arrived.
- `run_prg` (device, local or base64, validated like `run_prg`) resets and runs
  a PRG after the upload and resume; it is skipped if the upload fails.

The result reports `address`, `bytes_written`, `chunks` (data writes),
`guard` (with its `zero_chunks`) or null, `paused`, and `ran`.
A failure during the transfer can leave part of the range written.

### Keyboard, joystick and text

```json
{
  "events":[
    {"kind":"keyboard","inputs":["a"],"transition":"tap"},
    {"kind":"joystick","port":2,"inputs":["up","fire"],"transition":"press"},
    {"kind":"release_all"}
  ]
}
```

`send_input` accepts 1–64 events and at most 4096 serialized bytes.
Transitions are `press`, `release`, `tap`. Held inputs persist until released.
Keyboard chords allow 1–8 unique documented key names. `restore` must be alone
and tapped. Joystick ports are 1/2; inputs are up/down/left/right/fire/fire2/fire3.

`type_text` accepts `text`, optional `hold_ms` and `gap_ms` (both default 50,
range 1–1000), and at most 1000 characters. It validates all text first, then
presses/releases one chord at a time. Letters use unshifted physical C64 keys:
on the usual uppercase/graphics screen they appear uppercase; menu/lowercase
modes differ. CRLF becomes one Return.

Supported punctuation: `+ - . : @ , * ; = / ! " # $ % & ' ( ) ? < > [ ]`,
plus digits, spaces and newlines. Other characters are rejected.
Use raw events for exact Shift/Commodore combinations and special keys.
There is no automatic RAM keyboard-buffer fallback for older firmware.
Failures report completed characters; the current character may have applied.
MCP cancellation attempts to release the current chord while retaining the device
lock. Emergency release has a three-second bound; cancelling a queued operation
prevents it from executing.

### Screenshots

`capture_screenshot` binds a UDP receiver, starts video to that receiver,
assembles a complete frame, encodes PNG, then stops video. It captures the
documented cropped image: PAL 384×272 or NTSC 384×240, with a fixed reference palette.
It rejects incomplete frames and ignores packets from other IPs.

Video requires supported hardware and **Ethernet** on the Ultimate side.
Allow inbound UDP on the capture port in your firewall. On multi-interface
computers, set `C64U_CAPTURE_IP` to the reachable local IPv4 address.
Capture timeout covers setup/start/receive; stream cleanup has an additional
three-second bound. Errors distinguish no packets from incomplete frames.

Capture takes over the single video destination and can interrupt debug streaming.
It stops video afterward and does not restore another application's streams.
Cancellation attempts cleanup; communication failure or abrupt process termination
can leave a stream running. Use `stop_stream` if needed.

`read_text_screen` is a separate 40×25 memory interpretation, default address
`0400`; `include_colors:true` also reads color RAM. It assumes the default
uppercase/graphics character set and preserves raw codes. It does not reproduce
sprites, bitmap graphics, custom glyphs or raster effects.
`read_menu_screen` decodes firmware UI bytes and packed foreground/background
colors independently; unavailable glyphs remain available as raw codes.

## Firmware support and error behavior

| Capability | Documentation baseline / caveat |
| --- | --- |
| Core REST | Introduced in Ultimate firmware 3.11; validate actual C64U firmware separately |
| Network password | Documented from 3.12; sent as `X-Password` on every API request |
| Input and menu screens | Newer documentation; may be absent on installed firmware |
| Input, debug register, poweroff, streams | Hardware-specific; do not infer availability solely from model name |
| File metadata | Marked unfinished in the reference |
| Drive unlink | Uses source-confirmed `:unlink`; reference contains a duplicated `:remove` row |
| D64 tracks | Uses documented 35/40; newer source accepts additional values not exposed here |

Nonempty device `errors` arrays fail calls even with HTTP 200.
HTTP 403 reports authentication failure; 404 can mean a missing resource, closed
menu, or absent endpoint; 501 indicates unavailable functionality.
Transport errors never claim a mutation failed to execute: its outcome can be
uncertain. No automatic HTTP retries or redirects are enabled.
Discovery does not probe destructive operations.

## Development and hardware validation

```text
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Tests use loopback HTTP/UDP mocks and an actual stdio MCP subprocess.
They never read `C64U_HOST` to contact hardware. CI runs on Windows and Linux.
See the [hardware smoke test](docs/hardware-smoke-test.md) for initial checks and
the [opt-in hardware campaign](docs/hardware-campaign.md) for reproducible scratch
testing, cancellation recovery, and a five-minute soak.

Sources:
[Ultimate REST API](https://1541u-documentation.readthedocs.io/en/latest/api/api_calls.html),
[UDP stream format](https://1541u-documentation.readthedocs.io/en/latest/data_streams.html),
[firmware source](https://github.com/GideonZ/1541ultimate/tree/master/software/api),
[reference capture palette](https://github.com/GideonZ/1541ultimate/blob/master/python/grab.py),
[official Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk).
The sibling Impact project's Python helpers informed strict memory-length and
loss-detecting frame validation; this project has no runtime dependency on Impact.


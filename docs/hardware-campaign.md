# Opt-in hardware campaign

The campaign exercises a real C64U through the compiled stdio MCP server. It is
never run by `cargo test` or CI. Use a scratch session: **the running program is
replaced**, RAM and volatile configuration change, and cleanup reboots to BASIC.
The original running program is not backed up or recovered.

Enable the device's Web Remote Control service and connect its Ethernet port for
video. Allow inbound UDP on the capture port. The child server inherits
`C64U_PASSWORD` and capture environment settings described in the [README](../README.md).
Run one campaign at a time; its stream operations take over the device's video
destination and can interrupt other streams.

Build the server first, then substitute your device's IP:

```text
cargo build --release --locked
cargo run --locked --example hardware_campaign -- --host IP --scratch --output target/campaign-run-001
```

The required `--scratch` flag acknowledges the state changes. The default server
executable is `target/release/c64umcp` (`.exe` on Windows); use `--executable` to
test another build. Always choose a **new `--output` directory for each run**:
the harness refuses a directory already containing a report. The default
directory is `target/hardware-campaign`.

## Stages

Select a stage with `--stage`; `all` is the default. Every stage performs baseline
checks and attempts cleanup when it finishes, fails, or receives Ctrl+C.

| Stage | Checks |
| --- | --- |
| `all` | Programs, drives, then soak |
| `programs` | PRG load/run, heartbeat pause/resume, memory round trips and invalid arguments, resets, menus/input where available, then cartridge and media checks |
| `cartridge` | Temporary CRT launch, visible marker, reboot and BASIC readiness |
| `media` | Silent SID/MOD playback, stream controls, capture timeout and cancellation recovery |
| `drives` | Initially empty drive B: drive modes, temporary bus configuration, uploaded blank disk, remount/unlink/eject and restoration |
| `soak` | Heartbeat program, concurrent information/memory/drive requests, repeated screenshots and timing summary |

The soak defaults to `--soak-cycles 60`, with cycle starts spaced at least five
seconds apart; slow cycles can take longer. The maximum is 180 cycles. For a
short separate run:

```text
cargo run --locked --example hardware_campaign -- --host IP --scratch --stage soak --soak-cycles 6 --output target/campaign-run-002
```

Fixtures are generated from original source in
[`examples/support/fixtures.rs`](../examples/support/fixtures.rs): a heartbeat
PRG, marker cartridge, silent SID/MOD, and blank D64. No existing game, cartridge
ROM, or music file is required. The drive stage refuses to replace pre-existing
media in drive B. Flash persistence, poweroff, drive-ROM replacement, debug-register
writes, and creation of device-side disk files are excluded from the campaign.

## Cleanup and artifacts

Before mutations, the campaign validates and saves the current stream settings
and, for drive testing, drive B settings. These are **current configuration
snapshots, not flash backups**. Cleanup attempts to close its menu, stop streams,
restore scratch RAM and drive settings, release injected inputs, resume/reboot,
wait for BASIC, capture a final image, and restore stream settings. Configuration
and drive-state readbacks are checked; cleanup errors remain failures in the report.
Previously active streams are not restarted.

The output directory contains:

- `report.json`: checks, tool coverage, errors, timings, and cleanup outcome.
- `snapshots.json`: captured configuration values for inspection.
- Numbered PNG screenshots; image bytes in the report are replaced by artifact references.
- `heartbeat.prg` when the program-loading stage runs.

There is no automatic recovery from an older snapshot. A killed process, lost
connection, or powered-off host can prevent cleanup; retain its report and snapshots
for deliberate inspection and restoration. Starting a new run captures the state
that exists at that time.

## Cancellation and UDP regressions

The media stage deliberately misconfigures a separate screenshot client, checks
the resulting failure, cancels another capture through MCP, and checks that later
requests and normal capture work. Offline subprocess tests additionally cover
queued-mutation cancellation and typing/capture cleanup ordering. Emergency key
release and stream-stop attempts are bounded, and retain the device-operation lock
while cleanup runs.

The UDP receiver uses a full datagram buffer so an oversized packet does not cause
Windows `WSAEMSGSIZE` before validation. It still accepts only the documented
780-byte video packets and requires a complete frame. Regression tests inject an
oversized datagram followed by valid video and check that capture continues.
These tests and the campaign describe coverage; device compatibility is established
by the saved results of each hardware run.

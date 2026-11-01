# C64 Ultimate hardware results

The scratch-session campaign passed **32 of 44 tools** through the release server's
actual stdio MCP transport. Five tools depend on unavailable firmware endpoints;
seven operations were deliberately excluded. No unexpected failures remain in the
final functional report. A separate 60-cycle soak also passed.

Device tested: C64 Ultimate at the user-provided LAN address, firmware
**1.1.0**, FPGA **122**, core **1.49**, REST API **0.1**. These results describe this
device and firmware; they do not establish compatibility with every Ultimate model.

The authorized scratch session replaced the running program with original test
fixtures. Cleanup finished successfully: drive B's configuration and runtime state
matched the original snapshot, the RAM test buffer was restored before reboot, test streams
were stopped, and stream settings matched their original values. The final PNG was
visually checked and shows BASIC at `READY.`. Nothing was saved to configuration
flash. Previously running programs and active streams were not resumed.

## Functional evidence

| Exercise | Verification |
| --- | --- |
| PRG load and run | Local upload matched memory byte for byte without starting; base64 run displayed `C64U MCP TEST` and advanced a 32-bit heartbeat |
| Pause and resume | Heartbeat stopped while paused and advanced after resume |
| RAM boundaries | 1-, 128-, 129-, and 256-byte writes read back exactly; final-address read passed; invalid/wrapping requests were rejected |
| Reset and reboot | Polled for BASIC `READY.` and VIC display enable before proceeding |
| Cartridge | Original 8 KiB CRT executed and displayed its marker |
| SID | Original silent SID initialization signature and advancing play counter verified both callbacks |
| MOD | Original silent MOD accepted, with a subsequent screenshot; audio fidelity was not tested |
| Drive B | Enabled, switched between 1581/1541, changed bus ID temporarily, mounted a blank D64, queried metadata, remounted by device path, unlinked, reset, ejected, and disabled |
| Configuration | Single and batch updates read back correctly; original drive and stream values restored and compared exactly |
| Streams | Video, audio, and debug start/stop commands succeeded; this does not validate audio/debug payload decoding |
| Screenshots | Complete NTSC 384x240 PNGs decoded successfully; program, menu, and final BASIC images inspected |
| Recovery | Deliberate capture timeout and actual MCP cancellation released the operation lock; subsequent requests and normal capture succeeded |
| Menu | Open/close succeeded and screenshot showed the menu; structured menu read still returned HTTP 404 while open |

## Complete tool coverage

“Passed” means the final campaign exercised the tool successfully with the inputs
described above; it does not imply every argument combination was tested. Input
event tools were classified unavailable after the input-state endpoint returned
404, rather than attempting injection on that firmware. No RAM keyboard fallback
was used. Excluded operations remain unverified on hardware.

| Tool | Hardware status |
| --- | --- |
| `capture_screenshot` | Passed |
| `create_disk_image` | Excluded: persistent device file creation |
| `disable_drive` | Passed |
| `eject_disk` | Passed |
| `enable_drive` | Passed |
| `file_info` | Passed |
| `get_config` | Passed |
| `get_info` | Passed |
| `get_input_state` | Unavailable: HTTP 404 |
| `get_version` | Passed |
| `list_config_categories` | Passed |
| `list_drives` | Passed |
| `load_config_from_flash` | Excluded: bulk configuration replacement |
| `load_drive_rom` | Excluded: drive ROM replacement |
| `load_prg` | Passed |
| `mount_disk` | Passed |
| `pause_machine` | Passed |
| `play_mod` | Passed; acceptance only, silent fixture |
| `play_sid` | Passed; init/play callbacks, silent fixture |
| `poweroff_machine` | Excluded: poweroff |
| `read_debug_register` | Passed |
| `read_memory` | Passed |
| `read_menu_screen` | Unavailable: HTTP 404 while menu open |
| `read_text_screen` | Passed |
| `reboot_machine` | Passed |
| `release_all_inputs` | Unavailable: input capability preflight |
| `reset_config_defaults` | Excluded: bulk configuration replacement |
| `reset_drive` | Passed |
| `reset_machine` | Passed |
| `resume_machine` | Passed |
| `run_crt` | Passed |
| `run_prg` | Passed |
| `save_config_to_flash` | Excluded: flash persistence |
| `send_input` | Unavailable: input capability preflight |
| `set_config` | Passed |
| `set_config_batch` | Passed |
| `set_drive_mode` | Passed |
| `start_stream` | Passed |
| `stop_stream` | Passed |
| `toggle_menu` | Passed |
| `type_text` | Unavailable: input capability preflight |
| `unlink_disk` | Passed |
| `write_debug_register` | Excluded: debug-register writes |
| `write_memory` | Passed |

## Soak results

The soak ran **60 cycles in 296.02 seconds**, with starts spaced five seconds apart.
Each cycle submitted three concurrent reads (`get_version`, `read_memory`, and
`list_drives`) and then captured and decoded a screenshot: **240 successful MCP
calls**, with no failed cycles or incomplete PNGs. Setup and cleanup calls are
additional to that count.

Median whole-cycle latency was **185.81 ms**; p95 was **213.25 ms**. These measure
the reads plus screenshot, excluding the delay between cycles; they are not
individual tool latency measurements. Cleanup after the soak also passed.

## Fixes and regression coverage

The campaign and accompanying offline fault tests exposed and fixed:

- MCP cancellation did not drop the running handler automatically. Dispatch now
  observes the request cancellation token, drops the operation, and runs cleanup;
  a cancelled operation waiting for the device lock cannot execute later.
- Emergency keyboard release could hold the operation lock for the full HTTP
  timeout. It now has a three-second deadline that survives cancellation during
  cleanup. This behavior is covered offline because the device lacks input APIs.
- Oversized UDP packets could cause Windows `WSAEMSGSIZE` before packet validation.
  A full datagram buffer now permits validation and rejection without aborting a
  valid capture. The decoder still requires the documented 780-byte packets.

The test fixtures also needed two hardware-driven corrections: wait for actual
boot/display readiness after a reset acknowledgement, and write the 6510 port data
before enabling its output direction so a cartridge does not bank out its own ROM.
The cartridge regression simulates ROM visibility at every instruction fetch.
Earlier exploratory failures are retained locally; the final run passed after
these corrections.

A subsequent review found that normal MCP stdin closure could terminate the
runtime before cancellation cleanup finished. Shutdown now waits for active
handlers to unwind and for keyboard/video cleanup to release the device lock.
Three subprocess regressions verify delayed stream-stop and key-release responses
are awaited, and an unresponsive cleanup remains bounded. This fix was validated
offline; the hardware results above describe the earlier campaign.

All **67 offline tests** pass: 28 library tests, 14 HTTP tests, four actual stdio
cancellation tests, three stdio shutdown tests, four stdio protocol tests, eight
tool-contract tests, and six fixture tests. Formatting and Clippy with warnings
denied also pass. Offline tests use loopback mocks and never contact the hardware.

## Reproduce and inspect

See [hardware campaign instructions](hardware-campaign.md) for prerequisites,
stages, original fixtures, cleanup behavior, and commands. The final functional
run used `--stage all --soak-cycles 0`; the separate soak used
`--stage soak --soak-cycles 60`. Build the release executable before each campaign.

Local evidence is ignored by Git:

- [`target/hardware-campaign-final/report.json`](../target/hardware-campaign-final/report.json): authoritative functional checks and the full tool matrix.
- [`target/hardware-campaign-final/snapshots.json`](../target/hardware-campaign-final/snapshots.json): original configuration values.
- [`target/hardware-campaign-final/006-final-BASIC-screenshot.png`](../target/hardware-campaign-final/006-final-BASIC-screenshot.png): final machine state.
- [`target/hardware-campaign-soak/report.json`](../target/hardware-campaign-soak/report.json): timings and all soak checks, with 60 cycle PNGs alongside it.

These artifacts contain device identity and configuration data and remain local.
The earlier read-only smoke report is retained in `target/hardware-smoke/`; the
scratch campaign supersedes its limited coverage.

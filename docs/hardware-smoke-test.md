# C64U hardware smoke test

No hardware test is part of the automated suite. Run these steps through an MCP
client after configuring the real device address. Record product, firmware/API
versions, network connection, and the results; do not assume newer documented
endpoints exist on shipped C64U firmware.

## 1. Read-only connection checks

1. Enable Web Remote Control Service; configure C64U_HOST and, if needed,
   C64U_PASSWORD in the server environment.
2. Call get_version and get_info. Record the full responses.
3. Call list_drives and list_config_categories.
4. Call get_config with a category returned by discovery.
5. Call read_memory with {"address":"0400","length":16}; confirm 16 returned bytes.
6. Call read_text_screen. Compare text with the visible machine where applicable.
7. Call get_input_state; record unavailable-feature errors rather than using a
   memory-write fallback.

An authentication failure should be fixed before continuing. Do not change network
configuration as part of a connectivity test.

## 2. Scratch program and input

Use a disposable session with no unsaved program or disk work.

1. Call load_prg with a known harmless local PRG. Confirm reset and loaded state.
2. Call run_prg with the same program. Confirm expected output.
3. At a BASIC prompt, call type_text with {"text":"PRINT 2+2\n"}.
   Confirm one command and expected output; tune hold_ms/gap_ms if necessary.
4. Exercise a raw key tap and release_all_inputs.
5. Pause and resume the scratch program; always resume after the check.
6. If desired, mount a disposable image in readonly mode, verify list_drives,
   then eject it. Test writes only with a scratch image explicitly intended for
   modification.

## 3. Screens and cancellation

1. Use Ethernet for the Ultimate video stream. Set a reachable local capture IP
   and allow inbound UDP on the chosen port.
2. Call capture_screenshot; compare the PNG with the display, allowing for the
   documented crop and fixed palette.
3. Repeat on PAL/NTSC if both are available.
4. Toggle the menu, read_menu_screen, and close it afterward. A closed menu may
   return 404; distinguish that from an absent firmware endpoint.
5. Cancel a long type_text operation and check that no injected key remains held.
6. Cancel capture or deliberately use an unavailable receive port; check cleanup
   diagnostics and stop_stream afterward if necessary.

Screenshots replace the video destination and stop video afterward. Do not use
this sequence while another application needs an uninterrupted video/debug stream.

## Excluded from the default smoke test

Do not power off, save/reset persistent configuration, overwrite existing disk
images, change network settings, or upload a drive ROM just to check connectivity.
Those features have offline contract tests and can be verified later in a
deliberately prepared session.

Record results as passed, failed, or unsupported, with actual firmware versions.
An offline test pass is not a hardware compatibility claim.


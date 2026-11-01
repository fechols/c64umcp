//! Original, deterministic hardware-test payloads. No third-party program,
//! cartridge ROM, music, or sample data is embedded here.
//!
//! Format references (only the formats informed these original generators):
//! - CRT: https://vice-emu.sourceforge.io/vice_17.html#SEC415
//! - SID: https://www.hvsc.c64.org/download/C64Music/DOCUMENTS/SID_file_format.txt
//! - MOD: https://github.com/8bitbubsy/pt2-clone/blob/master/src/pt2_module_saver.c
//! - D64: https://github.com/VICE-Team/svn-mirror/blob/main/vice/src/vdrive/vdrive-bam.c
//! - KERNAL: https://www.pagetable.com/c64ref/kernal/
//!
//! Observe the PRG marker at $0400 and its little-endian u32 counter at
//! $C000..$C003. The counter runs until pause/reset; it is stable while paused.
//! The CRT shows C64U MCP CRT at $0400 and idles. Its startup phase byte at
//! $C010 is $A1 on entry, $A2 after CPU mapping, $A3 after CIA initialization,
//! $A4 after VIC/SID initialization, and $A5 after drawing. SID initialization writes $53
//! at $1020; its periodic play routine increments the byte at $1021 silently.
//! The MOD has one empty pattern; the D64 has no files and 664 data blocks free.

/// A BASIC `10 SYS2061` bootstrap plus original machine code at $080D.
/// Takes over the text display, disables interrupts, and loops indefinitely.
pub fn heartbeat_prg() -> Vec<u8> {
    let mut prg = vec![
        0x01, 0x08, // PRG load address: $0801
        0x0b, 0x08, // link to BASIC terminator at $080B
        0x0a, 0x00, // BASIC line 10
        0x9e, b'2', b'0', b'6', b'1', 0, // SYS2061
        0, 0, // end of BASIC program
    ];
    prg.extend(marker_program(0x080d, b"C64U MCP TEST", true));
    prg
}

/// A generic 8 KiB ROML cartridge with cold/warm CBM80 autostart at $8009.
/// Uses only volatile machine state; run_crt does not save this as default.
pub fn cartridge() -> Vec<u8> {
    let mut rom = vec![0xea; 8192]; // unused space contains NOPs
    rom[..9].copy_from_slice(&[0x09, 0x80, 0x09, 0x80, 0xc3, 0xc2, 0xcd, 0x38, 0x30]);
    let mut startup = vec![
        0x78, 0xd8, 0xa2, 0xff, 0x9a, // SEI; CLD; LDX #$FF; TXS
        0xa9, 0xa1, 0x8d, 0x10, 0xc0, // startup phase: entered cartridge
        // Set the output latch before enabling outputs: after reset the DDR
        // inputs hold the mapping lines high. Enabling outputs with an unset
        // latch can bank out this executing ROM before the next instruction.
        // The original KERNAL IOINIT uses this same data-before-DDR order.
        0xa9, 0x37, 0x85, 0x01, // normal ROM/I/O mapping latch
        0xa9, 0x2f, 0x85, 0x00, // enable CPU port outputs
        0xa9, 0xa2, 0x8d, 0x10, 0xc0, // startup phase: mapping initialized
        // This standalone cartridge needs no KERNAL calls, RAM test, vectors,
        // or screen editor. Initialize only the hardware used by its renderer.
        0xa9, 0x00, 0x8d, 0x16, 0xd0, // establish VIC refresh/test state
        0xa9, 0x7f, 0x8d, 0x0d, 0xdc, 0x8d, 0x0d, 0xdd, // mask CIA IRQ/NMI
        0xad, 0x0d, 0xdc, 0xad, 0x0d, 0xdd, // acknowledge pending CIA flags
        0xa9, 0x00, // stop all four CIA timers
        0x8d, 0x0e, 0xdc, 0x8d, 0x0f, 0xdc, 0x8d, 0x0e, 0xdd, 0x8d, 0x0f, 0xdd, 0xa9, 0xa3, 0x8d,
        0x10, 0xc0, // startup phase: CIAs initialized
        0xa9, 0x00, 0x8d, 0x1a, 0xd0, // disable VIC interrupts
        0x8d, 0x18, 0xd4, // mute SID
        0xa9, 0x0f, 0x8d, 0x19, 0xd0, // acknowledge pending VIC interrupts
        0xa9, 0xa4, 0x8d, 0x10, 0xc0, // startup phase: VIC/SID initialized
    ];
    startup.extend(marker_program(
        0x8009 + startup.len() as u16,
        b"C64U MCP CRT",
        false,
    ));
    rom[9..9 + startup.len()].copy_from_slice(&startup);
    let mut crt = vec![0; 64];
    crt[..16].copy_from_slice(b"C64 CARTRIDGE   ");
    be32(&mut crt, 0x10, 64);
    be16(&mut crt, 0x14, 0x0100);
    be16(&mut crt, 0x16, 0); // generic cartridge
    crt[0x18] = 0; // EXROM asserted
    crt[0x19] = 1; // GAME released: 8 KiB mode
    text(&mut crt[0x20..0x40], b"C64U MCP ORIGINAL TEST");
    let mut chip = vec![0; 16];
    chip[..4].copy_from_slice(b"CHIP");
    be32(&mut chip, 4, 16 + 8192);
    be16(&mut chip, 8, 0); // ROM
    be16(&mut chip, 10, 0); // bank 0
    be16(&mut chip, 12, 0x8000);
    be16(&mut chip, 14, 8192);
    crt.extend(chip);
    crt.extend(rom);
    crt
}

/// A PSID v2 tune with one silent song, valid on PAL or NTSC.
/// $1000 init clears SID registers and marks $1020=$53; $1015 play increments
/// $1021 and returns. Both observation bytes are inside the loaded payload.
pub fn sid() -> Vec<u8> {
    let mut sid = vec![0; 124];
    sid[..4].copy_from_slice(b"PSID");
    be16(&mut sid, 4, 2);
    be16(&mut sid, 6, 124);
    be16(&mut sid, 8, 0); // data starts with little-endian load address
    be16(&mut sid, 10, 0x1000);
    be16(&mut sid, 12, 0x1015);
    be16(&mut sid, 14, 1); // one song
    be16(&mut sid, 16, 1); // start song
    // speed=0: vertical-blank-rate playback; flags clock=both, model=any.
    be16(&mut sid, 118, 0x000c);
    text(&mut sid[22..54], b"C64U MCP SILENT TEST");
    text(&mut sid[54..86], b"c64umcp original fixture");
    text(&mut sid[86..118], b"2026");
    sid.extend([0x00, 0x10]);
    let code = [
        0xa9, 0x00, // LDA #0
        0xa2, 0x18, // LDX #24
        0x9d, 0x00, 0xd4, // clear: STA $D400,X
        0xca, // DEX
        0x10, 0xfa, // BPL clear
        0xa9, 0x53, 0x8d, 0x20, 0x10, // LDA #$53; STA $1020
        0xa9, 0x00, 0x8d, 0x21, 0x10, // LDA #0; STA $1021
        0x60, // RTS (init)
        0xee, 0x21, 0x10, 0x60, // play: INC $1021; RTS
    ];
    sid.extend(code);
    sid.resize(124 + 2 + 0x22, 0); // include signature/counter storage
    sid
}

/// A silent, four-channel ProTracker module: 31 empty samples, one empty pattern.
pub fn module() -> Vec<u8> {
    let mut module = vec![0; 1084 + 1024];
    text(&mut module[..20], b"C64U MCP SILENT");
    for sample in 0..31 {
        // ProTracker uses a one-word loop length even for an empty sample.
        be16(&mut module, 20 + sample * 30 + 28, 1);
    }
    module[950] = 1; // one pattern in the order list, pattern 0
    module[951] = 0x7f; // conventional ProTracker restart byte
    module[1080..1084].copy_from_slice(b"M.K.");
    module
}

/// An empty, formatted 35-track D64 with disk name C64U MCP TEST and ID CM.
pub fn disk_d64() -> Vec<u8> {
    let mut disk = vec![0; 174848];
    let bam_offset = sector_offset(18, 0);
    let bam = &mut disk[bam_offset..bam_offset + 256];
    bam[..4].copy_from_slice(&[18, 1, b'A', 0]);
    for track in 1..=35 {
        let count = sectors(track);
        let entry = 4 + (track - 1) * 4;
        let mut bitmap = (1u32 << count) - 1;
        if track == 18 {
            bitmap &= !3; // BAM and first directory sector are allocated
        }
        bam[entry] = bitmap.count_ones() as u8;
        bam[entry + 1] = bitmap as u8;
        bam[entry + 2] = (bitmap >> 8) as u8;
        bam[entry + 3] = (bitmap >> 16) as u8;
    }
    bam[0x90..0xab].fill(0xa0);
    text(&mut bam[0x90..0xa0], b"C64U MCP TEST");
    bam[0xa2..0xa4].copy_from_slice(b"CM");
    bam[0xa5..0xa7].copy_from_slice(b"2A");
    let directory = sector_offset(18, 1);
    disk[directory] = 0; // final directory sector
    disk[directory + 1] = 0xff;
    disk
}

fn marker_program(base: u16, marker: &[u8], heartbeat: bool) -> Vec<u8> {
    let mut code = Code {
        base,
        bytes: vec![0x78, 0xd8],
    }; // SEI; CLD
    // Select VIC bank 0 without disturbing other CIA2 port bits.
    code.emit(&[0xad, 0x02, 0xdd, 0x09, 0x03, 0x8d, 0x02, 0xdd]);
    code.emit(&[0xad, 0x00, 0xdd, 0x09, 0x03, 0x8d, 0x00, 0xdd]);
    for (address, value) in [
        (0xd011, 0x1b),
        (0xd016, 0x08),
        (0xd018, 0x14),
        (0xd015, 0),
        (0xd020, 6),
        (0xd021, 0),
    ] {
        code.store(address, value);
    }
    // Clear all four screen/color pages, including the unused final 24 bytes.
    code.emit(&[0xa2, 0x00, 0xa9, 0x20]);
    let clear = code.pc();
    for address in [0x0400, 0x0500, 0x0600, 0x0700] {
        code.absolute(0x9d, address);
    }
    code.emit(&[0xe8]); // INX
    code.branch(0xd0, clear);
    code.emit(&[0xa9, 1]);
    let color = code.pc();
    for address in [0xd800, 0xd900, 0xda00, 0xdb00] {
        code.absolute(0x9d, address);
    }
    code.emit(&[0xe8]);
    code.branch(0xd0, color);
    for (index, &ch) in marker.iter().enumerate() {
        let screen_code = if ch.is_ascii_uppercase() {
            ch - b'A' + 1
        } else {
            ch
        };
        code.store(0x0400 + index as u16, screen_code);
    }
    if heartbeat {
        for address in 0xc000..=0xc003 {
            code.store(address, 0);
        }
        let counter = code.pc();
        for address in 0xc000..=0xc003 {
            code.absolute(0xee, address); // INC byte
            if address < 0xc003 {
                code.branch(0xd0, counter);
            }
        }
        code.absolute(0x4c, counter);
    } else {
        code.store(0xc010, 0xa5); // cartridge marker is ready
        code.absolute(0x4c, code.pc()); // idle: JMP idle
    }
    code.bytes
}

struct Code {
    base: u16,
    bytes: Vec<u8>,
}
impl Code {
    fn pc(&self) -> u16 {
        self.base + self.bytes.len() as u16
    }
    fn emit(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }
    fn absolute(&mut self, opcode: u8, address: u16) {
        self.emit(&[opcode, address as u8, (address >> 8) as u8]);
    }
    fn store(&mut self, address: u16, value: u8) {
        self.emit(&[0xa9, value]);
        self.absolute(0x8d, address);
    }
    fn branch(&mut self, opcode: u8, target: u16) {
        let offset = i32::from(target) - i32::from(self.pc()) - 2;
        assert!((-128..=127).contains(&offset));
        self.emit(&[opcode, offset as i8 as u8]);
    }
}
fn text(destination: &mut [u8], source: &[u8]) {
    assert!(source.len() <= destination.len());
    destination[..source.len()].copy_from_slice(source);
}
fn be16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}
fn be32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}
fn sectors(track: usize) -> usize {
    match track {
        1..=17 => 21,
        18..=24 => 19,
        25..=30 => 18,
        31..=35 => 17,
        _ => panic!("invalid D64 track"),
    }
}
fn sector_offset(track: usize, sector: usize) -> usize {
    assert!(sector < sectors(track));
    ((1..track).map(sectors).sum::<usize>() + sector) * 256
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prg_has_a_valid_basic_entry_and_original_counter_loop() {
        let prg = heartbeat_prg();
        assert_eq!(
            &prg[..14],
            &[1, 8, 11, 8, 10, 0, 0x9e, b'2', b'0', b'6', b'1', 0, 0, 0]
        );
        assert_eq!(&prg[14..16], &[0x78, 0xd8]);
        let loop_start = prg.len() - 21;
        let address = 0x0801 + loop_start as u16 - 2;
        assert_eq!(
            &prg[loop_start..loop_start + 5],
            &[0xee, 0, 0xc0, 0xd0, 0xfb]
        );
        assert_eq!(
            &prg[loop_start + 5..loop_start + 10],
            &[0xee, 1, 0xc0, 0xd0, 0xf6]
        );
        assert_eq!(
            &prg[loop_start + 10..loop_start + 15],
            &[0xee, 2, 0xc0, 0xd0, 0xf1]
        );
        assert_eq!(
            &prg[loop_start + 15..],
            &[0xee, 3, 0xc0, 0x4c, address as u8, (address >> 8) as u8]
        );
    }

    #[test]
    fn crt_headers_and_autostart_vectors_describe_one_8k_rom() {
        let crt = cartridge();
        assert_eq!(crt.len(), 64 + 16 + 8192);
        assert_eq!(&crt[..16], b"C64 CARTRIDGE   ");
        assert_eq!(&crt[0x10..0x1a], &[0, 0, 0, 64, 1, 0, 0, 0, 0, 1]);
        assert_eq!(&crt[64..68], b"CHIP");
        assert_eq!(
            &crt[68..80],
            &[0, 0, 0x20, 0x10, 0, 0, 0, 0, 0x80, 0, 0x20, 0]
        );
        assert_eq!(
            &crt[80..89],
            &[9, 0x80, 9, 0x80, 0xc3, 0xc2, 0xcd, 0x38, 0x30]
        );
        assert_eq!(&crt[89..94], &[0x78, 0xd8, 0xa2, 0xff, 0x9a]);
    }

    #[test]
    fn crt_draws_without_any_kernal_rom_dependency() {
        let crt = cartridge();
        // Execute the emitted instruction subset with deliberately dirty RAM
        // and no KERNAL ROM. This catches bad branch addresses or ROM calls;
        // hardware behavior itself is checked by the explicit-target campaign.
        let mut memory = vec![0xaa; 65536];
        memory[0x8000..0xa000].copy_from_slice(&crt[80..]);
        memory[0] = 0; // CPU port inputs after reset; mapping pull-ups are high
        memory[1] = 0; // output latch must be initialized before enabling outputs
        let (mut pc, mut a, mut x, mut zero) = (0x8009usize, 0u8, 0u8, false);
        let mut idle = false;
        for _ in 0..10000 {
            let mapping = (memory[1] & memory[0]) | !memory[0];
            assert_eq!(
                mapping & 3,
                3,
                "8 KiB cartridge must stay mapped at instruction fetch ${pc:04X}"
            );
            let instruction = pc;
            let opcode = memory[pc];
            pc += 1;
            match opcode {
                0x78 | 0xd8 | 0x9a => {} // SEI, CLD, TXS
                0xa9 | 0xa2 | 0x09 => {
                    let value = memory[pc];
                    pc += 1;
                    match opcode {
                        0xa9 => a = value,
                        0xa2 => x = value,
                        _ => a |= value,
                    }
                    zero = if opcode == 0xa2 { x == 0 } else { a == 0 };
                }
                0x85 => {
                    let address = memory[pc] as usize;
                    memory[address] = a;
                    pc += 1;
                }
                0xad | 0x8d | 0x9d | 0x4c => {
                    let address = usize::from(u16::from_le_bytes([memory[pc], memory[pc + 1]]));
                    pc += 2;
                    match opcode {
                        0xad => {
                            a = memory[address];
                            zero = a == 0;
                        }
                        0x8d => memory[address] = a,
                        0x9d => memory[address + x as usize] = a,
                        _ => {
                            if address == instruction {
                                idle = true;
                                break;
                            }
                            pc = address;
                        }
                    }
                }
                0xe8 => {
                    x = x.wrapping_add(1);
                    zero = x == 0;
                }
                0xd0 => {
                    let offset = memory[pc] as i8;
                    pc += 1;
                    if !zero {
                        pc = pc.wrapping_add_signed(offset as isize);
                    }
                }
                _ => panic!("unexpected opcode ${opcode:02X} at ${instruction:04X}"),
            }
        }
        assert!(idle, "cartridge must finish initialization and idle");
        assert_eq!(
            &memory[0x400..0x40c],
            &[3, 54, 52, 21, 32, 13, 3, 16, 32, 3, 18, 20]
        );
        assert!(memory[0x40c..0x800].iter().all(|&byte| byte == 32));
        assert!(memory[0xd800..0xdc00].iter().all(|&byte| byte == 1));
        for (address, value) in [
            (0x00, 0x2f),
            (0x01, 0x37),
            (0xc010, 0xa5),
            (0xd011, 0x1b),
            (0xd016, 8),
            (0xd018, 0x14),
            (0xd015, 0),
            (0xd01a, 0),
            (0xd418, 0),
            (0xdc0e, 0),
            (0xdc0f, 0),
            (0xdd0e, 0),
            (0xdd0f, 0),
        ] {
            assert_eq!(memory[address], value, "register ${address:04X}");
        }
        assert_eq!(memory[0xdd00] & 3, 3);
        assert_eq!(memory[0xdd02] & 3, 3);
    }

    #[test]
    fn sid_entrypoints_and_observation_storage_are_inside_payload() {
        let sid = sid();
        assert_eq!(
            &sid[..18],
            &[
                b'P', b'S', b'I', b'D', 0, 2, 0, 124, 0, 0, 0x10, 0, 0x10, 0x15, 0, 1, 0, 1
            ]
        );
        assert_eq!(&sid[124..126], &[0, 0x10]);
        assert_eq!(&sid[126 + 0x15..126 + 0x19], &[0xee, 0x21, 0x10, 0x60]);
        assert_eq!(sid.len(), 126 + 0x22);
        assert_eq!(&sid[126 + 0x20..], &[0, 0]);
    }

    #[test]
    fn mod_has_one_empty_four_channel_pattern_and_no_sample_payload() {
        let module = module();
        assert_eq!(module.len(), 2108);
        assert_eq!(&module[950..952], &[1, 0x7f]);
        assert_eq!(&module[1080..1084], b"M.K.");
        assert!(module[1084..].iter().all(|&byte| byte == 0));
        for sample in 0..31 {
            let header = &module[20 + sample * 30..20 + (sample + 1) * 30];
            assert_eq!(&header[22..30], &[0, 0, 0, 0, 0, 0, 0, 1]);
        }
    }

    #[test]
    fn d64_bam_matches_sector_geometry_and_empty_directory() {
        let disk = disk_d64();
        assert_eq!(disk.len(), 683 * 256);
        let bam = &disk[sector_offset(18, 0)..sector_offset(18, 1)];
        assert_eq!(&bam[..4], &[18, 1, 0x41, 0]);
        let mut data_free = 0;
        for track in 1..=35 {
            let entry = 4 + (track - 1) * 4;
            let bitmap = u32::from_le_bytes([bam[entry + 1], bam[entry + 2], bam[entry + 3], 0]);
            assert_eq!(bitmap.count_ones(), u32::from(bam[entry]));
            assert_eq!(bitmap >> sectors(track), 0);
            if track == 18 {
                assert_eq!(bitmap & 3, 0);
                assert_eq!(bam[entry], 17);
            } else {
                data_free += bam[entry] as usize;
            }
        }
        assert_eq!(data_free, 664);
        assert_eq!(&bam[0x90..0x9d], b"C64U MCP TEST");
        let directory = &disk[sector_offset(18, 1)..sector_offset(18, 2)];
        assert_eq!(&directory[..2], &[0, 0xff]);
        assert!(directory[2..].iter().all(|&byte| byte == 0));
    }
}

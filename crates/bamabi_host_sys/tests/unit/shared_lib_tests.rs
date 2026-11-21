use super::*;

fn build_synthetic_elf64(has_pt_tls: bool, dt_flags: u64, dt_flags_1: u64) -> Vec<u8> {
    let phoff = 64usize;
    let phnum = if has_pt_tls { 2usize } else { 1usize };
    let dyn_off = phoff + phnum * 56;
    let dyn_entries = 3usize;
    let mut buf = vec![0u8; dyn_off + dyn_entries * 16];

    buf[0..4].copy_from_slice(b"\x7fELF");
    buf[4] = 2; // ELFCLASS64
    buf[5] = 1; // ELFDATA2LSB
    buf[6] = 1; // EV_CURRENT
    buf[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
    buf[54..56].copy_from_slice(&56u16.to_le_bytes());
    buf[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());

    // Phdr 0: PT_DYNAMIC
    let p0 = phoff;
    buf[p0..p0 + 4].copy_from_slice(&2u32.to_le_bytes());
    buf[p0 + 8..p0 + 16].copy_from_slice(&(dyn_off as u64).to_le_bytes());
    buf[p0 + 32..p0 + 40].copy_from_slice(&((dyn_entries * 16) as u64).to_le_bytes());

    if has_pt_tls {
        let p1 = phoff + 56;
        buf[p1..p1 + 4].copy_from_slice(&7u32.to_le_bytes());
    }

    // Dynamic entries: DT_FLAGS, DT_FLAGS_1, DT_NULL
    buf[dyn_off..dyn_off + 8].copy_from_slice(&30u64.to_le_bytes());
    buf[dyn_off + 8..dyn_off + 16].copy_from_slice(&dt_flags.to_le_bytes());
    buf[dyn_off + 16..dyn_off + 24].copy_from_slice(&0x6fff_fffbu64.to_le_bytes());
    buf[dyn_off + 24..dyn_off + 32].copy_from_slice(&dt_flags_1.to_le_bytes());
    buf
}

fn build_synthetic_elf32(has_pt_tls: bool, dt_flags: u32, dt_flags_1: u32) -> Vec<u8> {
    let phoff = 52usize;
    let phnum = if has_pt_tls { 2usize } else { 1usize };
    let dyn_off = phoff + phnum * 32;
    let mut buf = vec![0u8; dyn_off + 3 * 8];

    buf[0..4].copy_from_slice(b"\x7fELF");
    buf[4] = 1; // ELFCLASS32
    buf[5] = 1; // ELFDATA2LSB
    buf[28..32].copy_from_slice(&(phoff as u32).to_le_bytes());
    buf[42..44].copy_from_slice(&32u16.to_le_bytes());
    buf[44..46].copy_from_slice(&(phnum as u16).to_le_bytes());

    let p0 = phoff;
    buf[p0..p0 + 4].copy_from_slice(&2u32.to_le_bytes());
    buf[p0 + 4..p0 + 8].copy_from_slice(&(dyn_off as u32).to_le_bytes());
    buf[p0 + 16..p0 + 20].copy_from_slice(&24u32.to_le_bytes());

    if has_pt_tls {
        let p1 = phoff + 32;
        buf[p1..p1 + 4].copy_from_slice(&7u32.to_le_bytes());
    }

    buf[dyn_off..dyn_off + 4].copy_from_slice(&30u32.to_le_bytes());
    buf[dyn_off + 4..dyn_off + 8].copy_from_slice(&dt_flags.to_le_bytes());
    buf[dyn_off + 8..dyn_off + 12].copy_from_slice(&0x6fff_fffbu32.to_le_bytes());
    buf[dyn_off + 12..dyn_off + 16].copy_from_slice(&dt_flags_1.to_le_bytes());
    buf
}

fn build_synthetic_macho64(hdr_flags: u32, sect_name: &str, sect_flags: u32) -> Vec<u8> {
    let seg_cmd_size = 72usize + 80usize;
    let mut buf = vec![0u8; 32 + seg_cmd_size];

    buf[0..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
    buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // ncmds = 1
    buf[20..24].copy_from_slice(&(seg_cmd_size as u32).to_le_bytes());
    buf[24..28].copy_from_slice(&hdr_flags.to_le_bytes());

    let lc = 32usize;
    buf[lc..lc + 4].copy_from_slice(&0x19u32.to_le_bytes()); // LC_SEGMENT_64
    buf[lc + 4..lc + 8].copy_from_slice(&(seg_cmd_size as u32).to_le_bytes());
    buf[lc + 64..lc + 68].copy_from_slice(&1u32.to_le_bytes()); // nsects = 1

    let sec = lc + 72;
    let name_bytes = sect_name.as_bytes();
    let copy_len = name_bytes.len().min(16);
    buf[sec..sec + copy_len].copy_from_slice(&name_bytes[..copy_len]);
    buf[sec + 64..sec + 68].copy_from_slice(&sect_flags.to_le_bytes());
    buf
}

fn build_synthetic_pe_with_rsds(original_pdb_slot: &str) -> Vec<u8> {
    let lfanew = 0x80usize;
    let opt_size = 240usize; // PE32+
    let sections_off = lfanew + 24 + opt_size;
    let rdata_raw_ptr = 0x400usize;
    let debug_dir_rva = 0x2000u32;
    let cv_rva = 0x2020u32;
    let cv_raw_ptr = rdata_raw_ptr + 0x20;
    let cv_size = 24 + original_pdb_slot.len() + 1;

    let mut buf = vec![0u8; cv_raw_ptr + cv_size + 32];
    buf[0..2].copy_from_slice(b"MZ");
    buf[0x3c..0x40].copy_from_slice(&(lfanew as u32).to_le_bytes());
    buf[lfanew..lfanew + 4].copy_from_slice(b"PE\0\0");

    let coff = lfanew + 4;
    buf[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // 1 section
    buf[coff + 16..coff + 18].copy_from_slice(&(opt_size as u16).to_le_bytes());

    let opt = coff + 20;
    buf[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes()); // PE32+
    buf[opt + 64..opt + 68].copy_from_slice(&0x1234_5678u32.to_le_bytes()); // non-zero checksum
    buf[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes
    // IMAGE_DIRECTORY_ENTRY_DEBUG (index 6)
    buf[opt + 112 + 6 * 8..opt + 112 + 6 * 8 + 4].copy_from_slice(&debug_dir_rva.to_le_bytes());
    buf[opt + 112 + 6 * 8 + 4..opt + 112 + 6 * 8 + 8].copy_from_slice(&28u32.to_le_bytes());

    // Section 0 (.rdata): VA=0x2000, VirtSize=0x1000, RawSize=0x400, RawPtr=0x400
    buf[sections_off..sections_off + 6].copy_from_slice(b".rdata");
    buf[sections_off + 8..sections_off + 12].copy_from_slice(&0x1000u32.to_le_bytes());
    buf[sections_off + 12..sections_off + 16].copy_from_slice(&0x2000u32.to_le_bytes());
    buf[sections_off + 16..sections_off + 20].copy_from_slice(&0x400u32.to_le_bytes());
    buf[sections_off + 20..sections_off + 24]
        .copy_from_slice(&(rdata_raw_ptr as u32).to_le_bytes());

    // Debug directory entry at rdata_raw_ptr
    buf[rdata_raw_ptr + 12..rdata_raw_ptr + 16].copy_from_slice(&2u32.to_le_bytes()); // CODEVIEW
    buf[rdata_raw_ptr + 16..rdata_raw_ptr + 20].copy_from_slice(&(cv_size as u32).to_le_bytes());
    buf[rdata_raw_ptr + 20..rdata_raw_ptr + 24].copy_from_slice(&cv_rva.to_le_bytes());
    buf[rdata_raw_ptr + 24..rdata_raw_ptr + 28].copy_from_slice(&(cv_raw_ptr as u32).to_le_bytes());

    // RSDS CodeView record
    buf[cv_raw_ptr..cv_raw_ptr + 4].copy_from_slice(b"RSDS");
    buf[cv_raw_ptr + 24..cv_raw_ptr + 24 + original_pdb_slot.len()]
        .copy_from_slice(original_pdb_slot.as_bytes());

    buf
}

#[test]
fn elf64_and_elf32_residency_detects_nodelete_static_tls_and_pt_tls() {
    let clean = build_synthetic_elf64(false, 0, 0);
    let clean_info = inspect_library_residency(&clean);
    assert_eq!(clean_info.format, BinaryFormat::Elf64);
    assert!(!clean_info.will_pin_resident());

    let pinned64 = build_synthetic_elf64(true, 0x10, 0x08);
    let info64 = inspect_library_residency(&pinned64);
    assert_eq!(info64.format, BinaryFormat::Elf64);
    assert!(info64.nodelete);
    assert!(info64.static_tls);
    assert!(info64.has_tls);
    assert!(info64.will_pin_resident());

    let pinned32 = build_synthetic_elf32(false, 0, 0x08);
    let info32 = inspect_library_residency(&pinned32);
    assert_eq!(info32.format, BinaryFormat::Elf32);
    assert!(info32.nodelete);
    assert!(!info32.has_tls);
    assert!(info32.will_pin_resident());
}

#[test]
fn macho_residency_detects_thread_vars_tlv_and_reexport_flags() {
    let clean = build_synthetic_macho64(0x0010_0000, "__text", 0);
    let clean_info = inspect_library_residency(&clean);
    assert_eq!(clean_info.format, BinaryFormat::MachO64);
    assert!(clean_info.has_no_reexported_dylibs);
    assert!(!clean_info.has_tls);
    assert!(!clean_info.will_pin_resident());

    let tlv_sect = build_synthetic_macho64(0, "__thread_vars", 0x13);
    let tlv_info = inspect_library_residency(&tlv_sect);
    assert_eq!(tlv_info.format, BinaryFormat::MachO64);
    assert!(tlv_info.has_tls);
    assert!(tlv_info.will_pin_resident());

    let tlv_hdr = build_synthetic_macho64(0x0080_0000, "__data", 0);
    let hdr_info = inspect_library_residency(&tlv_hdr);
    assert!(hdr_info.has_tls);
    assert!(hdr_info.will_pin_resident());

    // Truncated/unknown data never panics.
    assert_eq!(
        inspect_library_residency(&[1, 2, 3]),
        LibraryResidencyInfo::default()
    );
}

#[test]
fn shadow_copy_stages_and_cleans_up_pdb_sidecar_and_patches_rsds() {
    let orig_pdb_str = "C:\\very\\long\\build\\output\\target\\debug\\sample_mod.pdb";
    let pe_bytes = build_synthetic_pe_with_rsds(orig_pdb_str);
    if cfg!(miri) {
        let mut pe = pe_bytes;
        assert!(patch_pe_codeview_pdb_path(
            &mut pe,
            Path::new("shadow_sample_mod.pdb")
        ));
        assert_eq!(&pe[0x80 + 24 + 64..0x80 + 24 + 68], &[0, 0, 0, 0]);
        return;
    }

    let dir = std::env::temp_dir().join(format!(
        "bam_pdb_shadow_test_{}_{}",
        std::process::id(),
        NEXT_SHADOW_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let src_dir = dir.join("src");
    let shd_dir = dir.join("shadow");
    std::fs::create_dir_all(&src_dir).unwrap();

    let dll_path = src_dir.join("sample_mod.dll");
    let pdb_path = src_dir.join("sample_mod.pdb");

    std::fs::write(&dll_path, &pe_bytes).unwrap();
    std::fs::write(&pdb_path, b"FAKE_PDB_DATA").unwrap();

    let guard = stage_shadow_copy(&dll_path, Some(&shd_dir)).unwrap();
    let shadow_dll = guard.path().to_path_buf();
    let shadow_pdb = guard
        .pdb_path()
        .expect("expected shadow .pdb sidecar to be staged")
        .to_path_buf();

    assert!(shadow_dll.is_file());
    assert!(shadow_pdb.is_file());
    assert_eq!(
        shadow_pdb.file_stem(),
        shadow_dll.file_stem(),
        "shadow .pdb must share unique per-generation stem with shadow .dll"
    );
    assert_eq!(std::fs::read(&shadow_pdb).unwrap(), b"FAKE_PDB_DATA");

    let patched_dll = std::fs::read(&shadow_dll).unwrap();
    let expected_pdb_name = shadow_pdb.file_name().unwrap().to_str().unwrap();
    assert!(
        patched_dll
            .windows(expected_pdb_name.len())
            .any(|w| w == expected_pdb_name.as_bytes()),
        "RSDS CodeView PDB path should be rewritten to shadow .pdb"
    );
    // CheckSum cleared to 0 after patching.
    assert_eq!(&patched_dll[0x80 + 24 + 64..0x80 + 24 + 68], &[0, 0, 0, 0]);

    drop(guard);
    assert!(!shadow_dll.exists());
    assert!(!shadow_pdb.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

fn build_synthetic_macho32(hdr_flags: u32, sect_name: &str, sect_flags: u32) -> Vec<u8> {
    let seg_cmd_size = 56usize + 68usize;
    let mut buf = vec![0u8; 28 + seg_cmd_size];

    buf[0..4].copy_from_slice(&0xfeed_faceu32.to_le_bytes());
    buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // ncmds = 1
    buf[20..24].copy_from_slice(&(seg_cmd_size as u32).to_le_bytes());
    buf[24..28].copy_from_slice(&hdr_flags.to_le_bytes());

    let lc = 28usize;
    buf[lc..lc + 4].copy_from_slice(&0x01u32.to_le_bytes()); // LC_SEGMENT
    buf[lc + 4..lc + 8].copy_from_slice(&(seg_cmd_size as u32).to_le_bytes());
    buf[lc + 48..lc + 52].copy_from_slice(&1u32.to_le_bytes()); // nsects = 1

    let sec = lc + 56;
    let name_bytes = sect_name.as_bytes();
    let copy_len = name_bytes.len().min(16);
    buf[sec..sec + copy_len].copy_from_slice(&name_bytes[..copy_len]);
    buf[sec + 56..sec + 60].copy_from_slice(&sect_flags.to_le_bytes());
    buf
}

fn build_synthetic_macho_fat(
    is_64_fat: bool,
    hdr_flags: u32,
    sect_name: &str,
    sect_flags: u32,
) -> Vec<u8> {
    let slice = if is_64_fat {
        build_synthetic_macho64(hdr_flags, sect_name, sect_flags)
    } else {
        build_synthetic_macho32(hdr_flags, sect_name, sect_flags)
    };
    let arch_size = if is_64_fat { 32usize } else { 20usize };
    let slice_off = 8usize + arch_size;
    let mut buf = vec![0u8; slice_off + slice.len()];

    let magic = if is_64_fat {
        0xcafe_babfu32
    } else {
        0xcafe_babeu32
    };
    buf[0..4].copy_from_slice(&magic.to_be_bytes());
    buf[4..8].copy_from_slice(&1u32.to_be_bytes()); // nfat_arch = 1
    if is_64_fat {
        buf[16..24].copy_from_slice(&(slice_off as u64).to_be_bytes());
        buf[24..32].copy_from_slice(&(slice.len() as u64).to_be_bytes());
    } else {
        buf[16..20].copy_from_slice(&(slice_off as u32).to_be_bytes());
        buf[20..24].copy_from_slice(&(slice.len() as u32).to_be_bytes());
    }
    buf[slice_off..].copy_from_slice(&slice);
    buf
}

fn build_synthetic_macho_fat_two_slices(
    is_64_fat: bool,
    slice0_flags: u32,
    slice1_tls: bool,
) -> Vec<u8> {
    let slice0 = build_synthetic_macho32(slice0_flags, "__text", 0);
    let slice1 = build_synthetic_macho64(
        0,
        if slice1_tls {
            "__thread_vars"
        } else {
            "__data"
        },
        if slice1_tls { 0x13 } else { 0 },
    );
    let arch_size = if is_64_fat { 32usize } else { 20usize };
    let off0 = 8usize + 2 * arch_size;
    let off1 = off0 + slice0.len();
    let mut buf = vec![0u8; off1 + slice1.len()];

    let magic = if is_64_fat {
        0xcafe_babfu32
    } else {
        0xcafe_babeu32
    };
    buf[0..4].copy_from_slice(&magic.to_be_bytes());
    buf[4..8].copy_from_slice(&2u32.to_be_bytes()); // nfat_arch = 2
    if is_64_fat {
        buf[16..24].copy_from_slice(&(off0 as u64).to_be_bytes());
        buf[24..32].copy_from_slice(&(slice0.len() as u64).to_be_bytes());
        let a1 = 8 + arch_size;
        buf[a1 + 8..a1 + 16].copy_from_slice(&(off1 as u64).to_be_bytes());
        buf[a1 + 16..a1 + 24].copy_from_slice(&(slice1.len() as u64).to_be_bytes());
    } else {
        buf[16..20].copy_from_slice(&(off0 as u32).to_be_bytes());
        buf[20..24].copy_from_slice(&(slice0.len() as u32).to_be_bytes());
        let a1 = 8 + arch_size;
        buf[a1 + 8..a1 + 12].copy_from_slice(&(off1 as u32).to_be_bytes());
        buf[a1 + 12..a1 + 16].copy_from_slice(&(slice1.len() as u32).to_be_bytes());
    }
    buf[off0..off0 + slice0.len()].copy_from_slice(&slice0);
    buf[off1..].copy_from_slice(&slice1);
    buf
}

fn build_synthetic_pe_variant(is_pe32_plus: bool, original_pdb_slot: &str) -> Vec<u8> {
    if is_pe32_plus {
        return build_synthetic_pe_with_rsds(original_pdb_slot);
    }
    let lfanew = 0x80usize;
    let opt_size = 224usize; // PE32
    let sections_off = lfanew + 24 + opt_size;
    let rdata_raw_ptr = 0x400usize;
    let debug_dir_rva = 0x2000u32;
    let cv_rva = 0x2020u32;
    let cv_raw_ptr = rdata_raw_ptr + 0x20;
    let cv_size = 24 + original_pdb_slot.len() + 1;

    let mut buf = vec![0u8; cv_raw_ptr + cv_size + 32];
    buf[0..2].copy_from_slice(b"MZ");
    buf[0x3c..0x40].copy_from_slice(&(lfanew as u32).to_le_bytes());
    buf[lfanew..lfanew + 4].copy_from_slice(b"PE\0\0");

    let coff = lfanew + 4;
    buf[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
    buf[coff + 16..coff + 18].copy_from_slice(&(opt_size as u16).to_le_bytes());

    let opt = coff + 20;
    buf[opt..opt + 2].copy_from_slice(&0x10bu16.to_le_bytes()); // PE32
    buf[opt + 64..opt + 68].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    buf[opt + 92..opt + 96].copy_from_slice(&16u32.to_le_bytes());
    buf[opt + 96 + 6 * 8..opt + 96 + 6 * 8 + 4].copy_from_slice(&debug_dir_rva.to_le_bytes());
    buf[opt + 96 + 6 * 8 + 4..opt + 96 + 6 * 8 + 8].copy_from_slice(&28u32.to_le_bytes());

    buf[sections_off..sections_off + 6].copy_from_slice(b".rdata");
    buf[sections_off + 8..sections_off + 12].copy_from_slice(&0x1000u32.to_le_bytes());
    buf[sections_off + 12..sections_off + 16].copy_from_slice(&0x2000u32.to_le_bytes());
    buf[sections_off + 16..sections_off + 20].copy_from_slice(&0x400u32.to_le_bytes());
    buf[sections_off + 20..sections_off + 24]
        .copy_from_slice(&(rdata_raw_ptr as u32).to_le_bytes());

    buf[rdata_raw_ptr + 12..rdata_raw_ptr + 16].copy_from_slice(&2u32.to_le_bytes());
    buf[rdata_raw_ptr + 16..rdata_raw_ptr + 20].copy_from_slice(&(cv_size as u32).to_le_bytes());
    buf[rdata_raw_ptr + 20..rdata_raw_ptr + 24].copy_from_slice(&cv_rva.to_le_bytes());
    buf[rdata_raw_ptr + 24..rdata_raw_ptr + 28].copy_from_slice(&(cv_raw_ptr as u32).to_le_bytes());

    buf[cv_raw_ptr..cv_raw_ptr + 4].copy_from_slice(b"RSDS");
    buf[cv_raw_ptr + 24..cv_raw_ptr + 24 + original_pdb_slot.len()]
        .copy_from_slice(original_pdb_slot.as_bytes());
    buf
}

fn assert_residency_subset(trunc: LibraryResidencyInfo, full: LibraryResidencyInfo) {
    assert!(!trunc.nodelete || full.nodelete);
    assert!(!trunc.static_tls || full.static_tls);
    assert!(!trunc.has_tls || full.has_tls);
    assert!(!trunc.has_no_reexported_dylibs || full.has_no_reexported_dylibs);
    assert!(!trunc.will_pin_resident() || full.will_pin_resident());
}

#[test]
fn parser_oversized_offsets_and_truncated_headers_regression() {
    // Minimized regression: PE section raw_ptr=u32::MAX, delta=1 must not overflow `raw_ptr + delta`.
    let mut pe_overflow = build_synthetic_pe_with_rsds("C:\\build\\sample.pdb");
    let sections_off = 0x80 + 24 + 240;
    pe_overflow[sections_off + 20..sections_off + 24].copy_from_slice(&u32::MAX.to_le_bytes());
    pe_overflow[sections_off + 16..sections_off + 20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        pe_rva_to_file_offset(&pe_overflow, sections_off, 1, 0x2001),
        if usize::BITS <= 32 {
            None
        } else {
            Some(u32::MAX as usize + 1)
        }
    );
    let orig_pe_overflow = pe_overflow.clone();
    assert!(!patch_pe_codeview_pdb_path(
        &mut pe_overflow,
        Path::new("shadow.pdb")
    ));
    assert_eq!(pe_overflow, orig_pe_overflow);

    // Minimized regression: ELF64 oversized phoff/phentsize/phnum/dyn_off/dyn_size.
    let mut elf64 = build_synthetic_elf64(true, 0x10, 0x08);
    elf64[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
    let info = inspect_library_residency(&elf64);
    assert_eq!(info, inspect_library_residency(&elf64));

    let mut elf64_dyn = build_synthetic_elf64(false, 0x10, 0x08);
    let p0 = 64usize;
    elf64_dyn[p0 + 8..p0 + 16].copy_from_slice(&u64::MAX.to_le_bytes());
    elf64_dyn[p0 + 32..p0 + 40].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(
        inspect_library_residency(&elf64_dyn),
        LibraryResidencyInfo {
            format: BinaryFormat::Elf64,
            ..Default::default()
        }
    );

    // Minimized regression: Mach-O 64 oversized cmdsize / ncmds / nsects.
    let mut macho64 = build_synthetic_macho64(0x0010_0000, "__thread_vars", 0x13);
    macho64[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    macho64[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
    macho64[32 + 4..32 + 8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        inspect_library_residency(&macho64),
        inspect_library_residency(&macho64)
    );

    // Minimized regression: Mach-O Fat 64 oversized slice_off + slice_size.
    let mut fat64 = build_synthetic_macho_fat(true, 0, "__thread_vars", 0x13);
    fat64[16..24].copy_from_slice(&(u64::MAX - 4).to_be_bytes());
    fat64[24..32].copy_from_slice(&64u64.to_be_bytes());
    assert_eq!(
        inspect_library_residency(&fat64),
        LibraryResidencyInfo::default()
    );

    // Minimized regression: PE with opt_size=96 and PE32+ magic (0x20b) returns false without mutation.
    let mut pe_small_opt = build_synthetic_pe_with_rsds("C:\\build\\sample.pdb");
    let coff = 0x80 + 4;
    pe_small_opt[coff + 16..coff + 18].copy_from_slice(&96u16.to_le_bytes());
    let orig_small = pe_small_opt.clone();
    assert!(!patch_pe_codeview_pdb_path(
        &mut pe_small_opt,
        Path::new("shadow.pdb")
    ));
    assert_eq!(pe_small_opt, orig_small);
}

mod proptest_shared_lib_parsers {
    use super::*;
    use proptest::prelude::*;
    use std::ops::Range;

    fn proptest_cases() -> u32 {
        if cfg!(miri) {
            return 4;
        }
        if let Ok(raw) = std::env::var("PROPTEST_CASES")
            && let Ok(parsed) = raw.parse::<u32>()
            && parsed > 0
        {
            return parsed;
        }
        256
    }

    const OVERSIZED_VALUES: &[u64] = &[
        0,
        1,
        7,
        24,
        25,
        28,
        0x7fff_ffff,
        0x8000_0000,
        0xffff_ff00,
        u32::MAX as u64,
        u64::MAX - 16,
        u64::MAX,
    ];

    #[derive(Clone, Debug)]
    enum BaseImageKind {
        Elf64 {
            has_pt_tls: bool,
            dt_flags: u64,
            dt_flags_1: u64,
        },
        Elf32 {
            has_pt_tls: bool,
            dt_flags: u32,
            dt_flags_1: u32,
        },
        MachO64 {
            hdr_flags: u32,
            tls_sect: bool,
            sect_flags: u32,
        },
        MachO32 {
            hdr_flags: u32,
            tls_sect: bool,
            sect_flags: u32,
        },
        MachOFat {
            is_64_fat: bool,
            two_slices: bool,
            hdr_flags: u32,
            tls_sect: bool,
            sect_flags: u32,
        },
        Pe {
            is_pe32_plus: bool,
            slot_len: usize,
        },
    }

    fn build_base_image(kind: &BaseImageKind) -> Vec<u8> {
        match *kind {
            BaseImageKind::Elf64 {
                has_pt_tls,
                dt_flags,
                dt_flags_1,
            } => build_synthetic_elf64(has_pt_tls, dt_flags, dt_flags_1),
            BaseImageKind::Elf32 {
                has_pt_tls,
                dt_flags,
                dt_flags_1,
            } => build_synthetic_elf32(has_pt_tls, dt_flags, dt_flags_1),
            BaseImageKind::MachO64 {
                hdr_flags,
                tls_sect,
                sect_flags,
            } => {
                let name = if tls_sect { "__thread_vars" } else { "__text" };
                build_synthetic_macho64(hdr_flags, name, sect_flags)
            }
            BaseImageKind::MachO32 {
                hdr_flags,
                tls_sect,
                sect_flags,
            } => {
                let name = if tls_sect { "__thread_bss" } else { "__text" };
                build_synthetic_macho32(hdr_flags, name, sect_flags)
            }
            BaseImageKind::MachOFat {
                is_64_fat,
                two_slices,
                hdr_flags,
                tls_sect,
                sect_flags,
            } => {
                if two_slices {
                    build_synthetic_macho_fat_two_slices(is_64_fat, hdr_flags, tls_sect)
                } else {
                    let name = if tls_sect { "__thread_data" } else { "__text" };
                    build_synthetic_macho_fat(is_64_fat, hdr_flags, name, sect_flags)
                }
            }
            BaseImageKind::Pe {
                is_pe32_plus,
                slot_len,
            } => {
                let slot = "x".repeat(slot_len);
                build_synthetic_pe_variant(is_pe32_plus, &slot)
            }
        }
    }

    fn candidate_header_fields(kind: &BaseImageKind) -> &'static [(usize, usize, bool)] {
        // (offset, width_bytes, little_endian)
        match kind {
            BaseImageKind::Elf64 { .. } => &[
                (32, 8, true), // e_phoff
                (54, 2, true), // e_phentsize
                (56, 2, true), // e_phnum
                (72, 8, true), // phdr[0].p_offset
                (96, 8, true), // phdr[0].p_filesz
            ],
            BaseImageKind::Elf32 { .. } => &[
                (28, 4, true), // e_phoff
                (42, 2, true), // e_phentsize
                (44, 2, true), // e_phnum
                (56, 4, true), // phdr[0].p_offset
                (68, 4, true), // phdr[0].p_filesz
            ],
            BaseImageKind::MachO64 { .. } => &[
                (16, 4, true), // ncmds
                (20, 4, true), // sizeofcmds
                (36, 4, true), // lc.cmdsize
                (96, 4, true), // seg64.nsects
            ],
            BaseImageKind::MachO32 { .. } => &[
                (16, 4, true), // ncmds
                (20, 4, true), // sizeofcmds
                (32, 4, true), // lc.cmdsize
                (76, 4, true), // seg.nsects
            ],
            BaseImageKind::MachOFat {
                is_64_fat: true, ..
            } => &[
                (4, 4, false),  // nfat_arch
                (16, 8, false), // fat_arch_64.offset
                (24, 8, false), // fat_arch_64.size
            ],
            BaseImageKind::MachOFat {
                is_64_fat: false, ..
            } => &[
                (4, 4, false),  // nfat_arch
                (16, 4, false), // fat_arch.offset
                (20, 4, false), // fat_arch.size
            ],
            BaseImageKind::Pe {
                is_pe32_plus: true, ..
            } => &[
                (0x3c, 4, true),            // e_lfanew
                (0x80 + 6, 2, true),        // NumberOfSections
                (0x80 + 20, 2, true),       // SizeOfOptionalHeader
                (0x80 + 24 + 108, 4, true), // NumberOfRvaAndSizes
                (0x80 + 24 + 160, 4, true), // Debug directory RVA
                (0x80 + 24 + 164, 4, true), // Debug directory Size
                (0x400 + 16, 4, true),      // CodeView SizeOfData
                (0x400 + 24, 4, true),      // CodeView PointerToRawData
            ],
            BaseImageKind::Pe {
                is_pe32_plus: false,
                ..
            } => &[
                (0x3c, 4, true),            // e_lfanew
                (0x80 + 6, 2, true),        // NumberOfSections
                (0x80 + 20, 2, true),       // SizeOfOptionalHeader
                (0x80 + 24 + 92, 4, true),  // NumberOfRvaAndSizes
                (0x80 + 24 + 144, 4, true), // Debug directory RVA
                (0x80 + 24 + 148, 4, true), // Debug directory Size
                (0x400 + 16, 4, true),      // CodeView SizeOfData
                (0x400 + 24, 4, true),      // CodeView PointerToRawData
            ],
        }
    }

    fn apply_mutations(
        mut buf: Vec<u8>,
        kind: &BaseImageKind,
        flips: &[(usize, u8)],
        field_patches: &[(usize, usize)],
        trunc_frac: Option<u16>,
    ) -> Vec<u8> {
        let fields = candidate_header_fields(kind);
        for &(field_sel, val_sel) in field_patches {
            let (off, width, le) = fields[field_sel % fields.len()];
            let val = OVERSIZED_VALUES[val_sel % OVERSIZED_VALUES.len()];
            if off + width <= buf.len() {
                match (width, le) {
                    (2, true) => buf[off..off + 2].copy_from_slice(&(val as u16).to_le_bytes()),
                    (2, false) => buf[off..off + 2].copy_from_slice(&(val as u16).to_be_bytes()),
                    (4, true) => buf[off..off + 4].copy_from_slice(&(val as u32).to_le_bytes()),
                    (4, false) => buf[off..off + 4].copy_from_slice(&(val as u32).to_be_bytes()),
                    (8, true) => buf[off..off + 8].copy_from_slice(&val.to_le_bytes()),
                    (8, false) => buf[off..off + 8].copy_from_slice(&val.to_be_bytes()),
                    _ => {}
                }
            }
        }
        if !buf.is_empty() {
            for &(idx, mask) in flips {
                let pos = idx % buf.len();
                buf[pos] ^= mask;
            }
        }
        if let Some(frac) = trunc_frac {
            let new_len = (buf.len() * (frac as usize)) / (u16::MAX as usize + 1);
            buf.truncate(new_len);
        }
        buf
    }

    fn arb_base_image_kind() -> impl Strategy<Value = BaseImageKind> {
        prop_oneof![
            (
                any::<bool>(),
                prop_oneof![Just(0u64), Just(0x10u64)],
                prop_oneof![Just(0u64), Just(0x08u64)]
            )
                .prop_map(|(has_pt_tls, dt_flags, dt_flags_1)| BaseImageKind::Elf64 {
                    has_pt_tls,
                    dt_flags,
                    dt_flags_1,
                }),
            (
                any::<bool>(),
                prop_oneof![Just(0u32), Just(0x10u32)],
                prop_oneof![Just(0u32), Just(0x08u32)]
            )
                .prop_map(|(has_pt_tls, dt_flags, dt_flags_1)| BaseImageKind::Elf32 {
                    has_pt_tls,
                    dt_flags,
                    dt_flags_1,
                }),
            (
                prop_oneof![Just(0u32), Just(0x0010_0000u32), Just(0x0080_0000u32)],
                any::<bool>(),
                prop_oneof![Just(0u32), Just(0x13u32)]
            )
                .prop_map(|(hdr_flags, tls_sect, sect_flags)| BaseImageKind::MachO64 {
                    hdr_flags,
                    tls_sect,
                    sect_flags,
                }),
            (
                prop_oneof![Just(0u32), Just(0x0010_0000u32), Just(0x0080_0000u32)],
                any::<bool>(),
                prop_oneof![Just(0u32), Just(0x13u32)]
            )
                .prop_map(|(hdr_flags, tls_sect, sect_flags)| BaseImageKind::MachO32 {
                    hdr_flags,
                    tls_sect,
                    sect_flags,
                }),
            (
                any::<bool>(),
                any::<bool>(),
                prop_oneof![Just(0u32), Just(0x0010_0000u32), Just(0x0080_0000u32)],
                any::<bool>(),
                prop_oneof![Just(0u32), Just(0x13u32)]
            )
                .prop_map(
                    |(is_64_fat, two_slices, hdr_flags, tls_sect, sect_flags)| {
                        BaseImageKind::MachOFat {
                            is_64_fat,
                            two_slices,
                            hdr_flags,
                            tls_sect,
                            sect_flags,
                        }
                    }
                ),
            (any::<bool>(), 1usize..=80usize).prop_map(|(is_pe32_plus, slot_len)| {
                BaseImageKind::Pe {
                    is_pe32_plus,
                    slot_len,
                }
            }),
        ]
    }

    fn collect_allowed_pe_write_ranges(
        pe_bytes: &[u8],
    ) -> (Option<Range<usize>>, Vec<Range<usize>>) {
        let Some(lfanew) = pe_header_offset(pe_bytes) else {
            return (None, Vec::new());
        };
        let Some(coff_off) = lfanew.checked_add(4) else {
            return (None, Vec::new());
        };
        let Some(num_sections) = coff_off
            .checked_add(2)
            .and_then(|o| read_u16_end(pe_bytes, o, true))
            .map(usize::from)
        else {
            return (None, Vec::new());
        };
        let Some(opt_size) = coff_off
            .checked_add(16)
            .and_then(|o| read_u16_end(pe_bytes, o, true))
            .map(usize::from)
        else {
            return (None, Vec::new());
        };
        let Some(opt_off) = coff_off.checked_add(20) else {
            return (None, Vec::new());
        };
        let Some(opt_end) = opt_off.checked_add(opt_size) else {
            return (None, Vec::new());
        };
        if opt_size < 96 || opt_end > pe_bytes.len() {
            return (None, Vec::new());
        }
        let Some(opt_magic) = read_u16_end(pe_bytes, opt_off, true) else {
            return (None, Vec::new());
        };
        let (Some(num_rva_off), Some(data_dir_off)) = (match opt_magic {
            0x10b => (opt_off.checked_add(92), opt_off.checked_add(96)),
            0x20b => (opt_off.checked_add(108), opt_off.checked_add(112)),
            _ => return (None, Vec::new()),
        }) else {
            return (None, Vec::new());
        };
        let Some(num_rva) = read_u32_end(pe_bytes, num_rva_off, true) else {
            return (None, Vec::new());
        };
        let Some(dir_end) = data_dir_off.checked_add(7 * 8) else {
            return (None, Vec::new());
        };
        if num_rva <= 6 || dir_end > opt_end {
            return (None, Vec::new());
        }
        let Some(debug_rva) = data_dir_off
            .checked_add(6 * 8)
            .and_then(|o| read_u32_end(pe_bytes, o, true))
        else {
            return (None, Vec::new());
        };
        let Some(debug_size) = data_dir_off
            .checked_add(6 * 8 + 4)
            .and_then(|o| read_u32_end(pe_bytes, o, true))
            .and_then(|v| usize::try_from(v).ok())
        else {
            return (None, Vec::new());
        };
        if debug_rva == 0 || debug_size < 28 {
            return (None, Vec::new());
        }
        let Some(debug_file_off) =
            pe_rva_to_file_offset(pe_bytes, opt_end, num_sections, debug_rva)
        else {
            return (None, Vec::new());
        };
        let Some(debug_end) = debug_file_off.checked_add(debug_size) else {
            return (None, Vec::new());
        };
        if debug_end > pe_bytes.len() {
            return (None, Vec::new());
        }

        let checksum_range = opt_off
            .checked_add(64)
            .and_then(|s| opt_off.checked_add(68).map(|e| s..e));
        let mut rsds_slots = Vec::new();
        for i in 0..(debug_size / 28) {
            let Some(entry_off) = i
                .checked_mul(28)
                .and_then(|d| debug_file_off.checked_add(d))
            else {
                break;
            };
            let Some(dbg_type) = entry_off
                .checked_add(12)
                .and_then(|o| read_u32_end(pe_bytes, o, true))
            else {
                continue;
            };
            if dbg_type != 2 {
                continue;
            }
            let Some(cv_size) = entry_off
                .checked_add(16)
                .and_then(|o| read_u32_end(pe_bytes, o, true))
                .and_then(|v| usize::try_from(v).ok())
            else {
                continue;
            };
            let Some(cv_ptr) = entry_off
                .checked_add(24)
                .and_then(|o| read_u32_end(pe_bytes, o, true))
                .and_then(|v| usize::try_from(v).ok())
            else {
                continue;
            };
            let Some(cv_end) = cv_ptr.checked_add(cv_size) else {
                continue;
            };
            if cv_size <= 25 || cv_end > pe_bytes.len() {
                continue;
            }
            if pe_bytes.get(cv_ptr..cv_ptr + 4) != Some(b"RSDS") {
                continue;
            }
            if let Some(slot_start) = cv_ptr.checked_add(24) {
                rsds_slots.push(slot_start..cv_end);
            }
        }
        (checksum_range, rsds_slots)
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: proptest_cases(),
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        #[test]
        fn arbitrary_bytes_inspect_library_residency_never_panics_and_is_deterministic(
            bytes in prop::collection::vec(any::<u8>(), 0..=512),
        ) {
            let first = inspect_library_residency(&bytes);
            let second = inspect_library_residency(&bytes);
            prop_assert_eq!(first, second);
        }

        #[test]
        fn mutated_synthetic_images_inspect_never_panics_and_is_deterministic(
            kind in arb_base_image_kind(),
            flips in prop::collection::vec((any::<usize>(), any::<u8>()), 0..=8),
            field_patches in prop::collection::vec((any::<usize>(), any::<usize>()), 0..=4),
            trunc_frac in prop::option::of(any::<u16>()),
        ) {
            let base = build_base_image(&kind);
            let mutated = apply_mutations(base, &kind, &flips, &field_patches, trunc_frac);
            let first = inspect_library_residency(&mutated);
            let second = inspect_library_residency(&mutated);
            prop_assert_eq!(first, second);
        }

        #[test]
        fn truncating_valid_images_at_every_length_is_monotonic_and_never_panics(
            kind in arb_base_image_kind(),
        ) {
            let full_bytes = build_base_image(&kind);
            let full_info = inspect_library_residency(&full_bytes);
            for len in 0..=full_bytes.len() {
                let prefix = &full_bytes[..len];
                let trunc_info = inspect_library_residency(prefix);
                prop_assert_eq!(trunc_info, inspect_library_residency(prefix));
                assert_residency_subset(trunc_info, full_info);
            }
        }

        #[test]
        fn mutated_pe_patch_codeview_pdb_path_is_safe_deterministic_and_bounded(
            is_pe32_plus in any::<bool>(),
            slot_len in 1usize..=80usize,
            flips in prop::collection::vec((any::<usize>(), any::<u8>()), 0..=8),
            field_patches in prop::collection::vec((any::<usize>(), any::<usize>()), 0..=4),
            trunc_frac in prop::option::of(any::<u16>()),
            pdb_choice in prop_oneof![
                Just("s.pdb"),
                Just("shadow_sample_mod.pdb"),
                Just("/tmp/bamabi_shadows/very_long_directory_name/shadow_mod_12345.pdb"),
                Just("a_path_that_is_intentionally_much_longer_than_eighty_bytes_for_overflow_testing_1234567890.pdb"),
            ],
        ) {
            let kind = BaseImageKind::Pe {
                is_pe32_plus,
                slot_len,
            };
            let base = build_base_image(&kind);
            let orig = apply_mutations(base, &kind, &flips, &field_patches, trunc_frac);
            let mut buf_a = orig.clone();
            let mut buf_b = orig.clone();

            let pdb_path = Path::new(pdb_choice);
            let res_a = patch_pe_codeview_pdb_path(&mut buf_a, pdb_path);
            let res_b = patch_pe_codeview_pdb_path(&mut buf_b, pdb_path);

            prop_assert_eq!(res_a, res_b);
            prop_assert_eq!(&buf_a, &buf_b);

            if !res_a {
                prop_assert_eq!(&buf_a, &orig);
            } else {
                let (checksum_range, rsds_slots) = collect_allowed_pe_write_ranges(&orig);
                prop_assert!(!rsds_slots.is_empty());
                for (idx, (&before, &after)) in orig.iter().zip(buf_a.iter()).enumerate() {
                    if before != after {
                        let in_rsds_slot = rsds_slots.iter().any(|r| r.contains(&idx));
                        let in_checksum = checksum_range.as_ref().is_some_and(|r| r.contains(&idx));
                        prop_assert!(
                            in_rsds_slot || in_checksum,
                            "unexpected mutation at byte offset {idx:#x} outside RSDS slots {rsds_slots:?} and checksum {checksum_range:?}"
                        );
                    }
                }
            }
        }
    }
}

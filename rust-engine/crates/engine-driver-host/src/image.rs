//! Executable image checks done with plain byte reads: the file is the right
//! container for the OS and was built for the right CPU. Not a security
//! boundary (the signed manifest's SHA-256 is); it turns "wrong download" into
//! a clear error instead of a loader failure.

use crate::{DriverVerificationError, TargetArch, TargetOs};
use std::io::{Read, Seek, SeekFrom};

const FAT_MAGIC: [u8; 4] = [0xCA, 0xFE, 0xBA, 0xBE];
const FAT_MAGIC_SWAPPED: [u8; 4] = [0xBE, 0xBA, 0xFE, 0xCA];
const MACHO_CPU_ARM64: u32 = 0x0100_000C;
const MACHO_CPU_X86_64: u32 = 0x0100_0007;

const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const ELF_CLASS_64: u8 = 2;
const ELF_DATA_LSB: u8 = 1;
const ELF_TYPE_DYN: u16 = 3;
const ELF_MACHINE_X86_64: u16 = 62;
const ELF_MACHINE_AARCH64: u16 = 183;

const PE_MACHINE_AMD64: u16 = 0x8664;
const PE_MACHINE_ARM64: u16 = 0xAA64;
const PE_FILE_DLL: u16 = 0x2000;
/// A PE header further than this into the file is not a real DLL.
const PE_MAX_HEADER_OFFSET: u32 = 1 << 20;

/// The executable container an OS loads: Mach-O, ELF or PE/COFF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageFormat {
    MachO,
    Elf,
    Pe,
}

impl ImageFormat {
    pub(crate) fn for_os(os: TargetOs) -> Result<Self, DriverVerificationError> {
        match os {
            TargetOs::Macos => Ok(Self::MachO),
            TargetOs::Linux => Ok(Self::Elf),
            TargetOs::Windows => Ok(Self::Pe),
            TargetOs::Other(name) => Err(unsupported(format!("unsupported host OS: {name}"))),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::MachO => "Mach-O",
            Self::Elf => "ELF",
            Self::Pe => "PE",
        }
    }

    /// Check the header of the image in `reader` against `arch`.
    pub(crate) fn verify<R: Read + Seek>(
        self,
        reader: &mut R,
        arch: TargetArch,
    ) -> Result<(), DriverVerificationError> {
        match self {
            Self::MachO => verify_mach_o(reader, arch),
            Self::Elf => verify_elf(reader, arch),
            Self::Pe => verify_pe(reader, arch),
        }
    }
}

fn unsupported(message: String) -> DriverVerificationError {
    DriverVerificationError::ArchUnsupported(message)
}

fn not_an_image(format: ImageFormat) -> DriverVerificationError {
    unsupported(format!("file is not a {} image", format.name()))
}

/// Read `len` bytes at `offset`; a short file is a bad image, not an I/O fault.
fn read_at<R: Read + Seek, const N: usize>(
    reader: &mut R,
    offset: u64,
    format: ImageFormat,
) -> Result<[u8; N], DriverVerificationError> {
    let mut buffer = [0u8; N];
    reader.seek(SeekFrom::Start(offset))?;
    match reader.read_exact(&mut buffer) {
        Ok(()) => Ok(buffer),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Err(unsupported(
            format!("file is too short to be a {} image", format.name()),
        )),
        Err(error) => Err(error.into()),
    }
}

fn mismatch(format: ImageFormat, found: String, arch: TargetArch) -> DriverVerificationError {
    unsupported(format!(
        "{} image built for {found} does not match {arch:?}",
        format.name()
    ))
}

fn verify_mach_o<R: Read + Seek>(
    reader: &mut R,
    arch: TargetArch,
) -> Result<(), DriverVerificationError> {
    let header: [u8; 8] = read_at(reader, 0, ImageFormat::MachO)?;
    let magic = [header[0], header[1], header[2], header[3]];
    if magic == FAT_MAGIC || magic == FAT_MAGIC_SWAPPED {
        return Ok(());
    }
    let cpu_bytes = [header[4], header[5], header[6], header[7]];
    let cpu_type = match magic {
        // Little-endian Mach-O: magic and cputype stored as LE.
        [0xCF, 0xFA, 0xED, 0xFE] | [0xCE, 0xFA, 0xED, 0xFE] => u32::from_le_bytes(cpu_bytes),
        // Big-endian Mach-O: magic and cputype stored as BE.
        [0xFE, 0xED, 0xFA, 0xCF] | [0xFE, 0xED, 0xFA, 0xCE] => u32::from_be_bytes(cpu_bytes),
        _ => return Err(not_an_image(ImageFormat::MachO)),
    };
    let required = match arch {
        TargetArch::Aarch64 => MACHO_CPU_ARM64,
        TargetArch::X86_64 => MACHO_CPU_X86_64,
        TargetArch::Other(name) => {
            return Err(unsupported(format!(
                "unsupported host architecture: {name}"
            )))
        }
    };
    if cpu_type == required {
        Ok(())
    } else {
        Err(mismatch(
            ImageFormat::MachO,
            format!("CPU type {cpu_type:#x}"),
            arch,
        ))
    }
}

fn verify_elf<R: Read + Seek>(
    reader: &mut R,
    arch: TargetArch,
) -> Result<(), DriverVerificationError> {
    let header: [u8; 20] = read_at(reader, 0, ImageFormat::Elf)?;
    if header[..4] != ELF_MAGIC {
        return Err(not_an_image(ImageFormat::Elf));
    }
    if header[4] != ELF_CLASS_64 || header[5] != ELF_DATA_LSB {
        return Err(unsupported("ELF image is not 64-bit little-endian".into()));
    }
    let kind = u16::from_le_bytes([header[16], header[17]]);
    if kind != ELF_TYPE_DYN {
        return Err(unsupported(format!(
            "ELF image is not a shared object (e_type {kind})"
        )));
    }
    let machine = u16::from_le_bytes([header[18], header[19]]);
    let required = match arch {
        TargetArch::Aarch64 => ELF_MACHINE_AARCH64,
        TargetArch::X86_64 => ELF_MACHINE_X86_64,
        TargetArch::Other(name) => {
            return Err(unsupported(format!(
                "unsupported host architecture: {name}"
            )))
        }
    };
    if machine == required {
        Ok(())
    } else {
        Err(mismatch(
            ImageFormat::Elf,
            format!("e_machine {machine}"),
            arch,
        ))
    }
}

fn verify_pe<R: Read + Seek>(
    reader: &mut R,
    arch: TargetArch,
) -> Result<(), DriverVerificationError> {
    let dos: [u8; 64] = read_at(reader, 0, ImageFormat::Pe)?;
    if dos[..2] != *b"MZ" {
        return Err(not_an_image(ImageFormat::Pe));
    }
    let offset = u32::from_le_bytes([dos[60], dos[61], dos[62], dos[63]]);
    if offset > PE_MAX_HEADER_OFFSET {
        return Err(not_an_image(ImageFormat::Pe));
    }
    let coff: [u8; 24] = read_at(reader, u64::from(offset), ImageFormat::Pe)?;
    if coff[..4] != *b"PE\0\0" {
        return Err(not_an_image(ImageFormat::Pe));
    }
    let machine = u16::from_le_bytes([coff[4], coff[5]]);
    let characteristics = u16::from_le_bytes([coff[22], coff[23]]);
    if characteristics & PE_FILE_DLL == 0 {
        return Err(unsupported("PE image is not a DLL".into()));
    }
    let required = match arch {
        TargetArch::Aarch64 => PE_MACHINE_ARM64,
        TargetArch::X86_64 => PE_MACHINE_AMD64,
        TargetArch::Other(name) => {
            return Err(unsupported(format!(
                "unsupported host architecture: {name}"
            )))
        }
    };
    if machine == required {
        Ok(())
    } else {
        Err(mismatch(
            ImageFormat::Pe,
            format!("machine {machine:#x}"),
            arch,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn check(format: ImageFormat, bytes: Vec<u8>, arch: TargetArch) -> Result<(), String> {
        format
            .verify(&mut Cursor::new(bytes), arch)
            .map_err(|error| error.to_string())
    }

    fn mach_o(magic: [u8; 4], cpu: [u8; 4]) -> Vec<u8> {
        [magic.as_slice(), cpu.as_slice(), &[0; 24]].concat()
    }

    fn elf(class: u8, kind: u16, machine: u16) -> Vec<u8> {
        let mut bytes = vec![0x7F, b'E', b'L', b'F', class, 1, 1];
        bytes.resize(16, 0);
        bytes.extend(kind.to_le_bytes());
        bytes.extend(machine.to_le_bytes());
        bytes.resize(64, 0);
        bytes
    }

    fn pe(machine: u16, characteristics: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; 0x80];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&0x80u32.to_le_bytes());
        bytes.extend(b"PE\0\0");
        bytes.extend(machine.to_le_bytes());
        bytes.resize(0x80 + 22, 0);
        bytes.extend(characteristics.to_le_bytes());
        bytes
    }

    #[test]
    fn mach_o_thin_fat_and_foreign_cpu() {
        use TargetArch::{Aarch64, X86_64};
        let arm = mach_o([0xCF, 0xFA, 0xED, 0xFE], MACHO_CPU_ARM64.to_le_bytes());
        let intel = mach_o([0xCF, 0xFA, 0xED, 0xFE], MACHO_CPU_X86_64.to_le_bytes());
        let big_endian = mach_o([0xFE, 0xED, 0xFA, 0xCF], MACHO_CPU_ARM64.to_be_bytes());
        let fat = mach_o(FAT_MAGIC, [0, 0, 0, 2]);
        assert!(check(ImageFormat::MachO, arm.clone(), Aarch64).is_ok());
        assert!(check(ImageFormat::MachO, big_endian, Aarch64).is_ok());
        assert!(check(ImageFormat::MachO, intel.clone(), X86_64).is_ok());
        assert!(check(ImageFormat::MachO, arm, X86_64).is_err());
        assert!(check(ImageFormat::MachO, intel, Aarch64).is_err());
        assert!(check(ImageFormat::MachO, fat.clone(), Aarch64).is_ok());
        assert!(check(ImageFormat::MachO, fat, X86_64).is_ok());
        assert!(check(ImageFormat::MachO, vec![0; 32], Aarch64).is_err());
        assert!(check(ImageFormat::MachO, vec![0xCF, 0xFA], Aarch64).is_err());
    }

    #[test]
    fn elf_checks_class_type_and_machine() {
        use TargetArch::{Aarch64, X86_64};
        assert!(check(ImageFormat::Elf, elf(2, 3, 62), X86_64).is_ok());
        assert!(check(ImageFormat::Elf, elf(2, 3, 183), Aarch64).is_ok());
        assert!(check(ImageFormat::Elf, elf(2, 3, 183), X86_64).is_err());
        assert!(
            check(ImageFormat::Elf, elf(2, 3, 3), X86_64).is_err(),
            "i386"
        );
        assert!(
            check(ImageFormat::Elf, elf(1, 3, 62), X86_64).is_err(),
            "ELFCLASS32"
        );
        let executable = check(ImageFormat::Elf, elf(2, 2, 62), X86_64).unwrap_err();
        assert!(executable.contains("shared object"), "{executable}");
        assert!(check(ImageFormat::Elf, vec![0; 64], X86_64).is_err());
        assert!(check(ImageFormat::Elf, elf(2, 3, 62)[..19].to_vec(), X86_64).is_err());
    }

    #[test]
    fn pe_checks_machine_and_dll_flag() {
        use TargetArch::{Aarch64, X86_64};
        assert!(check(ImageFormat::Pe, pe(0x8664, 0x2022), X86_64).is_ok());
        assert!(check(ImageFormat::Pe, pe(0xAA64, 0x2022), Aarch64).is_ok());
        assert!(check(ImageFormat::Pe, pe(0xAA64, 0x2022), X86_64).is_err());
        assert!(
            check(ImageFormat::Pe, pe(0x014C, 0x2022), X86_64).is_err(),
            "i386"
        );
        let exe = check(ImageFormat::Pe, pe(0x8664, 0x0022), X86_64).unwrap_err();
        assert!(exe.contains("not a DLL"), "{exe}");
        assert!(check(ImageFormat::Pe, vec![0; 200], X86_64).is_err());
        assert!(check(ImageFormat::Pe, pe(0x8664, 0x2022)[..0x84].to_vec(), X86_64).is_err());
        let mut far = pe(0x8664, 0x2022);
        far[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(check(ImageFormat::Pe, far, X86_64).is_err());
    }

    #[test]
    fn unknown_host_architecture_is_rejected() {
        let other = TargetArch::Other("riscv64");
        assert!(check(ImageFormat::Elf, elf(2, 3, 62), other).is_err());
        assert!(check(ImageFormat::Pe, pe(0x8664, 0x2000), other).is_err());
    }
}

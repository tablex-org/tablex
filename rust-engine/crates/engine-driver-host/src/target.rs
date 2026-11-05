use std::fmt;

/// Operating system a driver library is built for. Anything the host does not
/// know keeps the name `std::env::consts::OS` reported, so it still prints in
/// "No build for ..." messages and never matches a known artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetOs {
    Macos,
    Linux,
    Windows,
    Other(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetArch {
    Aarch64,
    X86_64,
    Other(&'static str),
}

/// The platform a driver artifact runs on: the unit the manifest selects by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DriverTarget {
    pub os: TargetOs,
    pub arch: TargetArch,
}

impl DriverTarget {
    pub const fn new(os: TargetOs, arch: TargetArch) -> Self {
        Self { os, arch }
    }

    /// The platform this binary runs on.
    pub fn current() -> Self {
        Self::from_consts(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// From `std::env::consts::{OS, ARCH}` style names.
    pub fn from_consts(os: &'static str, arch: &'static str) -> Self {
        Self {
            os: match os {
                "macos" => TargetOs::Macos,
                "linux" => TargetOs::Linux,
                "windows" => TargetOs::Windows,
                other => TargetOs::Other(other),
            },
            arch: match arch {
                "aarch64" => TargetArch::Aarch64,
                "x86_64" => TargetArch::X86_64,
                other => TargetArch::Other(other),
            },
        }
    }

    /// Manifest identifier, e.g. `macos-aarch64`.
    pub fn id(&self) -> String {
        format!("{}-{}", self.os.id(), self.arch.id())
    }

    /// Identifier of the macOS fat build that also satisfies this target, if any.
    pub fn universal_id(&self) -> Option<&'static str> {
        (self.os == TargetOs::Macos).then_some("macos-universal")
    }

    /// `libdbengine_<id>.dylib`, `libdbengine_<id>.so` or `dbengine_<id>.dll`.
    pub fn library_file_name(&self, id: &str) -> String {
        match self.os {
            TargetOs::Windows => format!("dbengine_{id}.dll"),
            _ => format!("libdbengine_{id}.{}", self.os.library_extension()),
        }
    }
}

impl fmt::Display for DriverTarget {
    /// Human label, e.g. `macOS arm64`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let arch = match self.arch {
            TargetArch::Aarch64 => "arm64",
            other => other.id(),
        };
        write!(f, "{} {arch}", self.os.label())
    }
}

impl TargetOs {
    fn id(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Windows => "windows",
            Self::Other(name) => name,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Macos => "macOS",
            Self::Linux => "Linux",
            Self::Windows => "Windows",
            Self::Other(name) => name,
        }
    }

    pub fn library_extension(self) -> &'static str {
        match self {
            Self::Macos => "dylib",
            Self::Windows => "dll",
            Self::Linux | Self::Other(_) => "so",
        }
    }
}

impl TargetArch {
    fn id(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::X86_64 => "x86_64",
            Self::Other(name) => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_labels() {
        let linux = DriverTarget::from_consts("linux", "x86_64");
        assert_eq!(linux.id(), "linux-x86_64");
        assert_eq!(linux.to_string(), "Linux x86_64");
        let mac = DriverTarget::from_consts("macos", "aarch64");
        assert_eq!(mac.id(), "macos-aarch64");
        assert_eq!(mac.to_string(), "macOS arm64");
        assert_eq!(mac.universal_id(), Some("macos-universal"));
        assert_eq!(linux.universal_id(), None);
        let odd = DriverTarget::from_consts("freebsd", "riscv64");
        assert_eq!(odd.id(), "freebsd-riscv64");
        assert_eq!(odd.to_string(), "freebsd riscv64");
    }

    #[test]
    fn library_names_follow_the_os() {
        let name = |os, arch| DriverTarget::from_consts(os, arch).library_file_name("sqlite");
        assert_eq!(name("macos", "aarch64"), "libdbengine_sqlite.dylib");
        assert_eq!(name("linux", "x86_64"), "libdbengine_sqlite.so");
        assert_eq!(name("windows", "aarch64"), "dbengine_sqlite.dll");
    }
}

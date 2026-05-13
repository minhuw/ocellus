use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;

const DEV_MEM_PATH: &str = "/dev/mem";

#[derive(Debug)]
pub struct Mmio {
    base: u64,
    file: File,
}

impl Mmio {
    pub fn open(base: u64) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(DEV_MEM_PATH)
            .map_err(|error| format!("failed to open {DEV_MEM_PATH}: {error}"))?;

        Ok(Self { base, file })
    }

    pub fn read_u64(&self, offset: u64) -> Result<u64, String> {
        self.read_u64_raw(offset)
            .map_err(|error| format!("failed to read MMIO {self} offset 0x{offset:x}: {error}"))
    }

    pub fn write_u32(&self, offset: u64, value: u32) -> Result<(), String> {
        self.write_u32_raw(offset, value).map_err(|error| {
            format!("failed to write MMIO {self} offset 0x{offset:x} value 0x{value:x}: {error}")
        })
    }

    fn read_u64_raw(&self, offset: u64) -> io::Result<u64> {
        let mut bytes = [0_u8; 8];
        self.file.read_exact_at(&mut bytes, self.base + offset)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn write_u32_raw(&self, offset: u64, value: u32) -> io::Result<()> {
        self.file
            .write_all_at(&value.to_le_bytes(), self.base + offset)
    }
}

impl fmt::Display for Mmio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "base 0x{:x}", self.base)
    }
}

#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

#[derive(Debug)]
pub struct Msr {
    file: File,
}

impl Msr {
    pub fn open(cpu: u32) -> io::Result<Self> {
        let path = msr_path(cpu);
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self { file })
    }

    pub fn open_readonly(cpu: u32) -> io::Result<Self> {
        let path = msr_path(cpu);
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(Self { file })
    }

    pub fn read(&self, address: u64) -> io::Result<u64> {
        let mut bytes = [0_u8; 8];
        self.file.read_exact_at(&mut bytes, address)?;
        Ok(u64::from_le_bytes(bytes))
    }

    pub fn write(&self, address: u64, value: u64) -> io::Result<()> {
        self.file.write_all_at(&value.to_le_bytes(), address)
    }
}

pub fn read(cpu: u32, address: u64) -> io::Result<u64> {
    Msr::open_readonly(cpu)?.read(address)
}

pub fn write(cpu: u32, address: u64, value: u64) -> io::Result<()> {
    Msr::open(cpu)?.write(address, value)
}

fn msr_path(cpu: u32) -> PathBuf {
    PathBuf::from(format!("/dev/cpu/{cpu}/msr"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_msr_path() {
        assert_eq!(msr_path(7), PathBuf::from("/dev/cpu/7/msr"));
    }

    #[test]
    #[ignore = "requires x86_64 Linux with the msr kernel module loaded and permission to read /dev/cpu/0/msr"]
    fn reads_actual_tsc_msr() {
        const IA32_TIME_STAMP_COUNTER: u64 = 0x10;

        let first = read(0, IA32_TIME_STAMP_COUNTER).expect("read IA32_TSC MSR first time");
        let second = read(0, IA32_TIME_STAMP_COUNTER).expect("read IA32_TSC MSR second time");

        assert!(second >= first);
    }
}

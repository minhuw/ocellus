use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

#[derive(Debug)]
pub struct Msr {
    cpu: u32,
    file: File,
}

impl Msr {
    pub fn open_readonly(cpu: u32) -> Result<Self, String> {
        Self::open_readonly_raw(cpu)
            .map_err(|error| format!("failed to open /dev/cpu/{cpu}/msr for MSR reads: {error}"))
    }

    pub fn open(cpu: u32) -> Result<Self, String> {
        Self::open_raw(cpu)
            .map_err(|error| format!("failed to open /dev/cpu/{cpu}/msr for MSR writes: {error}"))
    }

    fn open_raw(cpu: u32) -> io::Result<Self> {
        let path = msr_path(cpu);
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self { cpu, file })
    }

    fn open_readonly_raw(cpu: u32) -> io::Result<Self> {
        let path = msr_path(cpu);
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(Self { cpu, file })
    }

    pub fn read(&self, address: u64) -> Result<u64, String> {
        self.read_raw(address).map_err(|error| {
            format!(
                "failed to read MSR 0x{address:x} from /dev/cpu/{}/msr: {error}",
                self.cpu
            )
        })
    }

    pub fn write(&self, address: u64, value: u64) -> Result<(), String> {
        self.write_raw(address, value).map_err(|error| {
            format!(
                "failed to write MSR 0x{address:x} on /dev/cpu/{}/msr: {error}",
                self.cpu
            )
        })
    }

    fn read_raw(&self, address: u64) -> io::Result<u64> {
        let mut bytes = [0_u8; 8];
        self.file.read_exact_at(&mut bytes, address)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn write_raw(&self, address: u64, value: u64) -> io::Result<()> {
        self.file.write_all_at(&value.to_le_bytes(), address)
    }
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

        let msr = Msr::open_readonly(0).expect("open /dev/cpu/0/msr");
        let first = msr
            .read(IA32_TIME_STAMP_COUNTER)
            .expect("read IA32_TSC MSR first time");
        let second = msr
            .read(IA32_TIME_STAMP_COUNTER)
            .expect("read IA32_TSC MSR second time");

        assert!(second >= first);
    }
}

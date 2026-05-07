use raw_cpuid::{CpuId, CpuIdReaderNative};

#[derive(Clone, Debug)]
pub struct CpuInfo {
    cpuid: CpuId<CpuIdReaderNative>,
}

impl Default for CpuInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuInfo {
    pub fn new() -> Self {
        Self {
            cpuid: CpuId::new(),
        }
    }

    pub fn brand(&self) -> Option<String> {
        self.cpuid
            .get_processor_brand_string()
            .map(|brand| brand.as_str().trim().to_owned())
            .filter(|brand| !brand.is_empty())
    }

    pub fn vendor(&self) -> Option<String> {
        self.cpuid
            .get_vendor_info()
            .map(|vendor| vendor.as_str().to_owned())
    }

    pub fn has_tsc(&self) -> bool {
        self.cpuid
            .get_feature_info()
            .is_some_and(|features| features.has_tsc())
    }

    pub fn family_id(&self) -> Option<u8> {
        self.cpuid
            .get_feature_info()
            .map(|features| features.family_id())
    }

    pub fn has_invariant_tsc(&self) -> bool {
        self.cpuid
            .get_advanced_power_mgmt_info()
            .is_some_and(|features| features.has_invariant_tsc())
    }

    pub fn model_id(&self) -> Option<u8> {
        self.cpuid
            .get_feature_info()
            .map(|features| features.model_id())
    }
}

pub fn has_tsc() -> bool {
    CpuInfo::new().has_tsc()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpuid_can_be_queried() {
        let cpu = CpuInfo::new();

        assert!(cpu.vendor().is_some());
    }
}

use crate::metal::cpuid::CpuInfo;

const INTEL_VENDOR_ID: &str = "GenuineIntel";

#[derive(Clone, Copy, Debug, Default)]
pub struct ArchitectureFeatures {
    pub invariant_tsc: bool,
    pub package_rapl: bool,
    pub tsc: bool,
}

#[derive(Clone, Debug)]
pub struct Architecture {
    pub brand: String,
    pub family: u8,
    pub features: ArchitectureFeatures,
    pub model: u8,
    pub vendor: String,
}

impl Architecture {
    pub fn detect() -> Result<Self, String> {
        Self::from_cpu_info(&CpuInfo::new())
    }

    pub fn intel_server_model(&self) -> IntelServerCpuModel {
        IntelServerCpuModel::from_architecture(self)
            .expect("Architecture is validated before runtime starts")
    }

    pub fn validate(&self) -> Result<(), String> {
        if IntelServerCpuModel::from_architecture(self).is_some() {
            return Ok(());
        }

        Err(format!(
            "unsupported processor: ocellus currently supports Intel server processors only; detected vendor={}, family={}, model={}, brand={}",
            self.vendor, self.family, self.model, self.brand
        ))
    }

    fn from_cpu_info(cpu: &CpuInfo) -> Result<Self, String> {
        let brand = cpu
            .brand()
            .ok_or_else(|| "failed to read CPUID processor brand".to_string())?;
        let vendor = cpu
            .vendor()
            .ok_or_else(|| "failed to read CPUID vendor".to_string())?;
        let family = cpu
            .family_id()
            .ok_or_else(|| "failed to read CPUID family".to_string())?;
        let model = cpu
            .model_id()
            .ok_or_else(|| "failed to read CPUID model".to_string())?;
        let features = ArchitectureFeatures {
            invariant_tsc: cpu.has_invariant_tsc(),
            package_rapl: IntelServerCpuModel::from_parts(&vendor, family, model).is_some(),
            tsc: cpu.has_tsc(),
        };

        let architecture = Self {
            brand,
            family,
            features,
            model,
            vendor,
        };
        architecture.validate()?;

        Ok(architecture)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntelServerCpuModel {
    BroadwellDe,
    BroadwellXeon,
    EmeraldRapids,
    HaswellXeon,
    IceLakeXeon,
    IvyTown,
    KnightsLanding,
    SandyBridgeEp,
    SapphireRapids,
    SkylakeXeon,
}

impl IntelServerCpuModel {
    fn from_architecture(architecture: &Architecture) -> Option<Self> {
        Self::from_parts(
            &architecture.vendor,
            architecture.family,
            architecture.model,
        )
    }

    fn from_parts(vendor: &str, family: u8, model: u8) -> Option<Self> {
        if vendor != INTEL_VENDOR_ID {
            return None;
        }

        Self::from_family_model(family, model)
    }

    pub fn from_family_model(family: u8, model: u8) -> Option<Self> {
        if family != 6 {
            return None;
        }

        match model {
            0x2d => Some(Self::SandyBridgeEp),
            0x3e => Some(Self::IvyTown),
            0x3f => Some(Self::HaswellXeon),
            0x4f => Some(Self::BroadwellXeon),
            0x55 => Some(Self::SkylakeXeon),
            0x56 => Some(Self::BroadwellDe),
            0x57 => Some(Self::KnightsLanding),
            0x6a => Some(Self::IceLakeXeon),
            0x85 => Some(Self::KnightsLanding),
            0x8f => Some(Self::SapphireRapids),
            0xcf => Some(Self::EmeraldRapids),
            _ => None,
        }
    }

    pub fn has_fixed_dram_energy_unit(self) -> bool {
        matches!(
            self,
            Self::HaswellXeon
                | Self::BroadwellXeon
                | Self::KnightsLanding
                | Self::SkylakeXeon
                | Self::IceLakeXeon
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_intel_server_vendor_and_model() {
        assert!(IntelServerCpuModel::from_parts(INTEL_VENDOR_ID, 6, 0x55).is_some());
        assert!(IntelServerCpuModel::from_parts(INTEL_VENDOR_ID, 6, 0x97).is_none());
        assert!(IntelServerCpuModel::from_parts("AuthenticAMD", 0x17, 0x1).is_none());
        assert!(IntelServerCpuModel::from_parts("KVMKVMKVM", 6, 0x55).is_none());
    }

    #[test]
    fn maps_known_intel_models() {
        assert_eq!(
            IntelServerCpuModel::from_family_model(6, 0x55),
            Some(IntelServerCpuModel::SkylakeXeon)
        );
        assert_eq!(
            IntelServerCpuModel::from_family_model(6, 0x4f),
            Some(IntelServerCpuModel::BroadwellXeon)
        );
        assert_eq!(
            IntelServerCpuModel::from_family_model(6, 0x8f),
            Some(IntelServerCpuModel::SapphireRapids)
        );
        assert_eq!(
            IntelServerCpuModel::from_family_model(6, 0x6a),
            Some(IntelServerCpuModel::IceLakeXeon)
        );
        assert_eq!(IntelServerCpuModel::from_family_model(6, 0x6c), None);
        assert_eq!(
            IntelServerCpuModel::from_family_model(6, 0xcf),
            Some(IntelServerCpuModel::EmeraldRapids)
        );
        assert_eq!(IntelServerCpuModel::from_family_model(6, 0x97), None);
        assert_eq!(IntelServerCpuModel::from_family_model(6, 0), None);
    }

    #[test]
    fn broadwell_xeon_uses_fixed_dram_energy_unit() {
        assert!(IntelServerCpuModel::BroadwellXeon.has_fixed_dram_energy_unit());
    }

    #[test]
    fn broadwell_de_uses_msr_dram_energy_unit() {
        assert!(!IntelServerCpuModel::BroadwellDe.has_fixed_dram_energy_unit());
    }
}

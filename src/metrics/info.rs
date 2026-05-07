use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::metrics::ProcessorMetadata;

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct OcellusInfoLabels {
    version: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct ProcessorInfoLabels {
    brand: String,
    family: String,
    invariant_tsc_supported: String,
    model: String,
    package_rapl_supported: String,
    vendor: String,
}

pub fn register(registry: &mut Registry, processor: ProcessorMetadata) {
    let ocellus_info = Family::<OcellusInfoLabels, Gauge>::default();
    let processor_info = Family::<ProcessorInfoLabels, Gauge>::default();

    registry.register(
        "ocellus_info",
        "Static metadata for the ocellus exporter",
        ocellus_info.clone(),
    );
    registry.register(
        "processor_info",
        "Static processor metadata and capabilities",
        processor_info.clone(),
    );

    ocellus_info
        .get_or_create(&OcellusInfoLabels {
            version: env!("CARGO_PKG_VERSION").to_string(),
        })
        .set(1);
    processor_info
        .get_or_create(&ProcessorInfoLabels {
            brand: processor.brand,
            family: processor.family.to_string(),
            invariant_tsc_supported: processor.invariant_tsc_supported.to_string(),
            model: processor.model.to_string(),
            package_rapl_supported: processor.package_rapl_supported.to_string(),
            vendor: processor.vendor,
        })
        .set(1);
}

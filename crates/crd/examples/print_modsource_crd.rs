//! Prints the `ModSource` CRD manifest as YAML, generated straight from
//! the Rust type -- run via `cargo run -p crd --example print_modsource_crd`
//! and redirect into `charts/magpie/crds/modsources.yaml` whenever
//! `ModSourceSpec`/`ModSourceStatus` change, so the chart's CRD manifest
//! can never drift from what registry/sync-daemon actually read/write.

use kube::CustomResourceExt;

fn main() {
    let crd = crd::ModSource::crd();
    print!("{}", serde_yaml::to_string(&crd).expect("CRD serializes to YAML"));
}

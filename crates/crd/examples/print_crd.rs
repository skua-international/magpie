//! Prints the `ArmaServer` CRD manifest as YAML, generated straight from
//! the Rust type -- run via `cargo run -p crd --example print_crd` and
//! redirect into `charts/magpie/crds/armaservers.yaml` whenever
//! `ArmaServerSpec`/`ArmaServerStatus` change, so the chart's CRD manifest
//! can never drift from what the controller actually reads/writes.

use kube::CustomResourceExt;

fn main() {
    let crd = crd::ArmaServer::crd();
    print!("{}", serde_yaml::to_string(&crd).expect("CRD serializes to YAML"));
}

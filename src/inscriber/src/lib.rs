#[allow(unused)]
mod build_data;
pub mod canister;
mod constants;
pub mod inscription;
pub mod types;
mod utils;
pub mod wallet;

use ic_metrics::Metrics;

pub use crate::canister::Inscriber;

pub fn idl() -> String {
    let inscriber_idl = Inscriber::idl();
    let mut metrics_idl = <Inscriber as Metrics>::get_idl();
    metrics_idl.merge(&inscriber_idl);

    candid::pretty::candid::compile(&metrics_idl.env.env, &Some(metrics_idl.actor))
}

//! Throwaway diagnostic: list every cpal output device and the configs it
//! advertises. Run with `cargo run --example devices`. Not committed.

use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    let host = cpal::default_host();
    let default = host
        .default_output_device()
        .and_then(|d| d.description().ok())
        .map(|d| d.name().to_string())
        .unwrap_or_default();
    println!("default output: {default}\n");
    for device in host.output_devices().expect("enumerate") {
        let name = device
            .description()
            .map_or_else(|_| "<unnamed>".into(), |d| d.name().to_string());
        println!("== {name}");
        let Ok(configs) = device.supported_output_configs() else {
            println!("   (configs unreadable)");
            continue;
        };
        for cfg in configs {
            println!(
                "   {}ch  {}..{} Hz  {:?}",
                cfg.channels(),
                cfg.min_sample_rate(),
                cfg.max_sample_rate(),
                cfg.sample_format(),
            );
        }
    }
}

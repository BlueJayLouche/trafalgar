// nih-plug's standalone defaults to a 48kHz sample rate, and its CoreAudio backend
// panics outright if the audio device runs at a different rate (e.g. 44.1kHz
// speakers). So unless the user pinned a rate, default to the output device's
// actual rate — otherwise the app crashes on launch on any non-48kHz device.

use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--sample-rate" || a == "-r") {
        if let Some(sr) = default_output_sample_rate() {
            args.push("--sample-rate".into());
            args.push(sr.to_string());
        }
    }
    nih_plug::prelude::nih_export_standalone_with_args::<trafalgar::Trafalgar, _>(args);
}

fn default_output_sample_rate() -> Option<u32> {
    let device = cpal::default_host().default_output_device()?;
    Some(device.default_output_config().ok()?.sample_rate().0)
}

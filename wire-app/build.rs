use std::path::Path;

fn main() {
    // These are polish assets, not application requirements. Expose each file's
    // availability as a cfg so include_bytes! is only expanded for files that
    // actually exist in the checkout.
    let optional_assets = [
        (
            "wire_has_font_fraktion_sans",
            "fonts/PPFraktionSans-Light.otf",
        ),
        (
            "wire_has_font_fraktion_mono",
            "fonts/PPFraktionMono-Regular.otf",
        ),
        ("wire_has_font_kh_interference", "fonts/Kh-Interference.otf"),
        ("wire_has_sound_whoosh_1", "sound-kit/whoosh-1.wav"),
        ("wire_has_sound_whoosh_2", "sound-kit/whoosh-2.wav"),
        ("wire_has_sound_button_1", "sound-kit/button-1.wav"),
        ("wire_has_sound_button_2", "sound-kit/button-2.wav"),
        ("wire_has_sound_success", "sound-kit/success.wav"),
        ("wire_has_sound_fail", "sound-kit/fail.wav"),
        (
            "wire_has_sound_notification_pop",
            "sound-kit/notification-pop.wav",
        ),
        (
            "wire_has_sound_incoming_ring",
            "sound-kit/atmostphere-2.wav",
        ),
    ];

    for (cfg, path) in optional_assets {
        println!("cargo:rustc-check-cfg=cfg({cfg})");
        println!("cargo:rerun-if-changed={path}");
        if Path::new(path).is_file() {
            println!("cargo:rustc-cfg={cfg}");
        }
    }
}

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party");
    let lib = root.join("tensorflow/lite/experimental/microfrontend/lib");
    let files = [
        "fft.cc",
        "filterbank.cc",
        "frontend.cc",
        "frontend_util.cc",
        "log_scale.cc",
        "log_lut.cc",
        "noise_reduction.cc",
        "pcan_gain_control.cc",
        "window.cc",
        "fft_util.cc",
        "filterbank_util.cc",
        "log_scale_util.cc",
        "noise_reduction_util.cc",
        "pcan_gain_control_util.cc",
        "window_util.cc",
        "kiss_fft_int16.cc",
    ];
    let mut build = cc::Build::new();
    build.cpp(true).include(&root).include(root.join("kissfft")).std("c++11");
    for f in files {
        build.file(lib.join(f));
    }
    build.file(root.join("kissfft/kiss_fft.cc"));
    build.file(root.join("kissfft/tools/kiss_fftr.cc"));
    build.file("mf_wrapper.cc");
    build.compile("tflm_microfrontend");
    println!("cargo:rerun-if-changed=mf_wrapper.cc");
    println!("cargo:rerun-if-changed={}", lib.display());
}

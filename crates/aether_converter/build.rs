// The `bc6h` feature pulls in image_dds → intel_tex_2, whose prebuilt Linux
// archive (libispc_texcomp_astc…) contains C++ objects that need the C++
// runtime (__gxx_personality_v0). intel_tex_2 does not link it itself, and
// rustc links with -nodefaultlibs, so on linux-gnu the binary fails to link
// unless libstdc++ is named explicitly. macOS and Windows resolve it through
// their system toolchains.
//
// rustc-link-lib alone is not enough: Cargo hands it to the library target
// only, and the aether_converter binary compiles its modules itself instead of
// linking that library. rustc-link-arg reaches every linked target (bin,
// tests, examples); the link-lib keeps downstream users of the library linked.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let bc6h = std::env::var_os("CARGO_FEATURE_BC6H").is_some();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if bc6h && os == "linux" && env == "gnu" {
        println!("cargo:rustc-link-lib=dylib=stdc++");
        println!("cargo:rustc-link-arg=-lstdc++");
    }
}

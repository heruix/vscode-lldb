extern crate cpp_build;

fn main() {
    cpp_build::Config::new().include("include").build("src/lldb.rs");

    #[cfg(os = "macos")]
    {
        println!(
            "cargo:rustc-link-search=framework={}",
            "/Library/Developer/CommandLineTools/Library/PrivateFrameworks"
        );
        println!("cargo:rustc-link-lib=framework={}", "LLDB");
    }
}

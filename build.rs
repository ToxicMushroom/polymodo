fn main() {
    let config = slint_build::CompilerConfiguration::default()
        .with_style("fluent".into())
        // the UI tests (tests/ui.rs) need debug info to find elements;
        // only emit it in debug builds, which tests are.
        .with_debug_info(std::env::var("PROFILE").is_ok_and(|p| p == "debug"));

    slint_build::compile_with_config("ui/ui.slint", config).expect("Slint build failed");
}

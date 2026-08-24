fn main() {
    println!("cargo:rerun-if-changed=assets");

    #[cfg(target_os = "windows")]
    {
        let mut resource = winres::WindowsResource::new();
        resource
            .set_icon("assets/schedule-logo.ico")
            .set("ProductName", "Schedule Manager")
            .set("FileDescription", "Schedule Manager")
            .set("LegalCopyright", "Copyright Emssion");
        resource
            .compile()
            .expect("compile Windows application icon");
    }
    if std::env::var_os("CARGO_FEATURE_DESKTOP").is_some() {
        slint_build::compile("ui/main.slint").expect("compile Slint UI");
    }
}

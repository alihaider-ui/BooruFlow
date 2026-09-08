#[cfg(target_os = "windows")]
fn main() {
    // The library's historic name is `core`; alias it to avoid std::core ambiguity.
    extern crate core as booruflow_core;
    booruflow_core::web_auth_spike::run();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("The web authentication compatibility spike currently supports Windows only.");
    std::process::exit(1);
}

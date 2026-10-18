#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = prek::route_worker::native_fixture::entry() {
        eprintln!("managed native fixture failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("managed native fixture requires Linux; skipped");
}

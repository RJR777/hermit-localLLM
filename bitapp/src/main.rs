#[cfg(target_os = "hermit")]
mod hermit_app;

#[cfg(not(target_os = "hermit"))]
fn main() {
    println!("bitapp: host build stub; run on Hermit target");
}

#[cfg(target_os = "hermit")]
fn main() {
    hermit_app::run();
}

use std::process::Command;

fn main() {
    let status = Command::new("sh")
        .args(["scripts/build-app.sh"])
        .status()
        .expect("failed to run scripts/build-app.sh");

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

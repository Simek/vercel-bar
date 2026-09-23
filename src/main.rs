#![allow(unexpected_cfgs)]

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod settings;
mod vercel;

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
}

#![allow(unexpected_cfgs)]

#[cfg(target_os = "macos")]
mod macos;
mod vercel;

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
}

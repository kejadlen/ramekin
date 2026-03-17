use std::process::Command;

fn main() {
    let version = std::env::var("RAMEKIN_VERSION").unwrap_or_else(|_| {
        let commit = cmd("git", &["rev-parse", "--short", "HEAD"]);
        format!("dev+{commit}")
    });
    println!("cargo:rustc-env=RAMEKIN_VERSION={version}");
}

fn cmd(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

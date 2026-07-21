use std::{fs, path::Path};

fn main() {
    let stamp_path = Path::new("BUILD");
    let build = if stamp_path.exists() {
        let s = fs::read_to_string(stamp_path).unwrap_or_default();
        s.trim().parse::<u64>().unwrap_or(0) + 1
    } else {
        1
    };
    fs::write(stamp_path, build.to_string()).expect("failed to write BUILD stamp");
    println!("cargo:rustc-env=BUILD_NUMBER={}", build);
}

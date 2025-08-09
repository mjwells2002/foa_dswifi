use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path};



fn visit_dir(dir: &Path, base: &Path, files: &mut Vec<(String, String, String)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();

        if path.is_dir() {
            visit_dir(&path, base, files);
        } else if path.is_file() {
            let rel_path = path.strip_prefix(base).unwrap().to_str().unwrap();

            let var_name = rel_path
                .replace("/", "_")
                .replace("\\", "_")
                .replace(".", "_")
                .replace("-", "_")
                .replace(" ", "_")
                .to_uppercase();

            let buf = fs::canonicalize(path.clone()).unwrap();
            let path = buf.to_str().unwrap();

            files.push((rel_path.replace("\\", "/"), var_name, path.parse().unwrap()));
        }
    }
}

fn main() {
    println!("cargo:rustc-link-arg-bins=-Tlinkall.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
    println!("cargo:rerun-if-changed=webapp");
    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("embedded.rs");
    let mut f = File::create(&dest_path).unwrap();

    let mut files = Vec::new();
    let base = Path::new("webapp");
    visit_dir(base, base, &mut files);

    for (_, var_name, full_path) in &files {
        writeln!(
            f,
            "pub static {}: &[u8] = include_bytes!(\"{}\");",
            var_name, full_path
        ).unwrap();
    }

    writeln!(f, "pub static FILES: &[(&str, &[u8])] = &[").unwrap();
    for (rel_path, var_name, _) in &files {
        writeln!(f, "    (\"{}\", {}),", rel_path, var_name).unwrap();
    }
    writeln!(f, "];").unwrap();

}


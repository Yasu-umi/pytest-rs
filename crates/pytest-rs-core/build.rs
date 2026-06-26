use std::path::Path;

/// Embed every .py file under py/ into a generated SHIM_FILES manifest so
/// new shim modules are picked up without touching Rust code.
/// Also generates SHIM_CONTENT_HASH: a build-time FNV-1a hash of all shim
/// file contents, used to name a persistent cache directory that survives
/// across process invocations (avoiding 200+ file writes on every startup).
fn main() {
    println!("cargo:rerun-if-changed=py");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let py_root = Path::new(&manifest_dir).join("py");

    let mut files = Vec::new();
    collect(&py_root, &py_root, &mut files);
    files.sort();

    // FNV-1a over all file contents (sorted for determinism).
    let mut hash: u64 = 14695981039346656037; // FNV offset basis
    for rel in &files {
        for b in std::fs::read(py_root.join(rel)).expect("read shim file") {
            hash ^= b as u64;
            hash = hash.wrapping_mul(1099511628211); // FNV prime
        }
    }

    let mut out = String::from("pub const SHIM_FILES: &[(&str, &str)] = &[\n");
    for rel in &files {
        out.push_str(&format!(
            "    ({rel:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/py/\", {rel:?}))),\n"
        ));
    }
    out.push_str("];\n");
    out.push_str(&format!(
        "pub const SHIM_CONTENT_HASH: u64 = 0x{hash:016x};\n"
    ));

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    std::fs::write(Path::new(&out_dir).join("shim_manifest.rs"), out).expect("write manifest");
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read py dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect(root, &path, files);
        } else if path.extension().is_some_and(|ext| ext == "py") {
            let rel = path
                .strip_prefix(root)
                .expect("under py root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            files.push(rel);
        }
    }
}

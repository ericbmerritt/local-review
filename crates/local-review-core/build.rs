use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn main() {
    // Re-run this script whenever any extraction source file changes.
    println!("cargo:rerun-if-changed=src/semantic");

    // Hash all .rs files under src/semantic/ in deterministic (sorted) order.
    // Any change to extraction logic — plugins, differ, identity, Container Rule —
    // produces a different hash, automatically invalidating caches built by the
    // old binary without requiring a manual SCHEMA_VERSION bump.
    let dir = Path::new("src/semantic");
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    collect_rs_files(dir, &mut files);

    let mut hasher = blake3::Hasher::new();
    for (path, content) in &files {
        hasher.update(path.as_bytes());
        hasher.update(content);
    }
    let hash = hasher.finalize();
    // First 8 bytes as a lowercase hex string — compact, stable, unambiguous.
    let mut hex = String::with_capacity(16);
    for b in &hash.as_bytes()[..8] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }

    println!("cargo:rustc-env=SEMANTIC_EXTRACTION_HASH={hex}");
}

fn collect_rs_files(dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            if let Ok(content) = fs::read(&path) {
                out.insert(path.to_string_lossy().into_owned(), content);
            }
        }
    }
}

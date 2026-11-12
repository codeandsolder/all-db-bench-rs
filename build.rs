use std::{env, error::Error, fmt::Write as _, fs, path::PathBuf};

const ENGINE_DEPENDENCIES: &[(&str, &str, Option<&str>)] = &[
    ("REDB", "redb", None),
    ("FJALL", "fjall", None),
    ("SURREALKV", "surrealkv", None),
    ("HEED", "heed", None),
    ("SLED", "sled", None),
    ("LKV", "lkv", None),
    ("MANIFOLD", "manifold-db", None),
    ("TURBOKV", "turbokv", None),
    ("PARITYDB", "parity-db", None),
    ("ROCKSDB", "rocksdb", Some("kv-external")),
    ("MDBX", "libmdbx", Some("kv-external")),
    ("PERSY", "persy", Some("kv-external")),
    ("ROUGHDB", "roughdb", Some("kv-experimental")),
    ("JAMMDB", "jammdb", Some("kv-experimental")),
    ("LSMDB", "lsm-db", Some("kv-experimental")),
];

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=Cargo.toml");
    let manifest: toml_edit::DocumentMut = fs::read_to_string("Cargo.toml")?.parse()?;
    let deps = &manifest["dependencies"];
    let mut generated =
        String::from("// Derived from exact engine dependency pins in Cargo.toml.\n");
    for (constant, dependency, feature) in ENGINE_DEPENDENCIES {
        let entry = &deps[dependency];
        let requirement = entry
            .as_str()
            .or_else(|| entry.get("version").and_then(toml_edit::Item::as_str))
            .ok_or_else(|| format!("{dependency} has no direct manifest version"))?;
        let version = requirement
            .strip_prefix('=')
            .filter(|version| !version.is_empty())
            .ok_or_else(|| {
                format!("{dependency} must use an exact version pin, found {requirement}")
            })?;
        if let Some(feature) = feature {
            writeln!(generated, "#[cfg(feature = {feature:?})]")?;
        }
        writeln!(generated, "pub const {constant}: &str = {version:?};")?;
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?);
    fs::write(out_dir.join("engine_versions.rs"), generated)?;
    Ok(())
}

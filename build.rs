use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let package_root = manifest_dir.join("builtin-plugins");
    println!("cargo:rerun-if-changed={}", package_root.display());

    let mut packages = Vec::new();
    if package_root.is_dir() {
        for entry in fs::read_dir(&package_root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                packages.push(path);
            }
        }
    }
    packages.sort();

    let mut names = HashSet::new();
    let mut generated = String::from("pub static PLUGINS: &[EmbeddedPlugin] = &[\n");
    for package in packages {
        let manifest_path = package.join("plugin.json");
        let component_path = package.join("plugin.wasm");
        let manifest = fs::read_to_string(&manifest_path).map_err(|error| {
            format!(
                "failed to read bundled plugin manifest `{}`: {error}",
                manifest_path.display()
            )
        })?;
        let manifest_value: Value = serde_json::from_str(&manifest).map_err(|error| {
            format!(
                "bundled plugin manifest `{}` is invalid: {error}",
                manifest_path.display()
            )
        })?;
        let name = manifest_value
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("bundled plugin `{}` has no string name", package.display()))?;
        let version = manifest_value
            .get("version")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("bundled plugin `{name}` has no string version"))?;
        if !valid_segment(name) || !valid_segment(version) {
            return Err(
                format!("bundled plugin `{name}` has an invalid name or version segment").into(),
            );
        }
        if !names.insert(name.to_string()) {
            return Err(format!("bundled plugin name `{name}` is declared more than once").into());
        }
        let runtime = manifest_value
            .get("runtime")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("bundled plugin `{name}` has no runtime object"))?;
        if runtime.get("type").and_then(Value::as_str) != Some("wasm")
            || runtime.get("module").and_then(Value::as_str) != Some("plugin.wasm")
        {
            return Err(format!(
                "bundled plugin `{name}` must declare a Wasmtime runtime using `plugin.wasm`"
            )
            .into());
        }
        if !component_path.is_file() {
            return Err(format!(
                "bundled plugin `{name}` is missing `{}`",
                component_path.display()
            )
            .into());
        }

        println!("cargo:rerun-if-changed={}", manifest_path.display());
        println!("cargo:rerun-if-changed={}", component_path.display());
        let mut entries = fs::read_dir(&package)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            if path == manifest_path || path == component_path {
                continue;
            }
            let relative = path
                .strip_prefix(&package)
                .map_err(|error| format!("invalid bundled plugin path: {error}"))?
                .to_string_lossy()
                .replace('\\', "/");
            if !path.is_file() {
                return Err(format!(
                    "bundled plugin `{name}` contains `{relative}`; only `plugin.json` \
                     and `plugin.wasm` are allowed"
                )
                .into());
            }
        }

        generated.push_str("    EmbeddedPlugin {\n");
        generated.push_str(&format!("        name: {},\n", rust_literal(name)));
        generated.push_str(&format!("        version: {},\n", rust_literal(version)));
        generated.push_str(&format!(
            "        manifest: include_str!({}),\n",
            rust_literal(&manifest_path.to_string_lossy())
        ));
        generated.push_str(&format!(
            "        component: include_bytes!({}),\n",
            rust_literal(&component_path.to_string_lossy())
        ));
        generated.push_str("    },\n");
    }
    generated.push_str("];\n");

    let output = PathBuf::from(env::var("OUT_DIR")?).join("bundled_plugins.rs");
    fs::write(output, generated)?;
    Ok(())
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn rust_literal(value: &str) -> String {
    format!("{value:?}")
}

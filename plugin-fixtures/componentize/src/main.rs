use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use wit_component::ComponentEncoder;

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let input = PathBuf::from(args.next().context("missing core module path")?);
    let output = PathBuf::from(args.next().context("missing component output path")?);
    let _wit = args.next().context("missing WIT path")?;
    let module = fs::read(&input).with_context(|| format!("read {}", input.display()))?;

    let component = ComponentEncoder::default()
        .module(&module)?
        .validate(true)
        .encode()?;
    fs::write(&output, component).with_context(|| format!("write {}", output.display()))?;
    Ok(())
}

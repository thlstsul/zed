use crate::Oid;
use anyhow::{anyhow, Result};
use collections::HashMap;
use command::blocking::Command;
use std::path::Path;

pub fn get_messages(working_directory: &Path, shas: &[Oid]) -> Result<HashMap<Oid, String>> {
    if shas.is_empty() {
        return Ok(HashMap::default());
    }

    const MARKER: &str = "<MARKER>";

    let mut command = Command::new("git");

    command
        .current_dir(working_directory)
        .arg("show")
        .arg("-s")
        .arg(format!("--format=%B{}", MARKER))
        .args(shas.iter().map(ToString::to_string));

    let output = command
        .output()
        .map_err(|e| anyhow!("Failed to start git blame process: {}", e))?;

    anyhow::ensure!(
        output.status.success(),
        "'git show' failed with error {:?}",
        output.status
    );

    Ok(shas
        .iter()
        .cloned()
        .zip(
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .split_terminator(MARKER)
                .map(|str| str.trim().replace("<", "&lt;").replace(">", "&gt;")),
        )
        .collect::<HashMap<Oid, String>>())
}

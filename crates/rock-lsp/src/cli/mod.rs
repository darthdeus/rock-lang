pub mod bone_lsp;
pub mod inline_lua_census;

use anyhow::{anyhow, Result};

/// Parse a position string in format "file:line:column" (1-indexed)
pub fn parse_position(position: &str) -> Result<(String, u32, u32)> {
    let parts: Vec<&str> = position.rsplitn(3, ':').collect();
    if parts.len() != 3 {
        return Err(anyhow!(
            "Invalid position format. Expected 'file:line:column'"
        ));
    }

    let column: u32 = parts[0]
        .parse()
        .map_err(|_| anyhow!("Invalid column number: {}", parts[0]))?;
    let line: u32 = parts[1]
        .parse()
        .map_err(|_| anyhow!("Invalid line number: {}", parts[1]))?;
    let file = parts[2].to_string();

    if line == 0 || column == 0 {
        return Err(anyhow!(
            "Line and column numbers must be 1-indexed (got line={}, column={})",
            line,
            column
        ));
    }

    Ok((file, line, column))
}

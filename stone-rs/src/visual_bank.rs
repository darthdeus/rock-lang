use rock_visual_bank::{parse_line, resolve, sheet_files, SheetGeometry};
use serde_json::{json, Value};
use std::ffi::c_char;

unsafe fn text<'a>(data: *const u8, len: usize) -> Result<&'a str, String> {
    if data.is_null() {
        return Err("missing input".into());
    }
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(data, len) }).map_err(|e| e.to_string())
}

fn reply(value: Value) -> *const c_char {
    crate::native_ledger::watcher_reply(value.to_string())
}

/// Input is length-delimited; embedded NULs are parsed rather than truncating a bank.
#[no_mangle]
pub unsafe extern "C" fn stone_visual_bank_decode(data: *const u8, len: usize) -> *const c_char {
    let raw = match unsafe { text(data, len) } {
        Ok(s) => s,
        Err(e) => return reply(json!({"problems": [e]})),
    };
    let mut problems = Vec::new();
    if !raw.is_empty() && !raw.ends_with('\n') {
        problems.push("does not end with a newline".to_owned());
    }
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    for (index, raw) in raw.lines().enumerate() {
        if raw.is_empty() {
            continue;
        }
        match parse_line(raw) {
            Ok(line) => {
                rows.push(json!({"raw": raw, "files": sheet_files(&line)}));
                lines.push(line);
            }
            Err(e) => problems.push(format!(":{}: {e:#}", index + 1)),
        }
    }
    lines.sort_by(|a, b| a.key().cmp(&b.key()));
    problems.extend(resolve(&lines).1);
    reply(if problems.is_empty() {
        json!({"rows": rows, "problems": []})
    } else {
        json!({"problems": problems})
    })
}

#[no_mangle]
pub unsafe extern "C" fn stone_visual_geometry_decode(
    data: *const u8,
    len: usize,
    width: u32,
    height: u32,
    cell_width: u32,
    cell_height: u32,
    frames: u32,
    directions: u32,
) -> *const c_char {
    let result = unsafe { text(data, len) }
        .map_err(anyhow::Error::msg)
        .and_then(|raw| {
            let geometry: SheetGeometry = serde_json::from_str(raw)?;
            geometry.validate_layout(
                [width, height],
                [cell_width, cell_height],
                frames as usize,
                directions as usize,
            )?;
            Ok(raw)
        });
    reply(match result {
        Ok(value) => json!({"raw": value}),
        Err(e) => json!({"problem": format!("{e:#}")}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn bank_boundary_rejects_a_valid_prefix_before_a_nul() {
        let valid = format!(
            r#"{{"kind":"icon","id":"icon","dir":"{}","frame_size":[1,1],"pivot":[0,0],"skin":false}}"#,
            "a".repeat(64)
        ) + "\n";
        for raw in [valid.clone(), valid + "\0junk\n"] {
            let result = unsafe { stone_visual_bank_decode(raw.as_ptr(), raw.len()) };
            let value: Value =
                serde_json::from_slice(unsafe { CStr::from_ptr(result) }.to_bytes()).unwrap();
            assert_eq!(
                value["problems"].as_array().unwrap().is_empty(),
                !raw.contains('\0')
            );
        }
    }
}

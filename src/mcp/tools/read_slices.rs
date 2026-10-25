//! Per-file line slices, merged before reading and bounded by a shared budget.
use crate::session::Session;
use serde_json::Value;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

type SliceGroup = (PathBuf, PathBuf, Vec<(u32, u32)>);

fn range(text: &str) -> Result<(u32, u32), String> {
    let (start, end) = text
        .split_once('-')
        .ok_or("slice ranges must be start-end")?;
    let parse = |v: &str| {
        if v.starts_with('0') || !v.bytes().all(|b| b.is_ascii_digit()) {
            Err("slice line numbers must be positive integers")
        } else {
            v.parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or("slice line numbers must be positive integers")
        }
    };
    let (start, end) = (parse(start)?, parse(end)?);
    if start > end {
        return Err("slice start must not exceed end".into());
    }
    Ok((start, end))
}

pub(super) fn read(args: &Value, session: &Session, edit: bool) -> Result<String, String> {
    for key in [
        "path", "paths", "symbols", "section", "sections", "offset", "limit",
    ] {
        if args.get(key).is_some() {
            return Err(format!("slices cannot be combined with {key}"));
        }
    }
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("auto");
    if !matches!(mode, "auto" | "full") {
        return Err("slices support auto or full mode".into());
    }
    let slices = args["slices"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 20)
        .ok_or("slices requires 1-20 objects")?;
    let root = args.get("root").and_then(Value::as_str).map(Path::new);
    let mut groups: Vec<SliceGroup> = Vec::new();
    let mut count = 0;
    for slice in slices {
        let raw = slice
            .get("path")
            .and_then(Value::as_str)
            .ok_or("each slice requires a path")?;
        let path = super::resolve_read_path(Path::new(raw), root)?;
        let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
        let ranges = slice
            .get("sections")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .ok_or("each slice requires nonempty sections")?;
        count += ranges.len();
        if count > 20 {
            return Err("slices limited to 20 ranges in total".into());
        }
        let ranges = ranges
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| "slice sections must contain strings".to_owned())
                    .and_then(range)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(group) = groups.iter_mut().find(|g| g.0 == identity) {
            group.2.extend(ranges);
        } else {
            groups.push((identity, path, ranges));
        }
    }
    let budget = args.get("budget").and_then(Value::as_u64);
    let mut summary = String::from("# Batch slices\n");
    let mut bodies = Vec::new();
    for (_, path, mut ranges) in groups {
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (start, end) in ranges {
            if let Some(last) = merged
                .last_mut()
                .filter(|last| start <= last.1.saturating_add(1))
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        let sections: Vec<_> = merged.iter().map(|(a, b)| format!("{a}-{b}")).collect();
        let sections: Vec<_> = sections.iter().map(String::as_str).collect();
        session.record_read(&path);
        let result = match budget {
            Some(budget) => crate::read::read_ranges_with_budget(&path, &sections, edit, budget),
            None => crate::read::read_ranges(&path, &sections, edit),
        };
        match result {
            Ok(body) => {
                let _ = writeln!(summary, "- {}: {}", path.display(), sections.join(", "));
                bodies.push(body);
            }
            Err(error) => {
                let _ = writeln!(summary, "- {}: error: {error}", path.display());
            }
        }
    }
    Ok(crate::listing::finish(
        &format!("{summary}\n{}", bodies.join("\n\n")),
        budget,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mcp_slices_validate_before_io_and_merge_duplicate_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let run = |slices: Value| {
            read(
                &serde_json::json!({"root":dir.path(),"slices":slices}),
                &Session::new(),
                false,
            )
        };
        let out=run(serde_json::json!([{"path":"a.txt","sections":["2-3"]},{"path":"a.txt","sections":["1-2"]}])).unwrap();
        assert_eq!(out.matches("beta").count(), 1, "{out}");
        assert!(out.contains("1-3"), "{out}");
        for slices in [
            serde_json::json!([]),
            serde_json::json!([null]),
            serde_json::json!([{"path":"a.txt","sections":[]}]),
            serde_json::json!([{"path":"a.txt","sections":["1-1","+2-3"]}]),
            serde_json::json!([{"path":"a.txt","sections":["01-2"]}]),
        ] {
            assert!(run(slices.clone()).is_err(), "{slices}");
        }
        let out = read(
            &serde_json::json!({"slices":[{"path":"relative","sections":["1-1"]}]}),
            &Session::new(),
            false,
        );
        assert!(out.unwrap_err().contains("root"));
    }
    #[cfg(unix)]
    #[test]
    fn aliases_merge_as_one_physical_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();
        std::os::unix::fs::symlink(&path, dir.path().join("alias.txt")).unwrap();
        let out=read(&serde_json::json!({"root":dir.path(),"slices":[{"path":"alias.txt","sections":["1-2"]},{"path":"a.txt","sections":["2-2"]}]}),&Session::new(),false).unwrap();
        assert_eq!(out.matches("beta").count(), 1, "{out}");
    }
}

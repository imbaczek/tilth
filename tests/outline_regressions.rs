//! Regressions for reported outline failures. These assert the intended behavior
//! and remain failing until their respective implementation fixes land.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use tilth::cache::OutlineCache;

fn read(path: &Path, cache: &OutlineCache, budget: Option<u64>) -> String {
    tilth::run(
        path.to_str().unwrap(),
        path.parent().unwrap(),
        None,
        budget,
        None,
        cache,
    )
    .unwrap()
}

fn padding() -> String {
    (0..300)
        .map(|i| format!("// explanatory context {i}: {}\n", "detail ".repeat(12)))
        .collect()
}

#[test]
fn outline_keeps_decorated_python_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("handlers.py");
    let source = "def plain():\n    return 1\n\n\
@decorator\ndef decorated_handler():\n    return 2\n\n\
class Service:\n    @property\n    def decorated_property(self):\n        return 3\n\n\
    def normal_method(self):\n        return 4\n\n\
@decorator\nclass DecoratedClass:\n    def nested_method(self):\n        return 5\n\n";
    fs::write(&path, format!("{source}{}", padding().replace("//", "#"))).unwrap();
    let output = read(&path, &OutlineCache::new(), None);
    assert!(output.contains("[outline]"), "fixture must use an outline");
    assert!(output.contains("plain") && output.contains("normal_method"));
    let missing: Vec<_> = [
        "decorated_handler",
        "decorated_property",
        "DecoratedClass",
        "nested_method",
    ]
    .into_iter()
    .filter(|name| !output.contains(name))
    .collect();
    assert!(missing.is_empty(), "outline silently omitted {missing:?}");
}

#[test]
fn tight_auto_read_budget_keeps_a_fitting_outline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("medium.rs");
    let mut source = String::new();
    for i in 0..8 {
        writeln!(source, "pub fn entry_{i}() {{").unwrap();
        source.push_str(&"    let body_only_marker = 123456789;\n".repeat(30));
        source.push_str("}\n\n");
    }
    // Below the automatic 6,000-token gate, above the requested 500-token budget.
    assert!(source.len() > 2_000 && source.len() < 24_000);
    fs::write(&path, &source).unwrap();
    let baseline = read(&path, &OutlineCache::new(), None);
    assert!(baseline.contains("[full]"));
    // Prove the entire outline can fit this budget using the same declarations.
    let outline_path = dir.path().join("outlined.rs");
    fs::write(&outline_path, format!("{source}{}", padding())).unwrap();
    let outline = read(&outline_path, &OutlineCache::new(), Some(500));
    assert!(outline.contains("[outline]") && outline.contains("entry_7"));
    assert!(!outline.contains("truncated"));

    let output = read(&path, &OutlineCache::new(), Some(500));
    assert!(
        output.contains("entry_7"),
        "tight auto read hid the final function"
    );
    assert!(output.contains("[outline]"));
    assert!(!output.contains("body_only_marker"));
}

#[test]
fn outline_preserves_multiline_parameters_and_return_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multiline.rs");
    let declaration = "pub fn multiline_handler(\n    request: &str,\n    retries: u8,\n) -> Result<usize, &'static str> {\n    let body_only_marker = 42;\n    Ok(request.len())\n}\n";
    fs::write(&path, format!("{declaration}{}", padding())).unwrap();
    let output = read(&path, &OutlineCache::new(), None);
    assert!(output.contains("[outline]"));
    for required in [
        "request: &str",
        "retries: u8",
        "Result<usize, &'static str>",
    ] {
        assert!(
            output.contains(required),
            "outline lost {required:?}: {output}"
        );
    }
    assert!(!output.contains("body_only_marker"));
}

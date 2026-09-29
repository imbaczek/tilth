//! Regression coverage for decorated and multiline outlines.

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

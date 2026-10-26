use super::*;
use serde_json::json;
fn run(args: &Value) -> crate::read::tags::ReadResponse {
    read_response(args, &OutlineCache::new(), &Session::new(), false, true).unwrap()
}
#[test]
fn opt_in_match_miss_and_request_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();
    assert!(run(&json!({"path":path})).tag.is_none());
    let first = run(&json!({"path":path,"tag":"always"}));
    let tag = first.tag.unwrap();
    let hit = run(&json!({"root":dir.path(),"path":"a.txt","if_tag":tag}));
    assert!(hit.unchanged);
    assert_eq!(hit.output, "View unchanged.");
    let unconditional = run(&json!({"path":path,"tag":"always"}));
    assert!(!unconditional.unchanged);
    assert_eq!(unconditional.tag.as_ref(), Some(&tag));
    assert!(!run(&json!({"path":path,"mode":"full","if_tag":tag})).unchanged);
    std::fs::write(&path, "gamma\nbeta\n").unwrap();
    let miss = run(&json!({"path":path,"if_tag":tag}));
    assert!(!miss.unchanged);
    assert!(miss.output.contains("gamma"));
    assert_ne!(miss.tag.as_ref(), Some(&tag));
    assert!(!run(&json!({"path":path,"if_tag":"r2: future-format"})).unchanged);
}
#[test]
fn views_ignore_hidden_changes_but_not_addresses_or_selectors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();
    let tag = run(&json!({"path":path,"section":"1-1","tag":"always"}))
        .tag
        .unwrap();
    std::fs::write(&path, "alpha\ngamma\n").unwrap();
    assert!(run(&json!({"path":path,"section":"1-1","if_tag":tag})).unchanged);
    assert!(!run(&json!({"path":path,"section":"2-2","if_tag":tag})).unchanged);
    let mut text = String::new();
    for i in 0..200 {
        writeln!(text, "line {i:03} a").unwrap();
    }
    std::fs::write(&path, &text).unwrap();
    let first = run(&json!({"path":path,"mode":"full","budget":100,"tag":"always"}));
    assert!(crate::types::estimate_tokens(first.text().len() as u64) <= 100);
    let tag = first.tag.unwrap();
    std::fs::write(&path, text.replace("line 199 a", "line 199 b")).unwrap();
    assert!(run(&json!({"path":path,"mode":"full","budget":100,"if_tag":tag})).unchanged);
    assert!(!run(&json!({"path":path,"mode":"full","budget":200,"if_tag":tag})).unchanged);
}
#[test]
fn same_mtime_shared_cache_cannot_hide_edits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn alpha() {}\n").unwrap();
    let time = std::fs::metadata(&path).unwrap().modified().unwrap();
    let cache = OutlineCache::new();
    cache.get_or_parse(&path).unwrap();
    let args = json!({"path":path,"symbols":["alpha"],"tag":"always"});
    let first = read_response(&args, &cache, &Session::new(), false, true).unwrap();
    std::fs::write(&path, "fn bravo() {}\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(time))
        .unwrap();
    let response = read_response(
        &json!({"path":path,"symbols":["bravo"],"if_tag":first.tag.unwrap()}),
        &cache,
        &Session::new(),
        false,
        true,
    )
    .unwrap();
    assert!(!response.unchanged);
    assert!(response.output.contains("bravo"));
    assert!(response.tag.is_some());
}
#[test]
fn failures_never_receive_or_validate_tags() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn alpha() {}\n").unwrap();
    let tag = run(&json!({"path":path,"tag":"always"})).tag.unwrap();
    for args in [
        json!({"root":dir.path(),"paths":["a.rs","missing.rs"],"if_tag":tag}),
        json!({"paths":[],"tag":"always"}),
        json!({"path":dir.path(),"tag":"always"}),
        json!({"root":dir.path(),"slices":[{"path":"missing.rs","sections":["1-1"]}],"if_tag":tag}),
        json!({"path":path,"symbols":["missing"],"if_tag":tag}),
        json!({"path":path,"symbols":["alpha","missing"],"tag":"always","budget":40}),
    ] {
        let response = run(&args);
        assert!(response.tag.is_none());
        assert!(!response.unchanged);
    }
    std::fs::remove_file(&path).unwrap();
    assert!(read_response(
        &json!({"path":path,"if_tag":tag}),
        &OutlineCache::new(),
        &Session::new(),
        false,
        true
    )
    .is_err());
}
#[test]
fn auto_floor_and_invalid_options() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "small").unwrap();
    assert!(run(&json!({"path":path,"tag":"auto"})).tag.is_none());
    let tag = run(&json!({"path":path,"tag":"always"})).tag.unwrap();
    assert!(run(&json!({"path":path,"tag":"auto","if_tag":tag})).unchanged);
    for extras in [
        json!({"tag":"never","if_tag":tag}),
        json!({"tag":true}),
        json!({"if_tag":false}),
        json!({"if_tag":"bad"}),
        json!({"tag":"always","budget":32}),
    ] {
        let mut args = json!({"path":path});
        args.as_object_mut()
            .unwrap()
            .extend(extras.as_object().unwrap().clone());
        assert!(read_response(&args, &OutlineCache::new(), &Session::new(), false, true).is_err());
    }
    std::fs::write(&path, "line\n".repeat(2000)).unwrap();
    let tag = run(&json!({"path":path,"tag":"auto"})).tag.unwrap();
    assert!(run(&json!({"path":path,"if_tag":tag})).unchanged);
}
#[test]
fn edit_mode_and_batch_order_are_part_of_view() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, "alpha\n").unwrap();
    std::fs::write(&b, "beta\n").unwrap();
    let first = run(&json!({"paths":[a,b],"tag":"always"}));
    let tag = first.tag.unwrap();
    assert!(run(&json!({"paths":[a,b],"if_tag":tag})).unchanged);
    assert!(!run(&json!({"paths":[b,a],"if_tag":tag})).unchanged);
    let first = run(&json!({"path":a,"tag":"always"}));
    let edited = read_response(
        &json!({"path":a,"if_tag":first.tag}),
        &OutlineCache::new(),
        &Session::new(),
        true,
        true,
    )
    .unwrap();
    assert!(!edited.unchanged);
    assert!(edited.output.contains('|'));
}

#[test]
fn ordinary_source_is_not_mistaken_for_listing_navigation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(
        &path,
        format!(
            "> Next page: literal source\n> End of listing. also source\n{}",
            "body content\n".repeat(200)
        ),
    )
    .unwrap();
    let response = run(&json!({"path":path,"mode":"full","budget":120,"tag":"always"}));
    assert!(
        response.output.contains("> Next page: literal source"),
        "{}",
        response.output
    );
    assert!(
        response.output.contains("> End of listing. also source"),
        "{}",
        response.output
    );
}

#[test]
fn related_hint_config_bypasses_same_mtime_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.ts");
    let config = dir.path().join("tsconfig.json");
    std::fs::write(dir.path().join("a.ts"), "export const a=1;\n").unwrap();
    std::fs::write(dir.path().join("b.ts"), "export const a=2;\n").unwrap();
    std::fs::write(
        &config,
        r#"{"compilerOptions":{"paths":{"@dep":["./a.ts"]}}}"#,
    )
    .unwrap();
    let time = std::fs::metadata(&config).unwrap().modified().unwrap();
    std::fs::write(
        &path,
        format!(
            "import {{a}} from '@dep';\nexport function owner() {{}}\n{}",
            "// padding\n".repeat(5000)
        ),
    )
    .unwrap();
    let warm = crate::read::imports::resolve_related_files(&path);
    assert!(warm.iter().any(|p| p.ends_with("a.ts")));
    let first = run(&json!({"path":path,"tag":"always"}));
    assert!(first.output.contains("> Related:"));
    assert!(first.output.contains("a.ts"));
    std::fs::write(
        &config,
        r#"{"compilerOptions":{"paths":{"@dep":["./b.ts"]}}}"#,
    )
    .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&config)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(time))
        .unwrap();
    let next = run(&json!({"path":path,"if_tag":first.tag}));
    assert!(!next.unchanged);
    assert!(next.output.contains("b.ts"));
    std::fs::write(&config, "{ invalid configuration").unwrap();
    assert!(run(&json!({"path":path,"tag":"always"})).tag.is_none());
}

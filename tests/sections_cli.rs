use std::process::Command;

#[test]
fn repeated_sections_work_for_shorthand_and_read_commands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample.txt");
    std::fs::write(&path, "alpha\nbravo\ncharlie\ndelta\necho\n").unwrap();
    for args in [
        vec!["sample.txt", "--section", "4-5", "--section", "1-2"],
        vec!["read", "sample.txt", "--section", "4-5", "--section", "1-2"],
        vec![
            "--section",
            "4-5",
            "search",
            "sample.txt",
            "--section",
            "1-2",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let out = String::from_utf8(output.stdout).unwrap();
        assert!(
            out.find("delta").unwrap() < out.find("alpha").unwrap(),
            "{out}"
        );
        assert!(!out.contains("charlie"), "{out}");
    }
}

#[test]
fn repeated_sections_page_and_reject_excess() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("sample.txt"),
        "alpha\nbravo\ncharlie\ndelta\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(dir.path())
        .args([
            "sample.txt",
            "--section",
            "1-2",
            "--section",
            "4-4",
            "--offset",
            "1",
            "--limit",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = String::from_utf8(output.stdout).unwrap();
    assert!(
        out.contains("bravo") && !out.contains("alpha") && !out.contains("delta"),
        "{out}"
    );
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tilth"));
    cmd.current_dir(dir.path()).arg("sample.txt");
    for _ in 0..21 {
        cmd.args(["--section", "1-1"]);
    }
    assert!(!cmd.output().unwrap().status.success());
}

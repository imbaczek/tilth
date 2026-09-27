use std::fs;
use std::process::Command;

#[test]
fn repeated_scopes_full_expand_zero_still_obey_final_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let first = tmp.path().join("first");
    let second = tmp.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();

    for scope in [&first, &second] {
        for i in 0..15 {
            fs::write(
                scope.join(format!("file_{i:02}.rs")),
                "pub fn WidgetThing() {}\n",
            )
            .unwrap();
        }
    }

    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(tmp.path())
        .arg("WidgetThing")
        .arg("--scope")
        .arg(&first)
        .arg("--scope")
        .arg(&second)
        .arg("--full")
        .arg("--expand=0")
        .arg("--budget")
        .arg("80")
        .output()
        .expect("run tilth CLI");

    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("CLI output is UTF-8");
    let search_header = stdout
        .lines()
        .find(|line| line.starts_with("# Search:"))
        .unwrap_or_else(|| panic!("missing search header: {stdout}"));
    let count = search_header
        .rsplit_once("— ")
        .expect("search header count separator")
        .1
        .split_whitespace()
        .next()
        .expect("search header count")
        .parse::<usize>()
        .expect("numeric match count");

    assert!(
        count > 15,
        "--full --expand=0 with both scopes should retain more than one scope's default cap; header: {search_header}"
    );
    assert!(
        stdout.contains("... truncated"),
        "--budget must cap the final output after both scopes are joined: {stdout}"
    );
}

use std::process::Command;

fn cli_output(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .args(args)
        .output()
        .expect("run tilth");
    assert!(
        output.status.success(),
        "tilth {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 CLI output")
}

#[test]
fn completions_suggest_all_cli_verbs_for_every_shell() {
    for shell in ["bash", "elvish", "fish", "powershell", "zsh"] {
        let script = cli_output(&["--completions", shell]);
        for verb in ["read", "search", "install", "diff", "overview", "grok"] {
            let suggested = match shell {
                "bash" => script.lines().any(|line| {
                    line.trim_start().starts_with("opts=")
                        && line.split([' ', '"']).any(|word| word == verb)
                }),
                "elvish" => script.contains(&format!("cand {verb} '")),
                "fish" => script.contains(&format!(
                    "__fish_tilth_needs_command\" -a \"{verb}\""
                )),
                "powershell" => script.contains(&format!(
                    "[CompletionResult]::new('{verb}', '{verb}', [CompletionResultType]::ParameterValue,"
                )),
                "zsh" => script.contains(&format!("'{verb}:")),
                _ => unreachable!(),
            };
            assert!(suggested, "{shell} completions do not suggest {verb}");
        }
    }
}

#[test]
fn help_shows_a_comma_separated_search_example() {
    for flag in ["--help", "-h"] {
        let help = cli_output(&[flag]);
        assert!(help.contains("tilth \"Foo,Bar\" --scope src"));
    }
}

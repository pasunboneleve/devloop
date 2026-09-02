use std::process::Command;

#[test]
fn root_help_names_the_artifact_use_case_and_guide() {
    let output = Command::new(env!("CARGO_BIN_EXE_devloop"))
        .arg("--help")
        .output()
        .expect("run root help");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help is UTF-8");
    assert!(stdout.contains("Serving generated files that are replaced during rebuilds?"));
    assert!(stdout.contains("devloop docs artifacts"));
}

#[test]
fn bare_docs_prints_the_topic_index() {
    let output = Command::new(env!("CARGO_BIN_EXE_devloop"))
        .arg("docs")
        .output()
        .expect("run docs index");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("docs index is UTF-8");
    assert!(stdout.starts_with("DEVLOOP DOCUMENTATION"));
    assert!(stdout.contains("devloop docs artifacts"));
    assert!(stdout.contains("replaces files that a managed process is serving"));
}

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=ASHOT_BUILD_ID");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");

    let build_id = std::env::var("ASHOT_BUILD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(git_build_id);
    println!("cargo:rustc-env=ASHOT_BUILD_ID={}", sanitize_build_id(&build_id));
}

fn git_build_id() -> String {
    Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .and_then(|output| output.status.success().then_some(output.stdout))
        .and_then(|stdout| String::from_utf8(stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn sanitize_build_id(value: &str) -> String {
    value.trim().chars().map(|ch| if ch.is_control() { '_' } else { ch }).collect()
}

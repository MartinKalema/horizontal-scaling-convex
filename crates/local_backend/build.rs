use vergen::EmitBuilder;

fn main() -> anyhow::Result<()> {
    println!("cargo:rerun-if-env-changed=VERGEN_GIT_SHA");
    println!("cargo:rerun-if-env-changed=VERGEN_GIT_COMMIT_TIMESTAMP");

    // Recompile when there's a new git hash for beacon.
    // This is a workaround for https://github.com/rustyhorde/vergen/issues/174
    // Docker builds have no Git checkout, so explicit build arguments must take
    // precedence over Vergen's repository discovery.
    if let (Ok(git_sha), Ok(git_commit_timestamp)) = (
        std::env::var("VERGEN_GIT_SHA"),
        std::env::var("VERGEN_GIT_COMMIT_TIMESTAMP"),
    ) {
        println!("cargo:rustc-env=VERGEN_GIT_SHA={git_sha}");
        println!("cargo:rustc-env=VERGEN_GIT_COMMIT_TIMESTAMP={git_commit_timestamp}");
        return Ok(());
    }

    // Fall back to explicit unknown values if repository discovery fails too.
    if EmitBuilder::builder()
        .git_sha(false)
        .git_commit_timestamp()
        .fail_on_error()
        .emit()
        .is_err()
    {
        println!("cargo:rerun-if-changed=build.rs");
        println!(
            "cargo:rustc-env=VERGEN_GIT_SHA={}",
            std::env::var("VERGEN_GIT_SHA").unwrap_or_else(|_| "unknown".to_string())
        );
        println!(
            "cargo:rustc-env=VERGEN_GIT_COMMIT_TIMESTAMP={}",
            std::env::var("VERGEN_GIT_COMMIT_TIMESTAMP").unwrap_or_else(|_| "unknown".to_string())
        );
    }
    Ok(())
}

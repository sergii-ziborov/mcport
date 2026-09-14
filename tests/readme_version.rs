#[test]
fn readme_install_version_matches_the_package() {
    let readme = include_str!("../README.md");
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        readme.contains(&format!("mcport = \"{version}\""))
            || readme.contains(&format!("mcport@{version}")),
        "README must install the published package version {version}"
    );
    assert!(
        !readme.contains("mcport = \"0.4.0\""),
        "README must not recommend the unpublished 0.4.0 crate"
    );
}

#[test]
fn benchmark_docs_agree_on_historical_sdk_table() {
    let root = include_str!("../README.md");
    let competitors = include_str!("../benchmarks/competitors/README.md");
    assert!(
        root.contains("published mcport 0.3.0") && competitors.contains("published `mcport 0.3.0`"),
        "SDK comparison numbers must stay attributed to 0.3.0 until a pinned rerun"
    );
    assert!(
        !root.contains("mcport 0.4.0") || !root.contains("The table uses `mcport 0.4.0`"),
        "do not relabel the historical SDK table as 0.4.0"
    );
}

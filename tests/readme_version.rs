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
fn benchmark_docs_agree_on_published_sdk_table() {
    let root = include_str!("../README.md");
    let competitors = include_str!("../benchmarks/competitors/README.md");
    assert!(
        root.contains("published mcport 0.6.0") && competitors.contains("published `mcport 0.6.0`"),
        "SDK comparison numbers must be attributed to measured 0.6.0"
    );
    assert!(
        root.contains("three complete runner invocations")
            && competitors.contains("three complete runner invocations"),
        "both READMEs must cite the three-invocation method"
    );
    assert!(
        root.contains("rustc 1.97.1") && competitors.contains("rustc 1.97.1"),
        "both READMEs must record the MSVC rustc used for the published table"
    );
    for needle in [
        "12.42–14.72×",
        "17.76–23.37×",
        "7.91–11.11×",
        "11.91–13.64×",
    ] {
        assert!(root.contains(needle), "root README missing {needle}");
    }
    assert!(
        !root.contains("These historical numbers are for **published mcport 0.3.0**"),
        "do not keep 0.3.0 as the primary published claim"
    );
}

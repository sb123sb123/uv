use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context, Result};
#[cfg(feature = "test-pypi")]
use assert_cmd::assert::OutputAssertExt;
use assert_fs::fixture::FileWriteStr;
use assert_fs::fixture::{FileWriteBin, PathChild, PathCreateDir};
use indoc::{formatdoc, indoc};
use insta::allow_duplicates;
#[cfg(feature = "test-pypi")]
use insta::assert_snapshot;
use serde_json::json;
use sha2::{Digest, Sha256, Sha384, Sha512};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

use uv_extract::hash::Hasher;
use uv_pypi_types::{HashAlgorithm, HashDigest};
use uv_static::EnvVars;
use uv_test::archive::generate_source_archive;
use uv_test::packse::generate_wheel_with_files;
use uv_test::{TestContext, uv_snapshot};

#[cfg(feature = "test-pypi")]
fn dependency(context: &TestContext, version: &str) -> Result<String> {
    context
        .temp_dir
        .child("requirements.in")
        .write_str(&format!("idna=={version}"))?;
    context
        .pip_compile()
        .args(["requirements.in", "--no-deps", "-o", "pylock.toml"])
        .assert()
        .success();
    Ok(context.read("pylock.toml"))
}

fn tool(
    context: &TestContext,
    version: &str,
    lock: Option<&str>,
    python: Option<&str>,
) -> Result<()> {
    let pylock_path = format!("locked_tool-{version}.dist-info/pylock.toml");
    let entrypoints_path = format!("locked_tool-{version}.dist-info/entry_points.txt");
    let mut files = vec![
        (
            entrypoints_path.as_str(),
            "[console_scripts]\nlocked-tool = locked_tool.cli:main\n",
        ),
        (
            "locked_tool/cli.py",
            "from importlib.metadata import version\ndef main(): print(version('idna'))\n",
        ),
    ];
    if let Some(lock) = lock {
        files.push((&pylock_path, lock));
    }
    let (filename, bytes) = generate_wheel_with_files(
        &"locked-tool".parse()?,
        &version.parse()?,
        &["idna>=3.3,<3.5".parse()?],
        &BTreeMap::new(),
        python.map(str::parse).transpose()?.as_ref(),
        "py3-none-any",
        &files,
    );
    context
        .temp_dir
        .child("wheels")
        .child(filename)
        .write_binary(&bytes)?;
    Ok(())
}

#[test]
fn packaged_lock_rejects_sources_before_build() -> Result<()> {
    let context = uv_test::test_context!("3.12").with_tool_dirs();
    context.temp_dir.child("source").create_dir_all()?;
    context
        .temp_dir
        .child("source/pyproject.toml")
        .write_str(indoc! {r#"
        [build-system]
        requires = []
        build-backend = "backend"
        backend-path = ["."]
    "#})?;
    context
        .temp_dir
        .child("source/backend.py")
        .write_str(&format!(
            "from pathlib import Path\nPath({:?}).write_text('executed')\n",
            context.temp_dir.child("executed").path().to_string_lossy()
        ))?;
    context
        .temp_dir
        .child("override.txt")
        .write_str("./source\n")?;

    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "--preview-features", "locked-tools", "--no-index", "--from", "./source", "source-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: A wheel is required to install a tool with `--locked`
    ");
    uv_snapshot!(context.filters(), context.tool_install().args(["--locked", "--preview-features", "locked-tools", "--no-index", "./source"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: A wheel is required to install a tool with `--locked`
    ");
    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "--preview-features", "locked-tools", "--no-index", "--with", "./source", "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `--locked` requires a single tool package and cannot be combined with `--with`
    ");
    uv_snapshot!(context.filters(), context.tool_install().args(["--locked", "--preview-features", "locked-tools", "--no-index", "--with", "./source", "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `--locked` requires a single tool package and cannot be combined with `--with`
    ");
    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "--preview-features", "locked-tools", "--no-index", "--override", "override.txt", "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `--locked` cannot be combined with dependency overrides or exclusions
    ");
    uv_snapshot!(context.filters(), context.tool_install().args(["--locked", "--preview-features", "locked-tools", "--no-index", "--override", "override.txt", "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `--locked` cannot be combined with dependency overrides or exclusions
    ");
    assert!(!context.temp_dir.child("executed").path().exists());
    Ok(())
}

#[tokio::test]
async fn packaged_lock_root_url_hash() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_tool_dirs();
    context.temp_dir.child("wheels").create_dir_all()?;
    tool(&context, "1.0.0", None, None)?;
    let wheel = context
        .temp_dir
        .child("wheels/locked_tool-1.0.0-py3-none-any.whl");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/locked_tool-1.0.0-py3-none-any.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fs_err::read(wheel.path())?))
        .mount(&server)
        .await;
    let url = format!(
        "{}/locked_tool-1.0.0-py3-none-any.whl#sha256={}",
        server.uri(),
        "0".repeat(64)
    );
    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "--preview-features", "locked-tools", "--no-index", "--from", &url, "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The selected artifact for `locked-tool @ http://[LOCALHOST]/locked_tool-1.0.0-py3-none-any.whl#sha256=0000000000000000000000000000000000000000000000000000000000000000` does not match the required hashes
    ");
    Ok(())
}

#[tokio::test]
async fn packaged_lock_binds_hashless_tool_wheel() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_tool_dirs();
    let server = MockServer::start().await;
    let name = "race-tool".parse()?;
    let version = "1.0.0".parse()?;
    let make_wheel = |message: &str, lock: &str| {
        generate_wheel_with_files(
            &name,
            &version,
            &[],
            &BTreeMap::new(),
            None,
            "py3-none-any",
            &[
                (
                    "race_tool/cli.py",
                    &format!("def main(): print({message:?})\n"),
                ),
                (
                    "race_tool-1.0.0.dist-info/entry_points.txt",
                    "[console_scripts]\nrace-tool = race_tool.cli:main\n",
                ),
                ("race_tool-1.0.0.dist-info/pylock.toml", lock),
            ],
        )
    };
    let (filename, first) = make_wheel(
        "first",
        "lock-version = \"1.0\"\ncreated-by = \"test\"\npackages = []\n",
    );
    let (_, second) = make_wheel("second", "invalid lock");
    let context = context
        .with_filter((hex::encode(Sha256::digest(&first)), "[FIRST_HASH]"))
        .with_filter((hex::encode(Sha256::digest(&second)), "[SECOND_HASH]"));
    let index = format!("{}/simple", server.uri());
    let url = format!("{}/files/{filename}", server.uri());
    Mock::given(method("GET"))
        .and(path("/simple/race-tool/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("cache-control", "no-store")
                .set_body_raw(
                    json!({
                        "meta": { "api-version": "1.1" },
                        "name": "race-tool",
                        "files": [
                            { "filename": filename, "url": url, "hashes": {}, "upload-time": "2023-01-01T00:00:00Z" },
                            { "filename": "race_tool-2.0.0.tar.gz", "url": format!("{}/files/race_tool-2.0.0.tar.gz", server.uri()), "hashes": {}, "upload-time": "2023-01-01T00:00:00Z" }
                        ]
                    })
                    .to_string(),
                    "application/vnd.pypi.simple.v1+json",
                ),
        )
        .mount(&server)
        .await;
    let requests = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(move |_: &wiremock::Request| {
            let body = if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                &first
            } else {
                &second
            };
            ResponseTemplate::new(200)
                .insert_header("cache-control", "no-store")
                .set_body_bytes(body.clone())
        })
        .mount(&server)
        .await;
    uv_snapshot!(context.filters(), context.tool_run()
        .args([
            "--locked",
            "--preview-features",
            "locked-tools",
            "--refresh",
            "--index-url",
            &index,
            "race-tool",
        ]), @"
    exit_code: 1 (failure)
    ----- stderr -----
    error: Failed to download `race-tool==1.0.0`
      cause: Hash mismatch for `race-tool==1.0.0`

             Expected:
               sha256:[FIRST_HASH]

             Computed:
               sha256:[SECOND_HASH]
    ");
    Ok(())
}

#[tokio::test]
async fn packaged_lock_configured_index_and_constraints() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_filtered_exe_suffix()
        .with_tool_dirs();
    context.temp_dir.child("wheels").create_dir_all()?;
    let server = MockServer::start().await;
    let index = format!("{}/simple", server.uri());
    let (filename, wheel) = generate_wheel_with_files(
        &"idna".parse()?,
        &"3.3".parse()?,
        &[],
        &BTreeMap::new(),
        None,
        "py3-none-any",
        &[],
    );
    let hash = hex::encode(Sha256::digest(&wheel));
    let hash_sha512 = hex::encode(Sha512::digest(&wheel));
    let url = format!("{}/files/{filename}", server.uri());
    Mock::given(method("GET"))
        .and(path("/simple/idna/"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("cache-control", "max-age=0").set_body_raw(
                json!({
                    "meta": { "api-version": "1.1" },
                    "name": "idna",
                    "files": [{ "filename": filename, "url": url, "hashes": { "sha256": hash } }]
                })
                .to_string(),
                "application/vnd.pypi.simple.v1+json",
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .mount(&server)
        .await;
    let lock = formatdoc! {r#"
        lock-version = "1.0"
        created-by = "test"
        [[packages]]
        name = "idna"
        version = "3.3"
        index = "{index}"
        wheels = [{{ url = "{url}", hashes = {{ sha256 = "{hash}" }} }}]
    "#};
    tool(&context, "1.0.0", Some(&lock), None)?;
    let (different_filename, different_wheel) = generate_wheel_with_files(
        &"idna".parse()?,
        &"3.3".parse()?,
        &[],
        &BTreeMap::new(),
        None,
        "py3-none-any",
        &[("idna/private.py", "")],
    );
    context
        .temp_dir
        .child("wheels")
        .child(different_filename)
        .write_binary(&different_wheel)?;
    let bin_dir = context.temp_dir.child("bin");
    context
        .tool_install()
        .args(["--no-index", "--find-links", "wheels", "locked-tool"])
        .env(EnvVars::PATH, bin_dir.as_os_str())
        .assert()
        .success();
    let private_module = if cfg!(windows) {
        "tools/locked-tool/Lib/site-packages/idna/private.py"
    } else {
        "tools/locked-tool/lib/python3.12/site-packages/idna/private.py"
    };
    assert!(context.temp_dir.child(private_module).path().exists());
    context
        .temp_dir
        .child("constraints.txt")
        .write_str(&format!("idna==3.3 --hash=sha256:{hash}\n"))?;
    context
        .tool_install()
        .args([
            "--preview-features",
            "locked-tools",
            "--find-links",
            "wheels",
            "--index-url",
            &index,
            "--constraint",
            "constraints.txt",
            "locked-tool",
        ])
        .env(EnvVars::UV_TOOL_LOCKED, "1")
        .env(EnvVars::PATH, bin_dir.as_os_str())
        .assert()
        .success();
    assert!(
        context
            .read("tools/locked-tool/uv-receipt.toml")
            .contains("locked = true")
    );
    assert!(!context.temp_dir.child(private_module).path().exists());
    assert!(
        context
            .read("tools/locked-tool/uv-receipt.toml")
            .contains(&hash)
    );
    let requests_before = server
        .received_requests()
        .await
        .context("request recording disabled")?;
    let index_requests_before = requests_before
        .iter()
        .filter(|request| request.url.path() == "/simple/idna/")
        .count();
    context
        .tool_run()
        .args([
            "--locked",
            "--isolated",
            "--preview-features",
            "locked-tools",
            "--find-links",
            "wheels",
            "--index-url",
            &index,
            "locked-tool",
        ])
        .assert()
        .success();
    let requests_after = server
        .received_requests()
        .await
        .context("request recording disabled")?;
    assert_eq!(
        requests_after
            .iter()
            .filter(|request| request.url.path() == "/simple/idna/")
            .count(),
        index_requests_before
    );
    context
        .temp_dir
        .child("constraints.txt")
        .write_str("idna==3.4\n")?;
    uv_snapshot!(context.filters(), context.tool_run()
        .args(["--locked", "--preview-features", "locked-tools", "--find-links", "wheels",
            "--index-url", &index, "--constraint", "constraints.txt", "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The packaged lock selects `idna==3.3`, which is incompatible with constraint `idna==3.4`
    ");
    context.temp_dir.child("constraints.txt").write_str(
        "idna==3.3 --hash=sha256:0000000000000000000000000000000000000000000000000000000000000000\n",
    )?;
    uv_snapshot!(context.filters(), context.tool_run()
        .args(["--locked", "--preview-features", "locked-tools", "--find-links", "wheels",
            "--index-url", &index, "--constraint", "constraints.txt", "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The selected artifact for `idna==3.3` does not match the required hashes
    ");
    context.temp_dir.child("constraints.txt").write_str(
        "locked-tool==1.0.0 --hash=sha256:0000000000000000000000000000000000000000000000000000000000000000\n",
    )?;
    context
        .tool_run()
        .args([
            "--locked",
            "--preview-features",
            "locked-tools",
            "--find-links",
            "wheels",
            "--index-url",
            &index,
            "--constraint",
            "constraints.txt",
            "locked-tool",
        ])
        .assert()
        .failure();
    context
        .tool_run()
        .args([
            "--locked",
            "--preview-features",
            "locked-tools",
            "--index-url",
            &index,
            "--constraint",
            "constraints.txt",
            "--from",
            "./wheels/locked_tool-1.0.0-py3-none-any.whl",
            "locked-tool",
        ])
        .assert()
        .failure();
    let tool_hash = hex::encode(Sha256::digest(fs_err::read(
        context
            .temp_dir
            .child("wheels/locked_tool-1.0.0-py3-none-any.whl"),
    )?));
    context
        .temp_dir
        .child("constraints.txt")
        .write_str(&format!("locked-tool>=1 --hash=sha256:{tool_hash}\n"))?;
    context
        .tool_run()
        .args([
            "--locked",
            "--preview-features",
            "locked-tools",
            "--index-url",
            &index,
            "--constraint",
            "constraints.txt",
            "--from",
            "./wheels/locked_tool-1.0.0-py3-none-any.whl",
            "locked-tool",
        ])
        .assert()
        .success();
    context
        .tool_upgrade()
        .args([
            "--unlocked",
            "--preview-features",
            "locked-tools",
            "locked-tool",
        ])
        .env(EnvVars::UV_TOOL_LOCKED, "1")
        .env(EnvVars::PATH, bin_dir.as_os_str())
        .assert()
        .success();
    assert!(
        !context
            .read("tools/locked-tool/uv-receipt.toml")
            .contains("locked = true")
    );
    context
        .tool_upgrade()
        .args([
            "--locked",
            "--preview-features",
            "locked-tools",
            "locked-tool",
        ])
        .env(EnvVars::PATH, bin_dir.as_os_str())
        .assert()
        .success();
    assert!(
        context
            .read("tools/locked-tool/uv-receipt.toml")
            .contains("locked = true")
    );
    context
        .temp_dir
        .child("constraints.txt")
        .write_str(&format!("idna==3.3 --hash=sha256:{hash}\n"))?;
    context
        .tool_install()
        .args([
            "--unlocked",
            "--find-links",
            "wheels",
            "--index-url",
            &index,
            "--constraint",
            "constraints.txt",
            "locked-tool",
        ])
        .env(EnvVars::PATH, bin_dir.as_os_str())
        .assert()
        .success();
    assert!(
        !context
            .read("tools/locked-tool/uv-receipt.toml")
            .contains("locked = true")
    );
    context
        .tool_upgrade()
        .args([
            "--locked",
            "--preview-features",
            "locked-tools",
            "locked-tool",
        ])
        .env(EnvVars::PATH, bin_dir.as_os_str())
        .assert()
        .success();
    let invalid_hash = lock.replace(
        &hash,
        "0000000000000000000000000000000000000000000000000000000000000000",
    );
    tool(&context, "1.1.0", Some(&invalid_hash), None)?;
    uv_snapshot!(context.filters(), context.tool_upgrade()
        .args(["--preview-features", "locked-tools", "locked-tool"])
        .env(EnvVars::PATH, bin_dir.as_os_str()), @"
    exit_code: 1 (failure)
    ----- stderr -----
    error: Failed to upgrade locked-tool
      cause: Hash mismatch for `idna==3.3`

             Expected:
               sha256:0000000000000000000000000000000000000000000000000000000000000000

             Computed:
               sha256:b7a84ce11140568e6ef8f9ecd74ae5dee9dfab081a565ff422ebaa0d246bd078
    ");
    tool(&context, "1.2.0", Some(&lock), None)?;
    context
        .tool_upgrade()
        .args(["--preview-features", "locked-tools", "locked-tool"])
        .env(EnvVars::PATH, bin_dir.as_os_str())
        .assert()
        .success();
    assert!(
        context
            .read("tools/locked-tool/uv-receipt.toml")
            .contains("locked = true")
    );
    context
        .tool_upgrade()
        .args([
            "--unlocked",
            "--preview-features",
            "locked-tools",
            "locked-tool",
        ])
        .env(EnvVars::UV_TOOL_LOCKED, "1")
        .env(EnvVars::PATH, bin_dir.as_os_str())
        .assert()
        .success();
    assert!(
        !context
            .read("tools/locked-tool/uv-receipt.toml")
            .contains("locked = true")
    );
    let redirected = lock.replace(&url, &format!("{}/private/{filename}", server.uri()));
    tool(&context, "2.0.0", Some(&redirected), None)?;
    uv_snapshot!(context.filters(), context.tool_run()
        .args(["--locked", "--isolated", "--preview-features", "locked-tools", "--find-links",
            "wheels", "--index-url", &index, "locked-tool==2.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The packaged lock for `locked-tool==2.0.0` contains unverified artifacts
      cause: URL for `idna==3.3` is not listed by http://[LOCALHOST]/simple: http://[LOCALHOST]/private/idna-3.3-py3-none-any.whl
    ");
    let unconfigured = lock.replace(&index, "https://example.invalid/simple");
    tool(&context, "3.0.0", Some(&unconfigured), None)?;
    uv_snapshot!(context.filters(), context.tool_run()
        .args(["--locked", "--isolated", "--preview-features", "locked-tools", "--find-links",
            "wheels", "--index-url", &index, "locked-tool==3.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The packaged lock for `locked-tool==3.0.0` contains unverified artifacts
      cause: Index `https://example.invalid/simple` from the packaged lock is not configured
    ");
    assert!(
        !server
            .received_requests()
            .await
            .context("request recording disabled")?
            .iter()
            .any(|request| request.url.path().starts_with("/private/"))
    );
    let incorrect_sha512 = "0".repeat(128);
    let multiple_hashes = lock.replace(
        &format!("sha256 = \"{hash}\""),
        &format!("sha256 = \"{hash}\", sha512 = \"{incorrect_sha512}\""),
    );
    tool(&context, "4.0.0", Some(&multiple_hashes), None)?;
    context
        .temp_dir
        .child("constraints.txt")
        .write_str(&format!("idna==3.3 --hash=sha512:{incorrect_sha512}\n"))?;
    context
        .tool_run()
        .args([
            "--locked",
            "--isolated",
            "--preview-features",
            "locked-tools",
            "--find-links",
            "wheels",
            "--index-url",
            &index,
            "--constraint",
            "constraints.txt",
            "locked-tool==4.0.0",
        ])
        .assert()
        .failure();
    tool(&context, "5.0.0", Some(&lock), None)?;
    context
        .temp_dir
        .child("constraints.txt")
        .write_str(&format!("idna==3.3 --hash=sha512:{hash_sha512}\n"))?;
    context
        .tool_run()
        .args([
            "--locked",
            "--isolated",
            "--preview-features",
            "locked-tools",
            "--find-links",
            "wheels",
            "--index-url",
            &index,
            "--constraint",
            "constraints.txt",
            "locked-tool==5.0.0",
        ])
        .assert()
        .success();
    Ok(())
}

#[test]
#[cfg(feature = "test-pypi")]
fn packaged_lock_install_run_upgrade() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_filtered_exe_suffix()
        .with_tool_dirs();
    context.temp_dir.child("wheels").create_dir_all()?;
    let bin_dir = context.temp_dir.child("bin");
    let lock = dependency(&context, "3.3")?;
    let newer_lock = dependency(&context, "3.4")?;
    tool(&context, "1.0.0", Some(&lock), None)?;
    // Compatibility may eliminate a release before its lock is considered.
    tool(&context, "2.0.0", None, Some(">=3.13"))?;

    uv_snapshot!(context.filters(), context.tool_install()
        .args(["locked-tool", "--find-links", "wheels"])
        .env(EnvVars::PATH, bin_dir.as_os_str()), @"
    exit_code: 0 (success)
    ----- stderr -----
    Resolved [N] packages in [TIME]
    Prepared [N] packages in [TIME]
    Installed [N] packages in [TIME]
     + idna==3.4
     + locked-tool==1.0.0
    Installed 1 executable: locked-tool
    ");

    uv_snapshot!(context.filters(), context.tool_run()
        .args(["--locked", "--preview-features", "locked-tools", "--find-links", "wheels", "locked-tool"]), @"
    exit_code: 0 (success)
    ----- stdout -----
    3.3

    ----- stderr -----
    Installed [N] packages in [TIME]
     + idna==3.3
     + locked-tool==1.0.0
    ");

    // An existing installation resolved normally must be brought back to the packaged pins.
    uv_snapshot!(context.filters(), context.tool_install()
        .args(["--locked", "--preview-features", "locked-tools", "locked-tool", "--find-links", "wheels"])
        .env(EnvVars::PATH, bin_dir.as_os_str()), @"
    exit_code: 0 (success)
    ----- stderr -----
    Uninstalled [N] packages in [TIME]
    Installed [N] packages in [TIME]
     - idna==3.4
     + idna==3.3
     ~ locked-tool==1.0.0
    Installed 1 executable: locked-tool
    ");
    insta::with_settings!({ filters => context.filters() }, {
        assert_snapshot!(context.read("tools/locked-tool/uv-receipt.toml"), @r#"
        [tool]
        locked = true
        requirements = [{ name = "locked-tool" }]
        entrypoints = [
            { name = "locked-tool", install-path = "[TEMP_DIR]/bin/locked-tool", from = "locked-tool" },
        ]

        [tool.options]
        find-links = ["file://[TEMP_DIR]/wheels"]
        exclude-newer = "2024-03-25T00:00:00Z"
        "#);
    });

    tool(&context, "1.1.0", Some(&newer_lock), None)?;
    // The receipt keeps upgrades locked even without repeating --locked.
    uv_snapshot!(context.filters(), context.tool_upgrade()
        .args(["--preview-features", "locked-tools", "locked-tool"])
        .env(EnvVars::PATH, bin_dir.as_os_str()), @"
    exit_code: 0 (success)
    ----- stderr -----
    Updated locked-tool v1.0.0 -> v1.1.0
     - idna==3.3
     + idna==3.4
     - locked-tool==1.0.0
     + locked-tool==1.1.0
    Installed 1 executable: locked-tool
    ");
    uv_snapshot!(context.filters(), context.tool_upgrade()
        .args(["--locked", "--preview-features", "locked-tools", "locked-tool"])
        .env(EnvVars::PATH, bin_dir.as_os_str()), @"
    exit_code: 0 (success)
    ----- stderr -----
    Modified locked-tool environment
     ~ idna==3.4
     ~ locked-tool==1.1.0
    Installed 1 executable: locked-tool
    ");
    Ok(())
}

#[test]
fn packaged_lock_required() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_filtered_exe_suffix()
        .with_tool_dirs();
    context.temp_dir.child("wheels").create_dir_all()?;
    let lock = indoc! {r#"
        lock-version = "1.0"
        created-by = "test"
        requires-python = ">=3.12"
        packages = []
    "#};
    tool(&context, "1.0.0", Some(lock), None)?;
    tool(&context, "2.0.0", None, None)?;

    uv_snapshot!(context.filters(), context.tool_install().args(["--locked", "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `--locked` for tools requires the `locked-tools` preview feature
    ");
    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `--locked` for tools requires the `locked-tools` preview feature
    ");
    uv_snapshot!(context.filters(), context.tool_upgrade().args(["--locked", "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `--locked` for tools requires the `locked-tools` preview feature
    ");
    uv_snapshot!(context.filters(), context.tool_run()
        .args(["--locked", "--preview-features", "locked-tools", "python"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `--locked` requires a tool package with a bundled lock, not a Python interpreter
    ");
    // A compatible release without a lock cannot cause a fallback to an older release.
    uv_snapshot!(context.filters(), context.tool_install()
        .args(["--locked", "--preview-features", "locked-tools", "--no-index", "--find-links", "wheels", "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `locked-tool==2.0.0` does not contain `pylock.toml` in its `.dist-info` directory; `--locked` requires a packaged lock
    ");

    tool(&context, "3.0.0", Some("not valid TOML"), None)?;
    uv_snapshot!(context.filters(), context.tool_run()
        .args(["--locked", "--preview-features", "locked-tools", "--no-index", "--find-links", "wheels", "locked-tool==3.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `locked-tool==3.0.0` contains an invalid `pylock.toml`
      cause: TOML parse error at line 1, column 5
               |
             1 | not valid TOML
               |     ^
             key with no value, expected `=`
    ");

    tool(
        &context,
        "4.0.0",
        Some(&lock.replace(">=3.12", ">=3.13")),
        None,
    )?;
    uv_snapshot!(context.filters(), context.tool_install()
        .args(["--locked", "--preview-features", "locked-tools", "--no-index", "--find-links", "wheels", "locked-tool==4.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The requested interpreter resolved to Python 3.12.[X], which is incompatible with the `pylock.toml`'s Python requirement: `>=3.13`
    ");

    uv_snapshot!(context.filters(), context.tool_install()
        .args(["--locked", "--preview-features", "locked-tools", "--no-index", "--find-links", "wheels", "--with", "locked-dependency", "locked-tool==1.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `--locked` requires a single tool package and cannot be combined with `--with`
    ");
    Ok(())
}

#[test]
#[cfg(feature = "test-pypi")]
fn packaged_lock_hashes() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_filtered_exe_suffix()
        .with_tool_dirs();
    context.temp_dir.child("wheels").create_dir_all()?;
    let lock = dependency(&context, "3.3")?;
    tool(&context, "1.0.0", Some(&lock), None)?;
    // Change the expected digest while retaining a valid lock and artifact.
    let mut lock: toml::Value = toml::from_str(&lock)?;
    lock["packages"][0]["wheels"][0]["hashes"]["sha256"] = toml::Value::String("0".repeat(64));
    tool(&context, "2.0.0", Some(&toml::to_string(&lock)?), None)?;
    let bin_dir = context.temp_dir.child("bin");
    context
        .tool_install()
        .args([
            "--locked",
            "--preview-features",
            "locked-tools",
            "--find-links",
            "wheels",
            "locked-tool==1.0.0",
        ])
        .env(EnvVars::PATH, bin_dir.as_os_str())
        .assert()
        .success();
    uv_snapshot!(context.filters(), context.tool_run()
        .args(["--locked", "--preview-features", "locked-tools", "--find-links", "wheels", "locked-tool==2.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: Hash mismatch for `idna==3.3`

    Expected:
      sha256:0000000000000000000000000000000000000000000000000000000000000000

    Computed:
      sha256:84d9dd047ffa80596e0f246e2eab0b391788b0503584e8945f2368256d2735ff
    ");

    lock["packages"][0]["wheels"][0]["hashes"] = toml::Value::Table(toml::Table::new());
    tool(&context, "3.0.0", Some(&toml::to_string(&lock)?), None)?;
    uv_snapshot!(context.filters(), context.tool_run()
        .args(["--locked", "--preview-features", "locked-tools", "--find-links", "wheels", "locked-tool==3.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    warning: Empty hash tables in `pylock.toml` will be rejected in a future uv version. Rerun the original `uv export` or `uv pip compile` command to regenerate the file.
    error: The packaged lock for `locked-tool==3.0.0` is missing artifact hashes; regenerate the lock before publishing the package
    ");
    Ok(())
}

#[tokio::test]
#[cfg(feature = "test-pypi")]
async fn packaged_lock_index_cache() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_filtered_exe_suffix()
        .with_tool_dirs();
    context.temp_dir.child("wheels").create_dir_all()?;
    let server = MockServer::start().await;
    let index = format!("{}/simple", server.uri());
    let (filename, private_wheel) = generate_wheel_with_files(
        &"idna".parse()?,
        &"3.3".parse()?,
        &[],
        &BTreeMap::new(),
        None,
        "py3-none-any",
        &[("idna/private.py", "")],
    );
    let simple = json!({
        "meta": { "api-version": "1.1" },
        "name": "idna",
        "files": [{
            "filename": filename,
            "url": format!("{}/files/{filename}", server.uri()),
            "hashes": {},
            "core-metadata": true,
            "upload-time": "2024-03-24T00:00:00Z"
        }]
    });
    Mock::given(method("GET"))
        .and(path("/simple/idna/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(simple.to_string(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}.metadata")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("Metadata-Version: 2.1\nName: idna\nVersion: 3.3\n\n"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(private_wheel))
        .mount(&server)
        .await;
    let mut lock: toml::Value = toml::from_str(&dependency(&context, "3.3")?)?;
    lock["packages"][0]
        .as_table_mut()
        .context("Expected a package table")?
        .insert("index".to_owned(), toml::Value::String(index.clone()));
    tool(&context, "1.0.0", Some(&toml::to_string(&lock)?), None)?;

    uv_snapshot!(context.filters(), context.tool_run()
        .args(["--locked", "--preview-features", "locked-tools", "--find-links", "wheels", "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The packaged lock for `locked-tool==1.0.0` contains unverified artifacts
      cause: Index `http://[LOCALHOST]/simple` from the packaged lock is not configured
    ");

    context
        .pip_install()
        .args(["--index-url", &index, "idna==3.3"])
        .assert()
        .success();
    uv_snapshot!(context.python_command()
        .args(["-c", "import importlib.util; print('private' if importlib.util.find_spec('idna.private') else 'pypi')"]), @"
    exit_code: 0 (success)
    ----- stdout -----
    private
    ");
    Ok(())
}

#[test]
#[cfg(feature = "test-pypi")]
fn packaged_lock_artifact_urls() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_filtered_exe_suffix()
        .with_tool_dirs();
    context.temp_dir.child("wheels").create_dir_all()?;
    let lock = dependency(&context, "3.3")?;

    let mut redirected: toml::Value = toml::from_str(&lock)?;
    redirected["packages"][0]["wheels"][0]["url"] =
        toml::Value::String("https://example.invalid/idna-3.3-py3-none-any.whl".to_owned());
    // An index named in the lock must also be configured by the user.
    redirected["packages"][0]
        .as_table_mut()
        .context("Expected a package table")?
        .insert(
            "index".to_owned(),
            toml::Value::String("https://example.invalid/simple".to_owned()),
        );
    tool(
        &context,
        "1.0.0",
        Some(&toml::to_string(&redirected)?),
        None,
    )?;
    uv_snapshot!(context.filters(), context.tool_install()
        .args(["--locked", "--preview-features", "locked-tools", "--find-links", "wheels", "locked-tool==1.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The packaged lock for `locked-tool==1.0.0` contains unverified artifacts
      cause: Index `https://example.invalid/simple` from the packaged lock is not configured
    ");

    let mut wrong_path: toml::Value = toml::from_str(&lock)?;
    wrong_path["packages"][0]["wheels"][0]["url"] = toml::Value::String(
        "https://files.pythonhosted.org/packages/invalid/idna-3.3-py3-none-any.whl".to_owned(),
    );
    tool(
        &context,
        "2.0.0",
        Some(&toml::to_string(&wrong_path)?),
        None,
    )?;
    uv_snapshot!(context.filters(), context.tool_run()
        .args(["--locked", "--preview-features", "locked-tools", "--find-links", "wheels", "locked-tool==2.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The packaged lock for `locked-tool==2.0.0` contains unverified artifacts
      cause: URL for `idna==3.3` is not listed by https://pypi.org/simple: https://files.pythonhosted.org/packages/invalid/idna-3.3-py3-none-any.whl
    ");

    let mut unselected: toml::Value = toml::from_str(&lock)?;
    unselected["packages"][0]["sdist"]["url"] =
        toml::Value::String("https://example.invalid/idna-3.3.tar.gz".to_owned());
    tool(
        &context,
        "3.0.0",
        Some(&toml::to_string(&unselected)?),
        None,
    )?;
    uv_snapshot!(context.filters(), context.tool_install()
        .args(["--locked", "--preview-features", "locked-tools", "--find-links", "wheels", "locked-tool==3.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The packaged lock for `locked-tool==3.0.0` contains unverified artifacts
      cause: URL for `idna==3.3` is not listed by https://pypi.org/simple: https://example.invalid/idna-3.3.tar.gz
    ");

    let newer_lock: toml::Value = toml::from_str(&dependency(&context, "3.4")?)?;
    let mut wrong_version: toml::Value = toml::from_str(&lock)?;
    wrong_version["packages"][0]["sdist"]["url"] =
        newer_lock["packages"][0]["sdist"]["url"].clone();
    tool(
        &context,
        "4.0.0",
        Some(&toml::to_string(&wrong_version)?),
        None,
    )?;
    uv_snapshot!(context.filters(), context.tool_install()
        .args(["--locked", "--preview-features", "locked-tools", "--find-links", "wheels", "locked-tool==4.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The packaged lock for `locked-tool==4.0.0` contains unverified artifacts
      cause: URL for `idna==3.3` is not listed by https://pypi.org/simple: https://files.pythonhosted.org/packages/8b/e1/43beb3d38dba6cb420cefa297822eac205a277ab43e5ba5d5c46faf96438/idna-3.4.tar.gz
    ");

    tool(&context, "5.0.0", Some(&lock), None)?;
    uv_snapshot!(context.filters(), context.tool_install()
        .args(["--locked", "--preview-features", "locked-tools", "--no-index", "--find-links", "wheels", "locked-tool==5.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The packaged lock for `locked-tool==5.0.0` contains unverified artifacts
      cause: Index `https://pypi.org/simple` from the packaged lock is not configured
    ");
    Ok(())
}

#[test]
fn packaged_lock_non_registry_sources() -> Result<()> {
    // None of these sources may be accessed while validating a packaged lock.
    for source in [
        r#"archive = { url = "https://example.invalid/idna-3.3.tar.gz", hashes = { sha256 = "0000000000000000000000000000000000000000000000000000000000000000" } }"#,
        r#"directory = { path = "dependency" }"#,
        r#"vcs = { type = "git", url = "https://example.invalid/idna", commit-id = "0123456789012345678901234567890123456789" }"#,
        r#"wheels = [{ url = "https://files.pythonhosted.org/idna-3.3-py3-none-any.whl", path = "idna-3.3-py3-none-any.whl", hashes = { sha256 = "0000000000000000000000000000000000000000000000000000000000000000" } }]"#,
        r#"sdist = { path = "idna-3.3.tar.gz", hashes = { sha256 = "0000000000000000000000000000000000000000000000000000000000000000" } }"#,
    ] {
        let context = uv_test::test_context!("3.12").with_tool_dirs();
        context.temp_dir.child("wheels").create_dir_all()?;
        let lock = formatdoc! {r#"
            lock-version = "1.0"
            created-by = "test"
            [[packages]]
            name = "idna"
            version = "3.3"
            {source}
        "#};
        tool(&context, "1.0.0", Some(&lock), None)?;
        allow_duplicates! {
            uv_snapshot!(context.filters(), context.tool_install()
                .args(["--locked", "--preview-features", "locked-tools", "--no-index", "--find-links", "wheels", "locked-tool==1.0.0"]), @"
            exit_code: 2 (failure)
            ----- stderr -----
            error: The packaged lock for `locked-tool==1.0.0` contains unverified artifacts
              cause: `idna==3.3` must use wheel or source distribution URLs from an index
            ");
        }
    }
    Ok(())
}

#[test]
#[cfg(feature = "test-pypi")]
fn packaged_lock_from_build() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_filtered_exe_suffix()
        .with_tool_dirs();
    context.temp_dir.child("src/locked_tool").create_dir_all()?;
    context
        .temp_dir
        .child("src/locked_tool/__init__.py")
        .write_str("import idna\ndef main(): print(idna.__version__)\n")?;
    context
        .temp_dir
        .child("pyproject.toml")
        .write_str(indoc! {r#"
        [project]
        name = "locked-tool"
        version = "1.0.0"
        requires-python = ">=3.12"
        dependencies = ["idna>=3.4"]
        [project.scripts]
        locked-tool = "locked_tool:main"
        [build-system]
        requires = ["uv_build>=0.5.15,<2"]
        build-backend = "uv_build"
        [tool.uv]
        resolution = "lowest-direct"
    "#})?;
    context.lock().assert().success();
    context
        .build()
        .env(EnvVars::UV_EXPORT_LOCK, "true")
        .args(["--preview-features", "locked-tools", "--wheel"])
        .assert()
        .success();
    let bin_dir = context.temp_dir.child("bin");
    uv_snapshot!(context.filters(), context.tool_install()
        .args(["--locked", "--preview-features", "locked-tools", "--from", "./dist/locked_tool-1.0.0-py3-none-any.whl", "locked-tool"])
        .env(EnvVars::PATH, bin_dir.as_os_str()), @"
    exit_code: 0 (success)
    ----- stderr -----
    Installed [N] packages in [TIME]
     + idna==3.4
     + locked-tool==1.0.0 (from file://[TEMP_DIR]/dist/locked_tool-1.0.0-py3-none-any.whl)
    Installed 1 executable: locked-tool
    ");
    uv_snapshot!(context.filters(), context.tool_run()
        .args(["--locked", "--preview-features", "locked-tools", "--offline", "--from", "./dist/locked_tool-1.0.0-py3-none-any.whl", "locked-tool"]), @"
    exit_code: 0 (success)
    ----- stdout -----
    3.4

    ----- stderr -----
    Installed [N] packages in [TIME]
     + idna==3.4
     + locked-tool==1.0.0 (from file://[TEMP_DIR]/dist/locked_tool-1.0.0-py3-none-any.whl)
    ");
    Ok(())
}

async fn mount_locked_artifact(
    server: &MockServer,
    route: &str,
    filename: &str,
    bytes: &[u8],
    index_hash: &str,
    requires_python: Option<&str>,
) -> (String, String) {
    let index = format!("{}/{route}/simple", server.uri());
    // An index may use URLs that do not contain the artifact's filename.
    let url = format!("{}/{route}/artifact", server.uri());
    let mut file = json!({
        "filename": filename,
        "url": url,
        "hashes": { "sha256": index_hash },
    });
    if let Some(requires_python) = requires_python {
        file["requires-python"] = json!(requires_python);
    }
    Mock::given(method("GET"))
        .and(path(format!("/{route}/simple/idna/")))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                json!({
                    "meta": { "api-version": "1.1" },
                    "name": "idna",
                    "files": [file],
                })
                .to_string(),
                "application/vnd.pypi.simple.v1+json",
            ),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/{route}/artifact")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
        .mount(server)
        .await;
    (index, url)
}

fn locked_artifact(index: &str, url: &str, filename: &str, hashes: &str, kind: &str) -> String {
    let artifact = format!(r#"{{ name = "{filename}", url = "{url}", hashes = {{ {hashes} }} }}"#);
    let artifact = if kind == "wheels" {
        format!("[{artifact}]")
    } else {
        artifact
    };
    formatdoc! {r#"
        lock-version = "1.0"
        created-by = "test"
        [[packages]]
        name = "idna"
        version = "3.3"
        index = "{index}"
        {kind} = {artifact}
    "#}
}

#[tokio::test]
async fn packaged_lock_rejects_weak_hashes() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_tool_dirs();
    context.temp_dir.child("wheels").create_dir_all()?;
    let server = MockServer::start().await;
    let (filename, wheel) = generate_wheel_with_files(
        &"idna".parse()?,
        &"3.3".parse()?,
        &[],
        &BTreeMap::new(),
        None,
        "py3-none-any",
        &[],
    );
    let sha256 = hex::encode(Sha256::digest(&wheel));
    let mut hasher = Hasher::from(HashAlgorithm::Md5);
    hasher.update(&wheel);
    let md5 = HashDigest::from(hasher).to_string();
    let md5 = md5.strip_prefix("md5:").context("Expected an MD5 hash")?;
    let (index, url) =
        mount_locked_artifact(&server, "hashes", &filename, &wheel, &sha256, None).await;
    let context = context.with_filter((sha256.clone(), "[SHA256]"));
    let weak = locked_artifact(
        &index,
        &url,
        &filename,
        &format!("md5 = \"{md5}\""),
        "wheels",
    );
    let mixed = locked_artifact(
        &index,
        &url,
        &filename,
        &format!("md5 = \"{md5}\", sha256 = \"{}\"", "0".repeat(64)),
        "wheels",
    );
    let strong = locked_artifact(
        &index,
        &url,
        &filename,
        &format!("md5 = \"{md5}\", sha256 = \"{sha256}\""),
        "wheels",
    );
    tool(&context, "1.0.0", Some(&weak), None)?;
    tool(&context, "2.0.0", Some(&mixed), None)?;
    tool(&context, "3.0.0", Some(&strong), None)?;
    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "--isolated", "--preview-features", "locked-tools", "--find-links", "wheels", "--index-url", &index, "locked-tool==1.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The packaged lock for `locked-tool==1.0.0` has no secure hash for `idna==3.3`
    ");
    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "--isolated", "--preview-features", "locked-tools", "--find-links", "wheels", "--index-url", &index, "locked-tool==2.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: Hash mismatch for `idna==3.3`

    Expected:
      sha256:0000000000000000000000000000000000000000000000000000000000000000

    Computed:
      sha256:[SHA256]
    ");
    context
        .tool_run()
        .args([
            "--locked",
            "--isolated",
            "--preview-features",
            "locked-tools",
            "--find-links",
            "wheels",
            "--index-url",
            &index,
            "locked-tool==3.0.0",
        ])
        .assert()
        .success();
    Ok(())
}

#[tokio::test]
async fn packaged_lock_checks_index_filename() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_tool_dirs();
    context.temp_dir.child("wheels").create_dir_all()?;
    let server = MockServer::start().await;
    let (filename, wheel) = generate_wheel_with_files(
        &"idna".parse()?,
        &"3.3".parse()?,
        &[],
        &BTreeMap::new(),
        None,
        "cp313-cp313-win_amd64",
        &[],
    );
    let hash = hex::encode(Sha256::digest(&wheel));
    let (index, url) =
        mount_locked_artifact(&server, "filename", &filename, &wheel, &hash, None).await;
    let lock = locked_artifact(
        &index,
        &url,
        "idna-3.3-py3-none-any.whl",
        &format!("sha256 = \"{hash}\""),
        "wheels",
    );
    tool(&context, "1.0.0", Some(&lock), None)?;
    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "--isolated", "--preview-features", "locked-tools", "--find-links", "wheels", "--index-url", &index, "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The packaged lock for `locked-tool==1.0.0` contains unverified artifacts
      cause: Filename `idna-3.3-py3-none-any.whl` for `idna==3.3` does not match the file listed at http://[LOCALHOST]/filename/artifact by http://[LOCALHOST]/filename/simple
    ");
    assert!(
        !server
            .received_requests()
            .await
            .context("request recording disabled")?
            .iter()
            .any(|request| request.url.path() == "/filename/artifact")
    );
    let sdist = generate_source_archive(&"idna".parse()?, &"3.3".parse()?, "", None)?;
    let hash = hex::encode(Sha256::digest(&sdist));
    let (index, url) = mount_locked_artifact(
        &server,
        "source-name",
        "idna-3.3.tar.gz",
        &sdist,
        &hash,
        None,
    )
    .await;
    let lock = locked_artifact(
        &index,
        &url,
        "idna-3.3.zip",
        &format!("sha256 = \"{hash}\""),
        "sdist",
    );
    tool(&context, "2.0.0", Some(&lock), None)?;
    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "--isolated", "--preview-features", "locked-tools", "--find-links", "wheels", "--index-url", &index, "locked-tool==2.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The packaged lock for `locked-tool==2.0.0` contains unverified artifacts
      cause: Filename `idna-3.3.zip` for `idna==3.3` does not match the file listed at http://[LOCALHOST]/source-name/artifact by http://[LOCALHOST]/source-name/simple
    ");
    assert!(
        !server
            .received_requests()
            .await
            .context("request recording disabled")?
            .iter()
            .any(|request| request.url.path() == "/source-name/artifact")
    );
    Ok(())
}

#[tokio::test]
async fn packaged_lock_checks_dependency_python() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_tool_dirs();
    context.temp_dir.child("wheels").create_dir_all()?;
    let server = MockServer::start().await;
    let (filename, wheel) = generate_wheel_with_files(
        &"idna".parse()?,
        &"3.3".parse()?,
        &[],
        &BTreeMap::new(),
        Some(&">=99".parse()?),
        "py3-none-any",
        &[],
    );
    let hash = hex::encode(Sha256::digest(&wheel));
    let (indexed, indexed_url) =
        mount_locked_artifact(&server, "indexed", &filename, &wheel, &hash, Some(">=99")).await;
    let (unindexed, unindexed_url) =
        mount_locked_artifact(&server, "unindexed", &filename, &wheel, &hash, None).await;
    let hashes = format!("sha256 = \"{hash}\"");
    tool(
        &context,
        "1.0.0",
        Some(&locked_artifact(
            &indexed,
            &indexed_url,
            &filename,
            &hashes,
            "wheels",
        )),
        None,
    )?;
    tool(
        &context,
        "2.0.0",
        Some(&locked_artifact(
            &unindexed,
            &unindexed_url,
            &filename,
            &hashes,
            "wheels",
        )),
        None,
    )?;
    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "--isolated", "--preview-features", "locked-tools", "--find-links", "wheels", "--index-url", &indexed, "locked-tool==1.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `idna==3.3` requires Python `>=99`, but the selected interpreter is Python 3.12.[X]
    ");
    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "--isolated", "--preview-features", "locked-tools", "--find-links", "wheels", "--index-url", &unindexed, "locked-tool==2.0.0"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: `idna==3.3` requires Python `>=99`, but the selected interpreter is Python 3.12.[X]
    ");
    Ok(())
}

#[tokio::test]
async fn packaged_lock_checks_index_hash() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_tool_dirs();
    context.temp_dir.child("wheels").create_dir_all()?;
    let server = MockServer::start().await;
    let (filename, wheel) = generate_wheel_with_files(
        &"idna".parse()?,
        &"3.3".parse()?,
        &[],
        &BTreeMap::new(),
        None,
        "py3-none-any",
        &[],
    );
    let hash = hex::encode(Sha256::digest(&wheel));
    let (index, url) = mount_locked_artifact(
        &server,
        "index-hash",
        &filename,
        &wheel,
        &"0".repeat(64),
        None,
    )
    .await;
    let lock = locked_artifact(
        &index,
        &url,
        &filename,
        &format!("sha256 = \"{hash}\""),
        "wheels",
    );
    tool(&context, "1.0.0", Some(&lock), None)?;
    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "--isolated", "--preview-features", "locked-tools", "--find-links", "wheels", "--index-url", &index, "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: The selected artifact for `idna==3.3` does not match the required hashes
    ");
    Ok(())
}

#[tokio::test]
async fn packaged_lock_checks_sdist_constraints_before_building() -> Result<()> {
    let context = uv_test::test_context!("3.12")
        .with_filtered_counts()
        .with_tool_dirs();
    context.temp_dir.child("wheels").create_dir_all()?;
    let marker = context.temp_dir.child("backend-executed");
    let sdist =
        generate_source_archive(&"idna".parse()?, &"3.3".parse()?, "", Some(marker.path()))?;
    let hash = hex::encode(Sha256::digest(&sdist));
    let lock_hash = hex::encode(Sha512::digest(&sdist));
    let constraint_hash = hex::encode(Sha384::digest(&sdist));
    let context = context
        .with_filter((hash.clone(), "[INDEX_HASH]"))
        .with_filter((lock_hash.clone(), "[LOCK_HASH]"))
        .with_filter((constraint_hash.clone(), "[CONSTRAINT_HASH]"));
    let server = MockServer::start().await;
    let filename = "idna-3.3.tar.gz";
    let (index, url) = mount_locked_artifact(&server, "sdist", filename, &sdist, &hash, None).await;
    let lock = locked_artifact(
        &index,
        &url,
        filename,
        &format!("sha512 = \"{lock_hash}\""),
        "sdist",
    );
    tool(&context, "1.0.0", Some(&lock), None)?;
    context
        .temp_dir
        .child("constraints.txt")
        .write_str(&format!("idna==3.3 --hash=sha384:{}\n", "0".repeat(96)))?;
    uv_snapshot!(context.filters(), context.tool_run().args(["--locked", "--isolated", "--preview-features", "locked-tools", "--find-links", "wheels", "--index-url", &index, "--constraint", "constraints.txt", "locked-tool"]), @"
    exit_code: 2 (failure)
    ----- stderr -----
    error: Hash mismatch for `idna==3.3`

    Expected:
      sha512:[LOCK_HASH]
      sha384:000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000
      sha256:[INDEX_HASH]

    Computed:
      sha256:[INDEX_HASH]
      sha384:[CONSTRAINT_HASH]
      sha512:[LOCK_HASH]
    ");
    assert!(!marker.path().exists());
    let mut alternatives = String::new();
    for value in 0..130 {
        write!(&mut alternatives, " --hash=sha384:{value:096x}")?;
    }
    context
        .temp_dir
        .child("constraints.txt")
        .write_str(&format!(
            "idna==3.3{alternatives} --hash=sha384:{constraint_hash}\n"
        ))?;
    let requests_before = server
        .received_requests()
        .await
        .context("request recording disabled")?
        .iter()
        .filter(|request| request.url.path() == "/sdist/artifact")
        .count();
    context
        .tool_run()
        .args([
            "--locked",
            "--isolated",
            "--preview-features",
            "locked-tools",
            "--find-links",
            "wheels",
            "--index-url",
            &index,
            "--constraint",
            "constraints.txt",
            "locked-tool",
        ])
        .assert()
        .success();
    assert!(marker.path().exists());
    let requests_after = server
        .received_requests()
        .await
        .context("request recording disabled")?
        .iter()
        .filter(|request| request.url.path() == "/sdist/artifact")
        .count();
    assert_eq!(requests_after - requests_before, 1);
    fs_err::remove_file(marker.path())?;
    context
        .temp_dir
        .child("constraints.txt")
        .write_str(&format!(
            "idna==3.3 --hash=sha384:{constraint_hash} --hash=md5:{}\n",
            "0".repeat(32)
        ))?;
    context
        .tool_run()
        .args([
            "--locked",
            "--isolated",
            "--offline",
            "--preview-features",
            "locked-tools",
            "--find-links",
            "wheels",
            "--index-url",
            &index,
            "--constraint",
            "constraints.txt",
            "locked-tool",
        ])
        .assert()
        .success();
    assert!(!marker.path().exists());
    Ok(())
}

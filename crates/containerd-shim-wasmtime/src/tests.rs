use std::time::Duration;

use containerd_shim_wasm::container::Instance;
use containerd_shim_wasm::testing::modules::*;
use containerd_shim_wasm::testing::{oci_helpers, WasiTest};
use serial_test::serial;
use wasmtime::Config;
use WasmtimeTestInstance as WasiInstance;

use crate::instance::{WasiConfig, WasmtimeEngine};

// use test configuration to avoid dead locks when running tests
// https://github.com/containerd/runwasi/issues/357
type WasmtimeTestInstance = Instance<WasmtimeEngine<WasiTestConfig>>;

#[derive(Clone)]
struct WasiTestConfig {}

impl WasiConfig for WasiTestConfig {
    fn new_config() -> Config {
        let mut config = wasmtime::Config::new();
        // Disable Wasmtime parallel compilation for the tests
        // see https://github.com/containerd/runwasi/pull/405#issuecomment-1928468714 for details
        config.parallel_compilation(false);
        config.wasm_component_model(true); // enable component linking
        config
    }
}

#[test]
#[serial]
fn test_delete_after_create() -> anyhow::Result<()> {
    WasiTest::<WasiInstance>::builder()?.build()?.delete()?;
    Ok(())
}

#[test]
#[serial]
fn test_hello_world() -> anyhow::Result<()> {
    let (exit_code, stdout, _) = WasiTest::<WasiInstance>::builder()?
        .with_wasm(HELLO_WORLD)?
        .build()?
        .start()?
        .wait(Duration::from_secs(10))?;

    assert_eq!(exit_code, 0);
    assert_eq!(stdout, "hello world\n");

    Ok(())
}

#[test]
#[serial]
fn test_hello_world_oci() -> anyhow::Result<()> {
    let (builder, _oci_cleanup) = WasiTest::<WasiInstance>::builder()?
        .with_wasm(HELLO_WORLD)?
        .as_oci_image(None, None)?;

    let (exit_code, stdout, _) = builder.build()?.start()?.wait(Duration::from_secs(10))?;

    assert_eq!(exit_code, 0);
    assert_eq!(stdout, "hello world\n");

    Ok(())
}

#[test]
#[serial]
fn test_hello_world_oci_uses_precompiled() -> anyhow::Result<()> {
    let (builder, _oci_cleanup1) = WasiTest::<WasiInstance>::builder()?
        .with_wasm(HELLO_WORLD)?
        .as_oci_image(None, Some("c1".to_string()))?;

    let (exit_code, stdout, _) = builder.build()?.start()?.wait(Duration::from_secs(10))?;

    assert_eq!(exit_code, 0);
    assert_eq!(stdout, "hello world\n");

    let (label, id) = oci_helpers::get_image_label()?;
    assert!(
        label.starts_with("runwasi.io/precompiled/wasmtime/"),
        "was {}={}",
        label,
        id
    );

    std::thread::sleep(std::time::Duration::from_secs(30));

    // run second time, it should succeed without recompiling
    let (builder, _oci_cleanup2) = WasiTest::<WasiInstance>::builder()?
        .with_wasm(HELLO_WORLD)?
        .as_oci_image(None, Some("c2".to_string()))?;

    let (exit_code, stdout, _) = builder.build()?.start()?.wait(Duration::from_secs(10))?;

    assert_eq!(exit_code, 0);
    assert_eq!(stdout, "hello world\n");

    Ok(())
}

#[test]
#[serial]
fn test_hello_world_oci_uses_precompiled_when_content_removed() -> anyhow::Result<()> {
    let (builder, _oci_cleanup1) = WasiTest::<WasiInstance>::builder()?
        .with_wasm(HELLO_WORLD)?
        .as_oci_image(None, Some("c1".to_string()))?;

    let (exit_code, stdout, _) = builder.build()?.start()?.wait(Duration::from_secs(10))?;

    assert_eq!(exit_code, 0);
    assert_eq!(stdout, "hello world\n");

    let (label, id) = oci_helpers::get_image_label()?;

    // remove the compiled content from the cache
    assert!(label.starts_with("runwasi.io/precompiled/wasmtime/"));
    oci_helpers::remove_content(id)?;

    // run second time, it should succeed
    let (builder, _oci_cleanup2) = WasiTest::<WasiInstance>::builder()?
        .with_wasm(HELLO_WORLD)?
        .as_oci_image(None, Some("c2".to_string()))?;

    let (exit_code, stdout, _) = builder.build()?.start()?.wait(Duration::from_secs(10))?;

    assert_eq!(exit_code, 0);
    assert_eq!(stdout, "hello world\n");

    Ok(())
}

#[test]
#[serial]
fn test_custom_entrypoint() -> anyhow::Result<()> {
    let (exit_code, stdout, _) = WasiTest::<WasiInstance>::builder()?
        .with_start_fn("foo")?
        .with_wasm(CUSTOM_ENTRYPOINT)?
        .build()?
        .start()?
        .wait(Duration::from_secs(10))?;

    assert_eq!(exit_code, 0);
    assert_eq!(stdout, "hello world\n");

    Ok(())
}

#[test]
#[serial]
fn test_unreachable() -> anyhow::Result<()> {
    let (exit_code, _, _) = WasiTest::<WasiInstance>::builder()?
        .with_wasm(UNREACHABLE)?
        .build()?
        .start()?
        .wait(Duration::from_secs(10))?;

    assert_ne!(exit_code, 0);

    Ok(())
}

#[test]
#[serial]
fn test_exit_code() -> anyhow::Result<()> {
    let (exit_code, _, _) = WasiTest::<WasiInstance>::builder()?
        .with_wasm(EXIT_CODE)?
        .build()?
        .start()?
        .wait(Duration::from_secs(10))?;

    assert_eq!(exit_code, 42);

    Ok(())
}

#[test]
#[serial]
fn test_seccomp() -> anyhow::Result<()> {
    let (exit_code, stdout, _) = WasiTest::<WasiInstance>::builder()?
        .with_wasm(SECCOMP)?
        .build()?
        .start()?
        .wait(Duration::from_secs(10))?;

    assert_eq!(exit_code, 0);
    assert_eq!(stdout.trim(), "current working dir: /");

    Ok(())
}

#[test]
#[serial]
fn test_has_default_devices() -> anyhow::Result<()> {
    let (exit_code, _, _) = WasiTest::<WasiInstance>::builder()?
        .with_wasm(HAS_DEFAULT_DEVICES)?
        .build()?
        .start()?
        .wait(Duration::from_secs(10))?;

    assert_eq!(exit_code, 0);

    Ok(())
}

// Test that the shim can execute an named exported function
// that is not the default _start function in a wasm component.
// The current limitation is that there is no way to pass arguments
// to the exported function.
// Issue that tracks this: https://github.com/containerd/runwasi/issues/414
#[test]
#[serial]
fn test_simple_component() -> anyhow::Result<()> {
    let (exit_code, _, _) = WasiTest::<WasiInstance>::builder()?
        .with_wasm(SIMPLE_COMPONENT)?
        .with_start_fn("thunk")?
        .build()?
        .start()?
        .wait(Duration::from_secs(10))?;

    assert_eq!(exit_code, 0);

    Ok(())
}

// Test that the shim can execute a wasm component that is
// compiled with wasip2.
//
// This is using the `wasi:cli/command` world to run the component.
//
// The wasm component is built and copied over from
// https://github.com/Mossaka/wasm-component-hello-world. See
// README.md for how to build the component.
#[test]
#[serial]
fn test_wasip2_component() -> anyhow::Result<()> {
    let (exit_code, stdout, _) = WasiTest::<WasiInstance>::builder()?
        .with_wasm(COMPONENT_HELLO_WORLD)?
        .build()?
        .start()?
        .wait(Duration::from_secs(10))?;

    assert_eq!(exit_code, 0);
    assert_eq!(stdout, "Hello, world!\n");

    Ok(())
}

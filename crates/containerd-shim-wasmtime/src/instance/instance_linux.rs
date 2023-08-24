use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use containerd_shim_wasm::libcontainer_instance::{LibcontainerInstance, LinuxContainerExecutor};
use containerd_shim_wasm::sandbox::error::Error;
use containerd_shim_wasm::sandbox::instance::ExitCode;
use containerd_shim_wasm::sandbox::instance_utils::determine_rootdir;
use containerd_shim_wasm::sandbox::stdio::Stdio;
use containerd_shim_wasm::sandbox::InstanceConfig;
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::container::Container;
use libcontainer::syscall::syscall::create_syscall;

use crate::executor::WasmtimeExecutor;

static DEFAULT_CONTAINER_ROOT_DIR: &str = "/run/containerd/wasmtime";

pub struct Wasi {
    exit_code: ExitCode,
    engine: wasmtime::Engine,
    stdio: Stdio,
    bundle: String,
    rootdir: PathBuf,
    id: String,
}

impl LibcontainerInstance for Wasi {
    type Engine = wasmtime::Engine;

    fn new_libcontainer(id: String, cfg: Option<&InstanceConfig<Self::Engine>>) -> Self {
        // TODO: there are failure cases e.x. parsing cfg, loading spec, etc.
        // thus should make `new` return `Result<Self, Error>` instead of `Self`
        log::info!("creating new instance: {}", id);
        let cfg = cfg.unwrap();
        let bundle = cfg.get_bundle().unwrap_or_default();
        let rootdir = determine_rootdir(
            bundle.as_str(),
            &cfg.get_namespace(),
            DEFAULT_CONTAINER_ROOT_DIR,
        )
        .unwrap();
        Wasi {
            id,
            exit_code: Arc::new((Mutex::new(None), Condvar::new())),
            engine: cfg.get_engine(),
            stdio: Stdio {
                stdin: cfg.get_stdin().try_into().unwrap(),
                stdout: cfg.get_stdout().try_into().unwrap(),
                stderr: cfg.get_stderr().try_into().unwrap(),
            },
            bundle,
            rootdir,
        }
    }

    fn get_exit_code(&self) -> ExitCode {
        self.exit_code.clone()
    }

    fn get_id(&self) -> String {
        self.id.clone()
    }

    fn get_root_dir(&self) -> std::result::Result<PathBuf, Error> {
        Ok(self.rootdir.clone())
    }

    fn build_container(&self) -> std::result::Result<Container, Error> {
        let engine = self.engine.clone();
        let syscall = create_syscall();
        self.stdio.redirect()?;
        let err_others = |err| Error::Others(format!("failed to create container: {}", err));

        let wasmtime_executor = Box::new(WasmtimeExecutor::new(self.stdio.clone(), engine));
        let default_executor = Box::new(LinuxContainerExecutor::new(self.stdio.clone()));

        let container = ContainerBuilder::new(self.id.clone(), syscall.as_ref())
            .with_executor(vec![default_executor, wasmtime_executor])
            .map_err(err_others)?
            .with_root_path(self.rootdir.clone())
            .map_err(err_others)?
            .as_init(&self.bundle)
            .with_systemd(false)
            .build()
            .map_err(err_others)?;

        Ok(container)
    }
}

#[cfg(test)]
mod wasitest {
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::fs::{create_dir, read_to_string, File, OpenOptions};
    use std::io::prelude::*;
    use std::os::fd::RawFd;
    use std::os::unix::prelude::OpenOptionsExt;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use containerd_shim_wasm::function;
    use containerd_shim_wasm::sandbox::instance::Wait;
    use containerd_shim_wasm::sandbox::testutil::{has_cap_sys_admin, run_test_with_sudo};
    use containerd_shim_wasm::sandbox::Instance;
    use libc::{SIGKILL, STDERR_FILENO, STDIN_FILENO, STDOUT_FILENO};
    use nix::unistd::dup2;
    use oci_spec::runtime::{ProcessBuilder, RootBuilder, SpecBuilder};
    use tempfile::{tempdir, TempDir};

    use super::*;

    static mut STDIN_FD: Option<RawFd> = None;
    static mut STDOUT_FD: Option<RawFd> = None;
    static mut STDERR_FD: Option<RawFd> = None;

    fn reset_stdio() {
        unsafe {
            if let Some(stdin) = STDIN_FD {
                let _ = dup2(stdin, STDIN_FILENO);
            }
            if let Some(stdout) = STDOUT_FD {
                let _ = dup2(stdout, STDOUT_FILENO);
            }
            if let Some(stderr) = STDERR_FD {
                let _ = dup2(stderr, STDERR_FILENO);
            }
        }
    }

    // This is taken from https://github.com/bytecodealliance/wasmtime/blob/6a60e8363f50b936e4c4fc958cb9742314ff09f3/docs/WASI-tutorial.md?plain=1#L270-L298
    fn hello_world_module(start_fn: Option<&str>) -> Vec<u8> {
        let start_fn = start_fn.unwrap_or("_start");
        format!(r#"(module
            ;; Import the required fd_write WASI function which will write the given io vectors to stdout
            ;; The function signature for fd_write is:
            ;; (File Descriptor, *iovs, iovs_len, nwritten) -> Returns number of bytes written
            (import "wasi_unstable" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
    
            (memory 1)
            (export "memory" (memory 0))
    
            ;; Write 'hello world\n' to memory at an offset of 8 bytes
            ;; Note the trailing newline which is required for the text to appear
            (data (i32.const 8) "hello world\n")
    
            (func $main (export "{start_fn}")
                ;; Creating a new io vector within linear memory
                (i32.store (i32.const 0) (i32.const 8))  ;; iov.iov_base - This is a pointer to the start of the 'hello world\n' string
                (i32.store (i32.const 4) (i32.const 12))  ;; iov.iov_len - The length of the 'hello world\n' string
    
                (call $fd_write
                    (i32.const 1) ;; file_descriptor - 1 for stdout
                    (i32.const 0) ;; *iovs - The pointer to the iov array, which is stored at memory location 0
                    (i32.const 1) ;; iovs_len - We're printing 1 string stored in an iov - so one.
                    (i32.const 20) ;; nwritten - A place in memory to store the number of bytes written
                )
                drop ;; Discard the number of bytes written from the top of the stack
            )
        )
        "#).as_bytes().to_vec()
    }

    #[test]
    fn test_delete_after_create() -> anyhow::Result<()> {
        let cfg = InstanceConfig::new(
            Default::default(),
            "test_namespace".into(),
            "/containerd/address".into(),
        );

        let i = Wasi::new("".to_string(), Some(&cfg));
        i.delete()?;
        reset_stdio();
        Ok(())
    }

    #[test]
    fn test_wasi_entrypoint() -> Result<(), Error> {
        if !has_cap_sys_admin() {
            println!("running test with sudo: {}", function!());
            return run_test_with_sudo(function!());
        }
        // start logging
        // to enable logging run `export RUST_LOG=trace` and append cargo command with
        // --show-output before running test
        let _ = env_logger::try_init();

        let dir = tempdir()?;
        let path = dir.path();
        let wasm_bytes = hello_world_module(None);

        let res = run_wasi_test(&dir, wasm_bytes.into(), None)?;

        assert_eq!(res.0, 0);

        let output = read_to_string(path.join("stdout"))?;
        assert_eq!(output, "hello world\n");

        reset_stdio();
        Ok(())
    }

    // ignore until https://github.com/containerd/runwasi/issues/194 is resolved
    #[test]
    #[ignore]
    fn test_wasi_custom_entrypoint() -> Result<(), Error> {
        if !has_cap_sys_admin() {
            println!("running test with sudo: {}", function!());
            return run_test_with_sudo(function!());
        }
        // start logging
        let _ = env_logger::try_init();

        let dir = tempdir()?;
        let path = dir.path();
        let wasm_bytes = hello_world_module(Some("foo"));

        let res = run_wasi_test(&dir, wasm_bytes.into(), Some("foo"))?;

        assert_eq!(res.0, 0);

        let output = read_to_string(path.join("stdout"))?;
        assert_eq!(output, "hello world\n");

        reset_stdio();
        Ok(())
    }

    fn run_wasi_test(
        dir: &TempDir,
        wasmbytes: Cow<[u8]>,
        start_fn: Option<&str>,
    ) -> Result<(u32, DateTime<Utc>), Error> {
        create_dir(dir.path().join("rootfs"))?;
        let rootdir = dir.path().join("runwasi");
        create_dir(rootdir)?;
        let rootdir = PathBuf::from("/path/to/root");
        let mut opts = HashMap::new();
        opts.insert("root", rootdir);
        let serialized = serde_json::to_string(&opts)?;
        let opts_file = OpenOptions::new()
            .read(true)
            .create(true)
            .truncate(true)
            .write(true)
            .open(dir.path().join("options.json"))?;
        write!(&opts_file, "{}", serialized)?;

        let wasm_path = dir.path().join("rootfs/hello.wat");
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(wasm_path)?;
        f.write_all(&wasmbytes)?;

        let stdout = File::create(dir.path().join("stdout"))?;
        drop(stdout);

        let entrypoint = match start_fn {
            Some(s) => "./hello.wat#".to_string() + s,
            None => "./hello.wat".to_string(),
        };
        let spec = SpecBuilder::default()
            .root(RootBuilder::default().path("rootfs").build()?)
            .process(
                ProcessBuilder::default()
                    .cwd("/")
                    .args(vec![entrypoint])
                    .build()?,
            )
            .build()?;

        spec.save(dir.path().join("config.json"))?;

        let mut cfg = InstanceConfig::new(
            Default::default(),
            "test_namespace".into(),
            "/containerd/address".into(),
        );
        let cfg = cfg
            .set_bundle(dir.path().to_str().unwrap().to_string())
            .set_stdout(dir.path().join("stdout").to_str().unwrap().to_string());

        let wasi = Wasi::new("test".to_string(), Some(cfg));

        wasi.start()?;

        let (tx, rx) = channel();
        let waiter = Wait::new(tx);
        wasi.wait(&waiter).unwrap();

        let res = match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(res) => Ok(res),
            Err(e) => {
                wasi.kill(SIGKILL as u32).unwrap();
                return Err(Error::Others(format!(
                    "error waiting for module to finish: {0}",
                    e
                )));
            }
        };
        wasi.delete()?;
        res
    }
}
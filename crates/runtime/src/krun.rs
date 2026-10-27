// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! The slice of libkrun 2.0's C API the launcher uses, loaded at run time.
//!
//! The signatures follow `include/libkrun.h` in peerspan/libkrun. libkrun 2.0 is unreleased,
//! so check them against that header whenever the fork is rebased.

use std::convert::Infallible;
use std::ffi::{c_char, c_void};
use std::path::Path;
use std::ptr;

use anyhow::{Context, bail};
use libloading::Library;

type Handle = *mut c_void;
type KrunResult = u64;

const KRUN_RESULT_SUCCESS: KrunResult = 0;
const KRUN_LOG_LEVEL_WARN: u32 = 2;
const KRUN_LOG_STYLE_AUTO: u32 = 0;
const KRUN_KERNEL_FORMAT_ELF: u32 = 0;
const KRUN_PUSH_STR_TYPE_TAG: u32 = 16_777_228;
/// virtio device type of a filesystem.
const VIRTIO_ID_FS: u32 = 26;
/// The guest's vsock context ID; 2 is the host, and lower values are reserved.
const GUEST_CID: u64 = 3;
/// Two queues for virtiofsd: one high-priority queue and one request queue.
const FS_QUEUE_SIZES: [u16; 2] = [1024, 1024];

#[repr(C)]
#[derive(Clone, Copy)]
struct KrunStr {
    data: *const c_char,
    len: usize,
}

impl KrunStr {
    /// Borrows `s` for one call; libkrun copies what it keeps.
    fn new(s: &str) -> Self {
        KrunStr {
            data: s.as_ptr().cast(),
            len: s.len(),
        }
    }
}

#[repr(C)]
struct VtableHandle {
    type_tag: u32,
    metadata: u32,
    vtable_ptr: *const c_void,
    user_data: *const c_void,
    vtable_size: u16,
}

#[repr(C)]
struct PushStrVtable {
    drop: Option<unsafe extern "C" fn(*mut c_void)>,
    push: unsafe extern "C" fn(*mut c_void, KrunStr) -> bool,
}

/// The libkrun functions the launcher calls, resolved from one loaded library.
struct Api {
    init_log: unsafe extern "C" fn(i32, u32, u32, u32, *mut Handle) -> KrunResult,
    mmio_device_manager_new: unsafe extern "C" fn() -> Handle,
    mmio_device_manager_add: unsafe extern "C" fn(Handle, Handle),
    console_device_builder: unsafe extern "C" fn() -> Handle,
    console_builder_add_default_console:
        unsafe extern "C" fn(Handle, i32, i32, i32, *mut Handle) -> KrunResult,
    console_builder_build: unsafe extern "C" fn(Handle, *mut Handle) -> Handle,
    fs_device_new_read_only: unsafe extern "C" fn(KrunStr, KrunStr, *mut Handle) -> Handle,
    vhost_user_device_new:
        unsafe extern "C" fn(u32, KrunStr, KrunStr, u16, *const u16, usize, *mut Handle) -> Handle,
    vsock_device_new: unsafe extern "C" fn(u64, u32, *mut Handle) -> Handle,
    vsock_device_add_unix_port: unsafe extern "C" fn(Handle, u32, KrunStr, bool),
    payload_load_external:
        unsafe extern "C" fn(KrunStr, u32, KrunStr, KrunStr, *mut Handle) -> Handle,
    vmm_builder_new: unsafe extern "C" fn() -> Handle,
    vmm_builder_vcpus: unsafe extern "C" fn(*mut Handle, u8, *mut Handle) -> KrunResult,
    vmm_builder_ram_mib: unsafe extern "C" fn(*mut Handle, u32, *mut Handle) -> KrunResult,
    vmm_builder_payload: unsafe extern "C" fn(*mut Handle, Handle),
    vmm_builder_devices: unsafe extern "C" fn(*mut Handle, Handle),
    vmm_builder_build: unsafe extern "C" fn(*mut Handle, *mut Handle) -> Handle,
    vmm_run: unsafe extern "C" fn(Handle),
    error_message: unsafe extern "C" fn(Handle, *mut c_void),
    error_destroy: unsafe extern "C" fn(Handle),
    _library: Library,
}

/// Everything needed to boot one runtime VM.
#[derive(Debug)]
pub struct VmConfig<'cfg> {
    pub cpus: u8,
    pub memory_mib: u32,
    pub kernel: &'cfg str,
    pub initrd: &'cfg str,
    pub cmdline: &'cfg str,
    pub rootfs_tag: &'cfg str,
    pub rootfs_dir: &'cfg str,
    pub workspace_tag: &'cfg str,
    pub workspace_socket: &'cfg str,
    pub agent_port: u32,
    pub agent_socket: &'cfg str,
}

/// A loaded libkrun.
///
/// Loading happens before the launcher confines itself, because the library's path is
/// outside the jail.
#[derive(Debug)]
pub struct Libkrun {
    api: Api,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api").finish_non_exhaustive()
    }
}

impl Libkrun {
    /// Loads the library and resolves every symbol the launcher uses.
    ///
    /// # Errors
    ///
    /// Fails if the library or a symbol cannot be loaded.
    pub fn load(library: &Path) -> anyhow::Result<Libkrun> {
        Ok(Libkrun {
            api: Api::load(library)?,
        })
    }

    /// Builds the VM and runs it.
    ///
    /// On success this never returns: libkrun runs the VM on this thread and exits the process
    /// when the guest shuts down.
    ///
    /// # Errors
    ///
    /// Fails if libkrun rejects the configuration.
    pub fn boot(
        &self,
        config: &VmConfig<'_>,
        before_run: &mut dyn FnMut(),
    ) -> anyhow::Result<Infallible> {
        let api = &self.api;
        let mut err: Handle = ptr::null_mut();

        // SAFETY: every call below passes handles that libkrun returned and has not consumed,
        // KrunStr values borrowed from strings that outlive the call, and a valid error out-pointer
        // that `check` resets after reading.
        unsafe {
            api.check(
                (api.init_log)(-1, KRUN_LOG_LEVEL_WARN, KRUN_LOG_STYLE_AUTO, 0, &mut err),
                &mut err,
                "init log",
            )?;
            let devices = (api.mmio_device_manager_new)();

            let console = (api.console_device_builder)();
            api.check(
                (api.console_builder_add_default_console)(console, 0, 1, 2, &mut err),
                &mut err,
                "add console",
            )?;
            let console = api.handle(
                (api.console_builder_build)(console, &mut err),
                &mut err,
                "build console",
            )?;
            (api.mmio_device_manager_add)(devices, console);

            let rootfs = (api.fs_device_new_read_only)(
                KrunStr::new(config.rootfs_tag),
                KrunStr::new(config.rootfs_dir),
                &mut err,
            );
            (api.mmio_device_manager_add)(
                devices,
                api.handle(rootfs, &mut err, "root filesystem")?,
            );

            let workspace = (api.vhost_user_device_new)(
                VIRTIO_ID_FS,
                KrunStr::new(config.workspace_socket),
                KrunStr::new(config.workspace_tag),
                u16::try_from(FS_QUEUE_SIZES.len()).expect("two queues"),
                FS_QUEUE_SIZES.as_ptr(),
                FS_QUEUE_SIZES.len(),
                &mut err,
            );
            (api.mmio_device_manager_add)(devices, api.handle(workspace, &mut err, "workspace")?);

            // No TSI: the guest reaches the host only through the mapped Unix socket.
            let vsock = api.handle(
                (api.vsock_device_new)(GUEST_CID, 0, &mut err),
                &mut err,
                "vsock",
            )?;
            (api.vsock_device_add_unix_port)(
                vsock,
                config.agent_port,
                KrunStr::new(config.agent_socket),
                false,
            );
            (api.mmio_device_manager_add)(devices, vsock);

            let payload = (api.payload_load_external)(
                KrunStr::new(config.kernel),
                KRUN_KERNEL_FORMAT_ELF,
                KrunStr::new(config.initrd),
                KrunStr::new(config.cmdline),
                &mut err,
            );
            let payload = api.handle(payload, &mut err, "load kernel")?;

            let mut builder = (api.vmm_builder_new)();
            api.check(
                (api.vmm_builder_vcpus)(&mut builder, config.cpus, &mut err),
                &mut err,
                "set vCPUs",
            )?;
            api.check(
                (api.vmm_builder_ram_mib)(&mut builder, config.memory_mib, &mut err),
                &mut err,
                "set memory",
            )?;
            (api.vmm_builder_payload)(&mut builder, payload);
            (api.vmm_builder_devices)(&mut builder, devices);
            let vmm = api.handle(
                (api.vmm_builder_build)(&mut builder, &mut err),
                &mut err,
                "build VM",
            )?;
            before_run();
            (api.vmm_run)(vmm);
        }
        bail!("libkrun returned from running the VM")
    }
}

impl Api {
    fn load(path: &Path) -> anyhow::Result<Api> {
        // SAFETY: loading libkrun runs no initialisers with preconditions we must uphold.
        let library = unsafe { Library::new(path) }
            .with_context(|| format!("loading libkrun from {}", path.display()))?;
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: the field type this initialises matches the C declaration of $name in
                // libkrun.h, and `library` is kept alive in the returned Api.
                *unsafe { library.get(concat!($name, "\0").as_bytes()) }
                    .with_context(|| format!("libkrun symbol {}", $name))?
            };
        }
        Ok(Api {
            init_log: sym!("krun_init_log"),
            mmio_device_manager_new: sym!("krun_mmio_device_manager_new"),
            mmio_device_manager_add: sym!("krun_mmio_device_manager_add"),
            console_device_builder: sym!("krun_console_device_builder"),
            console_builder_add_default_console: sym!("krun_console_builder_add_default_console"),
            console_builder_build: sym!("krun_console_builder_build"),
            fs_device_new_read_only: sym!("krun_fs_device_new_read_only"),
            vhost_user_device_new: sym!("krun_vhost_user_device_new"),
            vsock_device_new: sym!("krun_vsock_device_new"),
            vsock_device_add_unix_port: sym!("krun_vsock_device_add_unix_port"),
            payload_load_external: sym!("krun_payload_load_external"),
            vmm_builder_new: sym!("krun_vmm_builder_new"),
            vmm_builder_vcpus: sym!("krun_vmm_builder_vcpus"),
            vmm_builder_ram_mib: sym!("krun_vmm_builder_ram_mib"),
            vmm_builder_payload: sym!("krun_vmm_builder_payload"),
            vmm_builder_devices: sym!("krun_vmm_builder_devices"),
            vmm_builder_build: sym!("krun_vmm_builder_build"),
            vmm_run: sym!("krun_vmm_run"),
            error_message: sym!("krun_error_message"),
            error_destroy: sym!("krun_error_destroy"),
            _library: library,
        })
    }

    /// Turns a result code and the error out-parameter into a `Result`.
    ///
    /// # Safety
    ///
    /// `err` must be null or an error handle libkrun just returned.
    unsafe fn check(&self, result: KrunResult, err: &mut Handle, what: &str) -> anyhow::Result<()> {
        // SAFETY: forwarded from this function's contract.
        let message = unsafe { self.take_error(err) };
        match (result, message) {
            (KRUN_RESULT_SUCCESS, None) => Ok(()),
            (_, Some(message)) => bail!("{what}: {message}"),
            (code, None) => bail!("{what}: libkrun returned {code}"),
        }
    }

    /// Returns `handle`, or the error libkrun reported while creating it.
    ///
    /// # Safety
    ///
    /// `err` must be null or an error handle libkrun just returned.
    unsafe fn handle(
        &self,
        handle: Handle,
        err: &mut Handle,
        what: &str,
    ) -> anyhow::Result<Handle> {
        // SAFETY: forwarded from this function's contract.
        if let Some(message) = unsafe { self.take_error(err) } {
            bail!("{what}: {message}");
        }
        if handle.is_null() {
            bail!("{what}: libkrun returned no handle");
        }
        Ok(handle)
    }

    /// Reads, destroys and clears a pending error.
    ///
    /// # Safety
    ///
    /// `err` must be null or an error handle libkrun returned and nothing has destroyed.
    unsafe fn take_error(&self, err: &mut Handle) -> Option<String> {
        if err.is_null() {
            return None;
        }
        let mut message = String::new();
        let vtable = PushStrVtable {
            drop: None,
            push: push_str,
        };
        let writer = VtableHandle {
            type_tag: KRUN_PUSH_STR_TYPE_TAG,
            metadata: 0,
            vtable_ptr: ptr::from_ref(&vtable).cast(),
            user_data: ptr::from_mut(&mut message).cast(),
            vtable_size: u16::try_from(size_of::<PushStrVtable>()).expect("small vtable"),
        };
        // SAFETY: *err is a live error handle; the writer and the String it appends to outlive
        // the call, and the handle is destroyed exactly once and then cleared.
        unsafe {
            (self.error_message)(*err, ptr::from_ref(&writer).cast_mut().cast());
            (self.error_destroy)(*err);
        }
        *err = ptr::null_mut();
        Some(message)
    }
}

unsafe extern "C" fn push_str(user_data: *mut c_void, s: KrunStr) -> bool {
    // SAFETY: user_data is the &mut String set up in take_error, valid for this call, and libkrun
    // passes a valid UTF-8 slice of s.len bytes.
    unsafe {
        let message = &mut *user_data.cast::<String>();
        let bytes = std::slice::from_raw_parts(s.data.cast::<u8>(), s.len);
        message.push_str(&String::from_utf8_lossy(bytes));
    }
    true
}

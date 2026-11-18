//! LS2 (Luna Service) FFI and dedicated glib worker.
//!
//! **Threading contract:** [`LunaWorker::start`] creates one native thread. `LSRegister`, every
//! `LSCall`, and every C callback run only on that thread's glib main loop. The cloneable worker
//! handle is safe to use from Tokio, but merely sends [`Command`]s over a thread-safe channel and
//! waits for acknowledgements. Volume and foreground callbacks execute on the glib thread; they
//! only forward values into Tokio channels.
//!
//! Worker API: `AudioBackend::{set_volume,set_muted,subscribe}`,
//! `TvPowerBackend::register_screen_saver_veto`, `LunaWorker::ensure_ui_foreground`,
//! `LunaWorker::foreground_app` and `LunaWorker::subscribe_foreground` are the public operations.
//! Commands carry a synchronous acknowledgement channel. Set calls acknowledge LS2 submission;
//! subscription/veto calls acknowledge the first accepted service reply (including one root→master
//! volume fallback), so setup errors propagate instead of being mistaken for a live subscription.
//! Auto-open acknowledges only after the launch was verified in the foreground (at most one
//! relaunch) or when its request deadline elapsed.

pub mod audio;
pub mod autoopen;
pub mod tvpower;

use std::ffi::{c_void, CStr, CString};
use std::os::raw::{c_char, c_int, c_uint};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use self::audio::{
    needs_volume_poll, parse_volume_reply, should_log_poll_failure, AudioBackend, VolumeCallback,
    VolumeObservation,
};
use self::autoopen::{foreground_decision, ForegroundDecision, APP_ID};
use self::tvpower::{
    response_screen_saver_payload, veto_reply_accepted, veto_request_timestamp, TvPowerBackend,
};

// dlopen / dlsym are in libdl.so.2 on glibc 2.24 (present on the TV at /lib/libdl.so.2).
#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *mut c_char;
}

// glibc's standard error stream, handed to LSErrorPrint so the LSError layout stays opaque.
unsafe extern "C" {
    static stderr: *mut c_void;
}

const RTLD_NOW: c_int = 2;
const RTLD_LOCAL: c_int = 0;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_POLL_MS: u32 = 10;
const VOLUME_POLL_MS: u32 = 250;

/// The LS2 service name (also the client name used for the screensaver veto).
pub const SERVICE_NAME: &str = "com.github.acuntex.takt.service";
const ROOT_GET_VOLUME: &str = "luna://com.webos.service.audio/getVolume";
const MASTER_GET_VOLUME: &str = "luna://com.webos.service.audio/master/getVolume";
const SET_VOLUME: &str = "luna://com.webos.service.audio/setVolume";
const SET_MUTED: &str = "luna://com.webos.audio/setMuted";
const REGISTER_VETO: &str = "luna://com.webos.service.tvpower/power/registerScreenSaverRequest";
const RESPOND_VETO: &str = "luna://com.webos.service.tvpower/power/responseScreenSaverRequest";
const GET_FOREGROUND_APP: &str = "luna://com.webos.applicationManager/getForegroundAppInfo";
const LAUNCH_APP: &str = "luna://com.webos.applicationManager/launch";

/// Monotonic token for `params.relaunch`. Only the value's novelty matters: a byte-identical
/// second launch can be coalesced with the one the application manager just swallowed.
static RELAUNCH_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn next_relaunch_token() -> u64 {
    RELAUNCH_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

// Opaque C types; none of these pointers is dereferenced in Rust.
type LSHandle = c_void;
type LSMessage = c_void;
type GMainLoop = c_void;

type LSFilterFunc = unsafe extern "C" fn(*mut LSHandle, *mut LSMessage, *mut c_void) -> bool;
type LSMethodFunction = unsafe extern "C" fn(*mut LSHandle, *mut LSMessage, *mut c_void) -> bool;

#[repr(C)]
struct LSMethod {
    name: *const c_char,
    function: LSMethodFunction,
    flags: c_int,
}

type LSRegisterFn = unsafe extern "C" fn(*const c_char, *mut *mut LSHandle, *mut c_void) -> bool;
type LSRegisterCategoryFn = unsafe extern "C" fn(
    *mut LSHandle,
    *const c_char,
    *const LSMethod,
    *const c_void,
    *const c_void,
    *mut c_void,
) -> bool;
type LSGmainAttachFn = unsafe extern "C" fn(*mut LSHandle, *mut GMainLoop, *mut c_void) -> bool;
type LSCallFn = unsafe extern "C" fn(
    *mut LSHandle,
    *const c_char,
    *const c_char,
    Option<LSFilterFunc>,
    *mut c_void,
    *mut c_void,
    *mut c_void,
) -> bool;
type LSMessageGetPayloadFn = unsafe extern "C" fn(*mut LSMessage) -> *const c_char;
type LSMessageReplyFn =
    unsafe extern "C" fn(*mut LSHandle, *mut LSMessage, *const c_char, *mut c_void) -> bool;
type LSErrorInitFn = unsafe extern "C" fn(*mut c_void) -> bool;
type LSErrorFreeFn = unsafe extern "C" fn(*mut c_void);
type LSErrorPrintFn = unsafe extern "C" fn(*mut c_void, *mut c_void);

type GSourceFunc = unsafe extern "C" fn(*mut c_void) -> c_int;
type GMainLoopNewFn = unsafe extern "C" fn(*mut c_void, c_int) -> *mut GMainLoop;
type GMainLoopRunFn = unsafe extern "C" fn(*mut GMainLoop);
type GTimeoutAddFn = unsafe extern "C" fn(c_uint, Option<GSourceFunc>, *mut c_void) -> c_uint;

/// Resolved symbols stay valid for the process lifetime because the libraries are never closed.
struct Luna {
    ls_register: LSRegisterFn,
    ls_register_category: LSRegisterCategoryFn,
    ls_gmain_attach: LSGmainAttachFn,
    ls_call: LSCallFn,
    /// `LSCallOneReply` is exported by this TV's library but not guaranteed on older firmware;
    /// the foreground query falls back to `LSCall`, which yields one reply without `subscribe`.
    ls_call_one_reply: Option<LSCallFn>,
    ls_message_get_payload: LSMessageGetPayloadFn,
    ls_message_reply: LSMessageReplyFn,
    ls_error_init: LSErrorInitFn,
    ls_error_free: LSErrorFreeFn,
    ls_error_print: LSErrorPrintFn,
    g_main_loop_new: GMainLoopNewFn,
    g_main_loop_run: GMainLoopRunFn,
    g_timeout_add: GTimeoutAddFn,
}

static LUNA: OnceLock<Luna> = OnceLock::new();
// Raw pointers are stored as usize in statics, but are only ever read on the glib worker thread.
static SERVICE_HANDLE: OnceLock<usize> = OnceLock::new();
static MAIN_LOOP: OnceLock<usize> = OnceLock::new();

fn cstr(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| anyhow!("string contains NUL byte: {s:?}"))
}

unsafe fn dlerror_str() -> String {
    let msg = unsafe { dlerror() };
    if msg.is_null() {
        "unknown error".to_string()
    } else {
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    }
}

unsafe fn open_library(name: &str) -> Result<*mut c_void> {
    let name = cstr(name)?;
    let handle = unsafe { dlopen(name.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
    if handle.is_null() {
        bail!("dlopen({}) failed: {}", name.to_string_lossy(), unsafe {
            dlerror_str()
        });
    }
    Ok(handle)
}

unsafe fn resolve<T>(handle: *mut c_void, symbol: &str) -> Result<T> {
    let name = cstr(symbol)?;
    let ptr = unsafe { dlsym(handle, name.as_ptr()) };
    if ptr.is_null() {
        bail!("dlsym({symbol}) failed: {}", unsafe { dlerror_str() });
    }
    // dlsym returns an object pointer; reinterpret it as the requested function pointer.
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&ptr) })
}

/// Like [`resolve`], but a missing symbol is not an error: older firmware may not export it.
unsafe fn resolve_optional<T>(handle: *mut c_void, symbol: &str) -> Option<T> {
    let name = cstr(symbol).ok()?;
    let ptr = unsafe { dlsym(handle, name.as_ptr()) };
    if ptr.is_null() {
        return None;
    }
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&ptr) })
}

#[repr(C, align(8))]
struct LSErrorBuf([u8; 64]);

impl LSErrorBuf {
    fn new() -> Self {
        Self([0u8; 64])
    }
}

unsafe extern "C" fn start_cb(sh: *mut LSHandle, msg: *mut LSMessage, _ctx: *mut c_void) -> bool {
    let luna = LUNA.get().expect("luna not initialised");
    let reply = c"{\"returnValue\":true}";
    unsafe { (luna.ls_message_reply)(sh, msg, reply.as_ptr(), std::ptr::null_mut()) }
}

unsafe extern "C" fn reply_log_cb(
    _sh: *mut LSHandle,
    msg: *mut LSMessage,
    _ctx: *mut c_void,
) -> bool {
    println!("[takt] Luna reply -> {}", unsafe { payload_str(msg) });
    true
}

unsafe fn payload_str(msg: *mut LSMessage) -> String {
    let luna = LUNA.get().expect("luna not initialised");
    let ptr = unsafe { (luna.ls_message_get_payload)(msg) };
    if ptr.is_null() {
        return "<null>".to_string();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Loads libraries and registers `/start`; called only by the worker thread.
fn init() -> Result<()> {
    if LUNA.get().is_none() {
        let loaded = load()?;
        LUNA.set(loaded)
            .map_err(|_| anyhow!("LS2 libraries were initialised concurrently"))?;
    }
    let luna = LUNA.get().ok_or_else(|| anyhow!("LS2 not initialised"))?;

    let name = cstr(SERVICE_NAME)?;
    let mut handle: *mut LSHandle = std::ptr::null_mut();
    let mut err = LSErrorBuf::new();
    unsafe { (luna.ls_error_init)(&mut err as *mut LSErrorBuf as *mut c_void) };

    let ok = unsafe {
        (luna.ls_register)(
            name.as_ptr(),
            &mut handle,
            &mut err as *mut LSErrorBuf as *mut c_void,
        )
    };
    if !ok {
        unsafe { (luna.ls_error_print)(&mut err as *mut LSErrorBuf as *mut c_void, stderr) };
        bail!("LSRegister({SERVICE_NAME}) failed");
    }
    SERVICE_HANDLE.set(handle as usize).ok();

    let mut methods = [
        LSMethod {
            name: c"start".as_ptr(),
            function: start_cb,
            flags: 0,
        },
        LSMethod {
            name: std::ptr::null(),
            function: start_cb,
            flags: 0,
        },
    ];
    let category = c"/";
    let ok = unsafe {
        (luna.ls_register_category)(
            handle,
            category.as_ptr(),
            methods.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            &mut err as *mut LSErrorBuf as *mut c_void,
        )
    };
    if !ok {
        unsafe { (luna.ls_error_print)(&mut err as *mut LSErrorBuf as *mut c_void, stderr) };
        bail!("LSRegisterCategory(\"/\") failed");
    }

    let loop_ = unsafe { (luna.g_main_loop_new)(std::ptr::null_mut(), 0) };
    if loop_.is_null() {
        bail!("g_main_loop_new failed");
    }
    MAIN_LOOP.set(loop_ as usize).ok();

    let ok = unsafe {
        (luna.ls_gmain_attach)(handle, loop_, &mut err as *mut LSErrorBuf as *mut c_void)
    };
    if !ok {
        unsafe { (luna.ls_error_print)(&mut err as *mut LSErrorBuf as *mut c_void, stderr) };
        bail!("LSGmainAttach failed");
    }

    unsafe { (luna.ls_error_free)(&mut err as *mut LSErrorBuf as *mut c_void) };
    println!("[takt] registered native LS2 service {SERVICE_NAME}");
    Ok(())
}

/// Issues an outgoing call on the current (glib worker) thread.
unsafe fn lscall_with(
    call: LSCallFn,
    call_name: &str,
    uri: &str,
    payload: &str,
    callback: Option<LSFilterFunc>,
    context: *mut c_void,
) -> Result<()> {
    let luna = LUNA.get().expect("luna not initialised");
    let handle = *SERVICE_HANDLE.get().expect("service not registered") as *mut LSHandle;
    let uri_c = cstr(uri)?;
    let payload_c = cstr(payload)?;
    let mut err = LSErrorBuf::new();
    unsafe { (luna.ls_error_init)(&mut err as *mut LSErrorBuf as *mut c_void) };

    let ok = unsafe {
        call(
            handle,
            uri_c.as_ptr(),
            payload_c.as_ptr(),
            callback,
            context,
            std::ptr::null_mut(),
            &mut err as *mut LSErrorBuf as *mut c_void,
        )
    };
    if !ok {
        unsafe {
            (luna.ls_error_print)(&mut err as *mut LSErrorBuf as *mut c_void, stderr);
            (luna.ls_error_free)(&mut err as *mut LSErrorBuf as *mut c_void);
        }
        bail!("{call_name}({uri}) failed");
    }
    unsafe { (luna.ls_error_free)(&mut err as *mut LSErrorBuf as *mut c_void) };
    Ok(())
}

/// Issues a one-shot call on the current (glib worker) thread.
///
/// `LSCallOneReply` is used when the library exports it; otherwise the plain `LSCall` is already
/// one-shot for a payload without `subscribe`, which is all the foreground query needs.
unsafe fn lscall_one_reply(
    uri: &str,
    payload: &str,
    callback: Option<LSFilterFunc>,
    context: *mut c_void,
) -> Result<()> {
    let luna = LUNA.get().expect("luna not initialised");
    match luna.ls_call_one_reply {
        Some(call) => unsafe {
            lscall_with(call, "LSCallOneReply", uri, payload, callback, context)
        },
        None => unsafe { lscall_with(luna.ls_call, "LSCall", uri, payload, callback, context) },
    }
}

/// Issues an asynchronous call on the current (glib worker) thread.
unsafe fn lscall(
    uri: &str,
    payload: &str,
    callback: Option<LSFilterFunc>,
    context: *mut c_void,
) -> Result<()> {
    let luna = LUNA.get().expect("luna not initialised");
    unsafe { lscall_with(luna.ls_call, "LSCall", uri, payload, callback, context) }
}

fn set_volume(level: u8) -> Result<()> {
    println!("[takt] audio/setVolume volume={level}");
    let payload = format!("{{\"volume\":{level}}}");
    unsafe {
        lscall(
            SET_VOLUME,
            &payload,
            Some(reply_log_cb),
            std::ptr::null_mut(),
        )
    }
}

fn set_muted(muted: bool) -> Result<()> {
    println!("[takt] com.webos.audio/setMuted muted={muted}");
    // Live webOS 4 verification: service.audio/setMute does not exist; the TV Audio API uses
    // service `com.webos.audio`, method `setMuted`, and a `muted` boolean.
    let payload = format!("{{\"muted\":{muted}}}");
    unsafe {
        lscall(
            SET_MUTED,
            &payload,
            Some(reply_log_cb),
            std::ptr::null_mut(),
        )
    }
}

unsafe fn subscribe_root_volume(context: *mut c_void) -> Result<()> {
    unsafe {
        lscall(
            ROOT_GET_VOLUME,
            "{\"subscribe\":true}",
            Some(volume_cb),
            context,
        )
    }
}

unsafe fn subscribe_master_volume(context: *mut c_void) -> Result<()> {
    unsafe {
        lscall(
            MASTER_GET_VOLUME,
            "{\"subscribe\":true}",
            Some(volume_cb),
            context,
        )
    }
}

unsafe fn register_veto(timestamp_secs: u64, context: *mut c_void) -> Result<()> {
    let payload = format!(
        "{{\"clientName\":\"{SERVICE_NAME}\",\"timestamp\":\"{timestamp_secs}\",\"subscribe\":true}}"
    );
    unsafe { lscall(REGISTER_VETO, &payload, Some(veto_cb), context) }
}

unsafe fn respond_veto(timestamp: &str) -> Result<()> {
    let payload = response_screen_saver_payload(SERVICE_NAME, timestamp);
    unsafe {
        lscall(
            RESPOND_VETO,
            &payload,
            Some(reply_log_cb),
            std::ptr::null_mut(),
        )
    }
}

/// One auto-open request: the foreground query, the launch, its verification and the optional
/// relaunch share this context so the waiting Tokio caller is acknowledged exactly once.
///
/// It is created on the glib thread by the command pump and released by the last scheduled
/// callback that still holds it. `pending` counts those callbacks — the hard deadline plus every
/// query, launch and verify timer in flight — so no callback can run after the box is gone.
/// A one-shot call that never answers leaks one small box; LS2 calls to the application manager
/// always answer.
struct ForegroundContext {
    ack: Option<Ack>,
    pending: u32,
}

impl ForegroundContext {
    fn finish(&mut self, result: Result<(), String>) {
        if let Some(ack) = self.ack.take() {
            ack.send(result).ok();
        }
    }
}

/// Completes one auto-open request exactly once. The box itself stays alive while a scheduled
/// callback still holds it, and the last of those releases it.
unsafe fn finish_foreground(context: *mut ForegroundContext, result: Result<(), String>) {
    unsafe { (*context).finish(result) };
    unsafe { release_foreground(context) };
}

/// Releases a foreground context once no scheduled callback can touch it any more.
unsafe fn release_foreground(context: *mut ForegroundContext) {
    if unsafe { (*context).pending } == 0 {
        unsafe { drop(Box::from_raw(context)) };
    }
}

/// Queries `getForegroundAppInfo`; the reply callback owns `context`.
unsafe fn query_foreground_app(context: *mut c_void, callback: LSFilterFunc) -> Result<()> {
    unsafe { lscall_one_reply(GET_FOREGROUND_APP, "{}", Some(callback), context) }
}

/// Issues the foreground query and counts its reply callback as an owner of `context`.
unsafe fn submit_foreground_query(
    context: *mut ForegroundContext,
    callback: LSFilterFunc,
) -> Result<()> {
    unsafe { (*context).pending += 1 };
    match unsafe { query_foreground_app(context.cast(), callback) } {
        Ok(()) => Ok(()),
        Err(error) => {
            unsafe { (*context).pending -= 1 };
            Err(error)
        }
    }
}

/// Issues a launch and counts its reply callback as an owner of `context`.
unsafe fn submit_launch(
    context: *mut ForegroundContext,
    payload: &str,
    callback: LSFilterFunc,
) -> Result<()> {
    unsafe { (*context).pending += 1 };
    match unsafe { lscall(LAUNCH_APP, payload, Some(callback), context.cast()) } {
        Ok(()) => Ok(()),
        Err(error) => {
            unsafe { (*context).pending -= 1 };
            Err(error)
        }
    }
}

/// Arms a one-shot glib timeout that counts as an owner of `context`.
fn schedule_foreground_timeout(
    delay_ms: u32,
    callback: GSourceFunc,
    context: *mut ForegroundContext,
) -> Result<()> {
    let luna = LUNA.get().ok_or_else(|| anyhow!("LS2 not initialised"))?;
    let source = unsafe { (luna.g_timeout_add)(delay_ms, Some(callback), context.cast()) };
    if source == 0 {
        bail!("g_timeout_add({delay_ms} ms) failed");
    }
    // The source cannot dispatch before this returns to the glib loop.
    unsafe { (*context).pending += 1 };
    Ok(())
}

/// Runs on glib: decide whether the UI is already visible and launch it otherwise.
unsafe extern "C" fn foreground_cb(
    _sh: *mut LSHandle,
    msg: *mut LSMessage,
    context: *mut c_void,
) -> bool {
    let context = context as *mut ForegroundContext;
    unsafe { (*context).pending -= 1 };
    let payload = unsafe { payload_str(msg) };
    let app = match autoopen::parse_foreground_app(&payload) {
        Ok(app) => app,
        Err(error) => {
            let message = format!("{error:#}; payload={payload}");
            eprintln!("[takt] auto-open failed: {message}");
            unsafe { finish_foreground(context, Err(message)) };
            return true;
        }
    };

    match foreground_decision(app.as_deref()) {
        ForegroundDecision::AlreadyForeground => {
            println!("[takt] auto-open: foreground is {APP_ID}; nothing to do");
            unsafe { finish_foreground(context, Ok(())) };
        }
        ForegroundDecision::Launch => {
            println!(
                "[takt] auto-open: foreground is {}; launching {APP_ID}",
                app.as_deref().unwrap_or("-")
            );
            if let Err(error) =
                unsafe { submit_launch(context, &autoopen::launch_payload(), launch_cb) }
            {
                let message = format!("{error:#}");
                eprintln!("[takt] auto-open failed: {message}");
                unsafe { finish_foreground(context, Err(message)) };
            }
            // On success the launch reply callback owns the context.
        }
    }
    true
}

/// Runs on glib: the application manager accepted or refused the launch.
///
/// An accepted reply is not proof that the app reached the foreground: measured on the TV, a
/// launch issued while the app was still starting was accepted and then swallowed. So do not
/// acknowledge yet; ask the foreground again after [`autoopen::VERIFY_DELAY_MS`].
unsafe extern "C" fn launch_cb(
    _sh: *mut LSHandle,
    msg: *mut LSMessage,
    context: *mut c_void,
) -> bool {
    let context = context as *mut ForegroundContext;
    unsafe { (*context).pending -= 1 };
    let payload = unsafe { payload_str(msg) };
    match autoopen::launch_accepted(&payload) {
        Ok(()) => {
            println!(
                "[takt] auto-open: launch accepted; verifying in {} ms",
                autoopen::VERIFY_DELAY_MS
            );
            if let Err(error) =
                schedule_foreground_timeout(autoopen::VERIFY_DELAY_MS, verify_timeout_cb, context)
            {
                // Without the verification timer the accepted launch is all there is to report.
                eprintln!("[takt] auto-open: verification could not be scheduled: {error:#}");
                unsafe { finish_foreground(context, Ok(())) };
            }
        }
        Err(error) => {
            let message = format!("{error:#}; payload={payload}");
            eprintln!("[takt] auto-open failed: {message}");
            unsafe { finish_foreground(context, Err(message)) };
        }
    }
    true
}

/// Runs on glib: ask once more which app has the foreground after the accepted launch.
unsafe extern "C" fn verify_timeout_cb(context: *mut c_void) -> c_int {
    let context = context as *mut ForegroundContext;
    unsafe { (*context).pending -= 1 };
    if unsafe { (*context).ack.is_none() } {
        // The hard deadline already answered the waiting caller; issue nothing more.
        unsafe { release_foreground(context) };
        return 0; // G_SOURCE_REMOVE
    }
    if let Err(error) = unsafe { submit_foreground_query(context, verify_foreground_cb) } {
        let message = format!("auto-open verification failed: {error:#}");
        eprintln!("[takt] {message}");
        unsafe { finish_foreground(context, Err(message)) };
    }
    0 // G_SOURCE_REMOVE
}

/// Runs on glib: the foreground after the launch either confirms the UI or asks for one retry.
unsafe extern "C" fn verify_foreground_cb(
    _sh: *mut LSHandle,
    msg: *mut LSMessage,
    context: *mut c_void,
) -> bool {
    let context = context as *mut ForegroundContext;
    unsafe { (*context).pending -= 1 };
    let payload = unsafe { payload_str(msg) };
    let app = match autoopen::parse_foreground_app(&payload) {
        Ok(app) => app,
        Err(error) => {
            let message = format!("{error:#}; payload={payload}");
            eprintln!("[takt] auto-open verification failed: {message}");
            unsafe { finish_foreground(context, Err(message)) };
            return true;
        }
    };

    match foreground_decision(app.as_deref()) {
        ForegroundDecision::AlreadyForeground => {
            println!("[takt] auto-open: {APP_ID} is in the foreground");
            unsafe { finish_foreground(context, Ok(())) };
        }
        ForegroundDecision::Launch => {
            println!(
                "[takt] auto-open: still {} after launch; relaunching",
                app.as_deref().unwrap_or("-")
            );
            let payload = autoopen::retry_launch_payload(next_relaunch_token());
            if let Err(error) = unsafe { submit_launch(context, &payload, relaunch_cb) } {
                let message = format!("{error:#}");
                eprintln!("[takt] auto-open failed: {message}");
                unsafe { finish_foreground(context, Err(message)) };
            }
        }
    }
    true
}

/// Runs on glib: the single retry was accepted or refused.
///
/// This is final. The retry is not verified again; the request deadline is the only remaining
/// reason to keep the context alive, and it releases it.
unsafe extern "C" fn relaunch_cb(
    _sh: *mut LSHandle,
    msg: *mut LSMessage,
    context: *mut c_void,
) -> bool {
    let context = context as *mut ForegroundContext;
    unsafe { (*context).pending -= 1 };
    let payload = unsafe { payload_str(msg) };
    let result = match autoopen::launch_accepted(&payload) {
        Ok(()) => {
            println!("[takt] auto-open: relaunch accepted");
            Ok(())
        }
        Err(error) => {
            let message = format!("{error:#}; payload={payload}");
            eprintln!("[takt] auto-open failed: {message}");
            Err(message)
        }
    };
    unsafe { finish_foreground(context, result) };
    true
}

/// Runs on glib: the hard bound for one auto-open request elapsed.
///
/// It acknowledges a request that has not answered yet but never releases a context whose LS2
/// call is still in flight; that call's callback releases it.
unsafe extern "C" fn auto_open_deadline_cb(context: *mut c_void) -> c_int {
    let context = context as *mut ForegroundContext;
    unsafe { (*context).pending -= 1 };
    if unsafe { (*context).ack.is_some() } {
        let message = format!(
            "auto-open did not finish within {} ms",
            autoopen::REQUEST_DEADLINE_MS
        );
        eprintln!("[takt] {message}");
        unsafe { finish_foreground(context, Err(message)) };
    } else {
        unsafe { release_foreground(context) };
    }
    0 // G_SOURCE_REMOVE
}

/// One `/api/foreground` query: the reply callback owns this context and completes the waiting
/// Tokio caller. Like the auto-open query, a call that never answers leaks one small box; the
/// application manager always answers a one-shot.
struct ForegroundStatusContext {
    ack: Option<ForegroundAck>,
}

/// Runs on glib: hands the parsed foreground app back to the HTTP layer.
///
/// The app id is deliberately not logged here. The UI polls this path while it is hidden, and a
/// per-query line would flood the TV log; only a transition is logged, by the HTTP cache.
unsafe extern "C" fn foreground_status_cb(
    _sh: *mut LSHandle,
    msg: *mut LSMessage,
    context: *mut c_void,
) -> bool {
    let payload = unsafe { payload_str(msg) };
    let result = autoopen::parse_foreground_app(&payload)
        .map_err(|error| format!("invalid foreground reply: {error:#}"));
    let mut context = unsafe { Box::from_raw(context as *mut ForegroundStatusContext) };
    if let Some(ack) = context.ack.take() {
        ack.send(result).ok();
    }
    true
}

type Ack = SyncSender<Result<(), String>>;
type ForegroundAck = SyncSender<Result<Option<String>, String>>;
type ForegroundSender = tokio::sync::mpsc::UnboundedSender<Option<String>>;

/// One held `getForegroundAppInfo` subscription: every pushed answer goes to the watchdog.
///
/// The application manager answers with an acknowledgement and then one message per foreground
/// change; [`autoopen::parse_foreground_app`] reads both. At most one of these exists per worker.
/// A rejected setup is released by the command pump, so a later subscribe arms exactly one
/// replacement instead of piling a second subscription onto the same worker.
struct ForegroundSubscriptionContext {
    sender: ForegroundSender,
    ack: Option<Ack>,
    accepted: bool,
    failed: bool,
}

impl ForegroundSubscriptionContext {
    fn finish(&mut self, result: Result<(), String>) {
        if let Some(ack) = self.ack.take() {
            ack.send(result).ok();
        }
    }
}

/// Runs on glib: hands every pushed foreground answer to the watchdog.
///
/// The first reply is the subscription acknowledgement and may already name the current app; an
/// acknowledgement without an app id is not an observation and is not forwarded, because "no app"
/// would re-arm the departure latch. A rejection fails a waiting setup command; a rejection after
/// the subscription was accepted marks the context failed for the command pump, which drops it so
/// the supervisor can arm one replacement. The watchdog's fallback query covers the gap.
unsafe extern "C" fn foreground_event_cb(
    _sh: *mut LSHandle,
    msg: *mut LSMessage,
    context: *mut c_void,
) -> bool {
    let context = unsafe { &mut *(context as *mut ForegroundSubscriptionContext) };
    let payload = unsafe { payload_str(msg) };
    match autoopen::parse_foreground_app(&payload) {
        Ok(app) => {
            if !context.accepted {
                context.accepted = true;
                println!("[takt] foreground subscription held");
                context.finish(Ok(()));
                if let Some(app) = app {
                    context.sender.send(Some(app)).ok();
                }
            } else {
                // A closed receiver means the daemon is shutting down; nothing left to tell.
                context.sender.send(app).ok();
            }
        }
        Err(error) => {
            let message =
                format!("invalid getForegroundAppInfo reply: {error:#}; payload={payload}");
            if context.accepted {
                eprintln!("[takt] foreground subscription ended: {message}");
            } else {
                eprintln!("[takt] {message}");
            }
            context.failed = true;
            context.finish(Err(message));
        }
    }
    true
}

unsafe fn subscribe_foreground_app(context: *mut c_void) -> Result<()> {
    unsafe {
        lscall(
            GET_FOREGROUND_APP,
            "{\"subscribe\":true}",
            Some(foreground_event_cb),
            context,
        )
    }
}

struct VolumeContext {
    callback: VolumeCallback,
    ack: Option<Ack>,
    master_fallback_attempted: bool,
    accepted: bool,
    failed: bool,
    last_muted: bool,
    observation: VolumeObservation,
    polling_started: bool,
    poll_failures: u32,
}

impl VolumeContext {
    fn finish(&mut self, result: Result<(), String>) {
        if let Some(ack) = self.ack.take() {
            ack.send(result).ok();
        }
    }

    /// Arms the single poll chain if this accepted reply should own one. On failure the flag stays
    /// false so the next accepted reply (or an idempotent re-subscribe) can try again.
    fn arm_volume_poll(&mut self) {
        if !needs_volume_poll(self.polling_started, self.master_fallback_attempted) {
            return;
        }
        match schedule_volume_poll(self as *mut VolumeContext as *mut c_void) {
            Ok(()) => self.polling_started = true,
            Err(error) => {
                self.polling_started = false;
                self.finish(Err(format!("volume polling fallback failed: {error:#}")));
            }
        }
    }
}

/// Runs on glib. A rejected root subscription falls back to `master/getVolume` exactly once.
unsafe extern "C" fn volume_cb(
    _sh: *mut LSHandle,
    msg: *mut LSMessage,
    context: *mut c_void,
) -> bool {
    let context = unsafe { &mut *(context as *mut VolumeContext) };
    let payload = unsafe { payload_str(msg) };
    let reply = match parse_volume_reply(&payload) {
        Ok(reply) => reply,
        Err(error) => {
            let message = format!("invalid audio/getVolume reply: {error:#}; payload={payload}");
            eprintln!("[takt] {message}");
            context.failed = true;
            context.finish(Err(message));
            return true;
        }
    };

    if !reply.accepted {
        if !context.master_fallback_attempted {
            context.master_fallback_attempted = true;
            eprintln!(
                "[takt] root audio/getVolume subscription rejected; trying master/getVolume once"
            );
            if let Err(error) =
                unsafe { subscribe_master_volume(context as *mut VolumeContext as *mut c_void) }
            {
                let message = format!("master/getVolume fallback failed: {error:#}");
                context.failed = true;
                context.finish(Err(message));
            }
        } else {
            context.failed = true;
            context.finish(Err(format!(
                "root and master audio/getVolume subscriptions rejected: {payload}"
            )));
        }
        return true;
    }

    // This TV acknowledges root getVolume subscriptions but (verified with both FFI and
    // `luna-send -i`) never publishes subsequent API/remote-style changes. Keep that subscription
    // held, and supplement it with deduplicated one-shot FFI polls on the same glib thread.
    context.accepted = true;
    context.arm_volume_poll();

    context.finish(Ok(()));
    if let Some(muted) = reply.muted {
        context.last_muted = muted;
    }
    if let Some(volume) = reply.volume {
        let muted = context.last_muted;
        if context.observation.changed(volume, muted) {
            println!("[takt] audio/getVolume changed -> {payload}");
            (context.callback)(volume, muted);
        }
    }
    true
}

fn schedule_volume_poll(context: *mut c_void) -> Result<()> {
    let luna = LUNA.get().ok_or_else(|| anyhow!("LS2 not initialised"))?;
    let source = unsafe { (luna.g_timeout_add)(VOLUME_POLL_MS, Some(volume_poll_cb), context) };
    if source == 0 {
        bail!("g_timeout_add(volume poll) failed");
    }
    Ok(())
}

unsafe extern "C" fn volume_poll_cb(context: *mut c_void) -> c_int {
    let context = unsafe { &mut *(context as *mut VolumeContext) };
    // A one-shot getVolume still uses FFI on the glib thread; the daemon never shells out.
    let raw = context as *mut VolumeContext as *mut c_void;
    match unsafe { lscall(ROOT_GET_VOLUME, "{}", Some(volume_cb), raw) } {
        Ok(()) => {
            context.poll_failures = 0;
            1
        }
        Err(error) => {
            // Keep the single poll source alive: a transient LSCall failure must not disable
            // external volume/mute reporting until restart.
            context.poll_failures = context.poll_failures.saturating_add(1);
            if should_log_poll_failure(context.poll_failures) {
                eprintln!(
                    "[takt] volume polling failed (attempt {}), keeping poll source alive: {error:#}",
                    context.poll_failures
                );
            }
            1
        }
    }
}

struct VetoContext {
    ack: Option<Ack>,
}

impl VetoContext {
    fn finish(&mut self, result: Result<(), String>) {
        if let Some(ack) = self.ack.take() {
            ack.send(result).ok();
        }
    }
}

unsafe extern "C" fn veto_cb(
    _sh: *mut LSHandle,
    msg: *mut LSMessage,
    context: *mut c_void,
) -> bool {
    let context = unsafe { &mut *(context as *mut VetoContext) };
    let payload = unsafe { payload_str(msg) };
    println!("[takt] tvpower/registerScreenSaverRequest reply -> {payload}");
    if let Some(timestamp) = veto_request_timestamp(&payload) {
        // This is an actual screensaver request pushed on the held subscription, not a
        // subscription acknowledgement. The push itself proves the subscription is live, so a
        // setup command still waiting for its ack must complete immediately instead of timing out.
        // Respond on the same glib thread and keep the held subscription untouched.
        context.finish(Ok(()));
        println!("[takt] screensaver request ack=false -> {payload}");
        if let Err(error) = unsafe { respond_veto(&timestamp) } {
            eprintln!("[takt] screensaver response failed: {error:#}");
        }
        return true;
    }
    let result = match veto_reply_accepted(&payload) {
        Ok(true) => {
            println!("[takt] screensaver veto subscription held");
            Ok(())
        }
        Ok(false) => Err(format!("screensaver veto was not subscribed: {payload}")),
        Err(error) => Err(format!(
            "invalid screensaver-veto reply: {error:#}; payload={payload}"
        )),
    };
    if let Err(error) = &result {
        eprintln!("[takt] {error}");
    }
    context.finish(result);
    true
}

enum Command {
    SetVolume { volume: u8, ack: Ack },
    SetMuted { muted: bool, ack: Ack },
    SubscribeVolume { callback: VolumeCallback, ack: Ack },
    RegisterVeto { timestamp_secs: u64, ack: Ack },
    EnsureUiForeground { ack: Ack },
    QueryForeground { ack: ForegroundAck },
    SubscribeForeground { sender: ForegroundSender, ack: Ack },
}

struct WorkerState {
    commands: Receiver<Command>,
    // Stable allocation reused by the initial registration and every idempotent renewal.
    veto: Box<VetoContext>,
    // At most one held volume subscription per worker/Audio instance. A failed setup drops it so a
    // later retry can create exactly one replacement.
    volume: Option<Box<VolumeContext>>,
    // At most one held foreground subscription per worker. A failed or ended one is dropped by the
    // pump so the subscription supervisor can arm exactly one replacement.
    foreground: Option<Box<ForegroundSubscriptionContext>>,
}

unsafe extern "C" fn command_pump(context: *mut c_void) -> c_int {
    let state = unsafe { &mut *(context as *mut WorkerState) };
    // A rejected or ended foreground subscription has already delivered the last message it will
    // ever get, so releasing the context here is safe. Its sender drops with it, the supervisor's
    // channel closes, and the supervisor arms one replacement.
    if state
        .foreground
        .as_ref()
        .is_some_and(|context| context.failed)
    {
        state.foreground = None;
    }
    while let Ok(command) = state.commands.try_recv() {
        match command {
            Command::SetVolume { volume, ack } => {
                ack.send(set_volume(volume).map_err(|error| format!("{error:#}")))
                    .ok();
            }
            Command::SetMuted { muted, ack } => {
                ack.send(set_muted(muted).map_err(|error| format!("{error:#}")))
                    .ok();
            }
            Command::SubscribeVolume { callback, ack } => {
                // A definitively failed, never-accepted setup owns no callback chain, so it is safe
                // to replace. An accepted context (which may own the poll source) is never dropped.
                if state
                    .volume
                    .as_ref()
                    .is_some_and(|context| context.failed && !context.accepted)
                {
                    state.volume = None;
                }
                if let Some(context) = state.volume.as_mut() {
                    // Re-subscribe on the same Audio instance must reuse the held LS2 context.
                    context.callback = callback;
                    context.ack = Some(ack);
                    if context.accepted {
                        context.arm_volume_poll();
                        context.finish(Ok(()));
                    } else {
                        context.finish(Err(
                            "volume subscription is still pending acknowledgement".to_string()
                        ));
                    }
                } else {
                    let context = Box::new(VolumeContext {
                        callback,
                        ack: Some(ack),
                        master_fallback_attempted: false,
                        accepted: false,
                        failed: false,
                        last_muted: false,
                        observation: VolumeObservation::default(),
                        polling_started: false,
                        poll_failures: 0,
                    });
                    state.volume = Some(context);
                    let raw = state
                        .volume
                        .as_mut()
                        .expect("volume context was just installed")
                        .as_mut() as *mut VolumeContext
                        as *mut c_void;
                    if let Err(error) = unsafe { subscribe_root_volume(raw) } {
                        if let Some(mut context) = state.volume.take() {
                            context.finish(Err(format!(
                                "root audio/getVolume subscription failed: {error:#}"
                            )));
                        }
                    }
                }
                // On success the held LS2 subscription owns the callback context for its lifetime.
            }
            Command::RegisterVeto {
                timestamp_secs,
                ack,
            } => {
                state.veto.ack = Some(ack);
                let context = state.veto.as_mut() as *mut VetoContext as *mut c_void;
                if let Err(error) = unsafe { register_veto(timestamp_secs, context) } {
                    state
                        .veto
                        .finish(Err(format!("screensaver-veto call failed: {error:#}")));
                }
            }
            Command::EnsureUiForeground { ack } => {
                // The glib loop never waits: the query's reply callback decides, may submit the
                // launch, and only acknowledges after the foreground confirmed the app or the
                // request deadline elapsed.
                let context = Box::into_raw(Box::new(ForegroundContext {
                    ack: Some(ack),
                    pending: 0,
                }));
                // Arm the hard deadline first. Without it a TV that swallows both launches would
                // hold the Tokio caller until its own command timeout; if the source cannot be
                // created, answer at once instead of starting an unbounded request.
                if let Err(error) = schedule_foreground_timeout(
                    autoopen::REQUEST_DEADLINE_MS,
                    auto_open_deadline_cb,
                    context,
                ) {
                    let message = format!("auto-open deadline could not be scheduled: {error:#}");
                    eprintln!("[takt] auto-open failed: {message}");
                    unsafe { finish_foreground(context, Err(message)) };
                } else if let Err(error) =
                    unsafe { submit_foreground_query(context, foreground_cb) }
                {
                    let message = format!("{error:#}");
                    eprintln!("[takt] auto-open failed: {message}");
                    unsafe { finish_foreground(context, Err(message)) };
                    // The deadline source still owns the context and releases it when it fires.
                }
            }
            Command::QueryForeground { ack } => {
                // Same one-shot call as auto-open, but the reply only reports the app id. The
                // reply callback owns the box; a submission failure releases it here.
                let context = Box::into_raw(Box::new(ForegroundStatusContext { ack: Some(ack) }))
                    as *mut c_void;
                if let Err(error) = unsafe { query_foreground_app(context, foreground_status_cb) } {
                    let mut context =
                        unsafe { Box::from_raw(context as *mut ForegroundStatusContext) };
                    if let Some(ack) = context.ack.take() {
                        ack.send(Err(format!("{error:#}"))).ok();
                    }
                }
            }
            Command::SubscribeForeground { sender, ack } => {
                if let Some(context) = state.foreground.as_mut() {
                    // One subscription per worker: a re-subscribe points the held call at the new
                    // receiver instead of arming a second one.
                    context.sender = sender;
                    context.ack = Some(ack);
                    if context.accepted {
                        context.finish(Ok(()));
                    } else {
                        context.finish(Err(
                            "foreground subscription is still pending acknowledgement".to_string(),
                        ));
                    }
                } else {
                    let context = Box::new(ForegroundSubscriptionContext {
                        sender,
                        ack: Some(ack),
                        accepted: false,
                        failed: false,
                    });
                    state.foreground = Some(context);
                    let raw = state
                        .foreground
                        .as_mut()
                        .expect("foreground context was just installed")
                        .as_mut()
                        as *mut ForegroundSubscriptionContext
                        as *mut c_void;
                    if let Err(error) = unsafe { subscribe_foreground_app(raw) } {
                        if let Some(mut context) = state.foreground.take() {
                            context.finish(Err(format!(
                                "getForegroundAppInfo subscription failed: {error:#}"
                            )));
                        }
                    }
                }
                // On success the held LS2 subscription owns the callback context for its lifetime.
            }
        }
    }
    1 // G_SOURCE_CONTINUE
}

fn schedule_command_pump(state: *mut c_void) -> Result<()> {
    let luna = LUNA.get().ok_or_else(|| anyhow!("LS2 not initialised"))?;
    let source = unsafe { (luna.g_timeout_add)(COMMAND_POLL_MS, Some(command_pump), state) };
    if source == 0 {
        bail!("g_timeout_add(command pump) failed");
    }
    Ok(())
}

fn run() -> ! {
    let luna = LUNA.get().expect("luna not initialised");
    let loop_ = *MAIN_LOOP.get().expect("service not initialised") as *mut GMainLoop;
    unsafe { (luna.g_main_loop_run)(loop_) };
    unreachable!("g_main_loop_run returned")
}

/// Cloneable, thread-safe command handle. It contains no LS2/glib pointer and never calls FFI.
#[derive(Clone)]
pub struct LunaWorker {
    commands: mpsc::Sender<Command>,
}

impl LunaWorker {
    /// Starts the one dedicated LS2/glib thread and waits until registration succeeds or fails.
    pub fn start() -> Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("takt-luna-glib".to_owned())
            .spawn(move || {
                if let Err(error) = init() {
                    ready_sender.send(Err(format!("{error:#}"))).ok();
                    return;
                }
                let state = Box::new(WorkerState {
                    commands: receiver,
                    veto: Box::new(VetoContext { ack: None }),
                    volume: None,
                    foreground: None,
                });
                let state = Box::into_raw(state);
                if let Err(error) = schedule_command_pump(state as *mut c_void) {
                    unsafe { drop(Box::from_raw(state)) };
                    ready_sender.send(Err(format!("{error:#}"))).ok();
                    return;
                }
                ready_sender.send(Ok(())).ok();
                run();
            })?;

        match ready_receiver.recv_timeout(COMMAND_TIMEOUT) {
            Ok(Ok(())) => Ok(Self { commands }),
            Ok(Err(error)) => bail!("Luna worker initialisation failed: {error}"),
            Err(error) => bail!("Luna worker initialisation acknowledgement failed: {error}"),
        }
    }

    fn request_value<T>(
        &self,
        command: impl FnOnce(SyncSender<Result<T, String>>) -> Command,
    ) -> Result<T> {
        let (ack, result) = mpsc::sync_channel(1);
        self.commands
            .send(command(ack))
            .map_err(|_| anyhow!("Luna worker command channel is closed"))?;
        match result.recv_timeout(COMMAND_TIMEOUT) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => bail!(error),
            Err(error) => bail!("Luna worker acknowledgement failed: {error}"),
        }
    }

    fn request(&self, command: impl FnOnce(Ack) -> Command) -> Result<()> {
        self.request_value(command)
    }

    /// Brings the bundled UI to the foreground unless it already is.
    ///
    /// The glib worker queries `getForegroundAppInfo` first and only launches on a different (or
    /// absent) foreground app, so an already visible UI is never relaunched and focus is never
    /// stolen. An accepted launch is not trusted on its own: 1.5 s later the worker queries the
    /// foreground again and, if the app did not arrive, relaunches once with a distinct payload.
    /// The acknowledgement arrives within 4 s either way; failures are returned for the caller to
    /// log and never touch playback.
    pub fn ensure_ui_foreground(&self) -> Result<()> {
        self.request(|ack| Command::EnsureUiForeground { ack })
    }

    /// Queries the TV's current foreground app for `GET /api/foreground`.
    ///
    /// `Ok(None)` means the TV named no app. This is the same one-shot `getForegroundAppInfo`
    /// call as auto-open, on the same glib worker; the result is never logged here because the
    /// endpoint may poll it.
    pub fn foreground_app(&self) -> Result<Option<String>> {
        self.request_value(|ack| Command::QueryForeground { ack })
    }

    /// Arms the held `getForegroundAppInfo` subscription; every foreground change is sent to
    /// `sender` as `Some(app_id)`, or `None` when the TV names no app.
    ///
    /// The acknowledgement arrives only after the application manager accepted the subscription
    /// (an acknowledgement that already names an app is forwarded as that observation), so a
    /// refused setup is visible to the caller. At most one subscription exists per worker: a
    /// second call re-points the held one at the new receiver and creates no second call.
    pub fn subscribe_foreground(&self, sender: ForegroundSender) -> Result<()> {
        self.request(|ack| Command::SubscribeForeground { sender, ack })
    }
}

impl AudioBackend for LunaWorker {
    fn set_volume(&self, volume: u8) -> Result<()> {
        self.request(|ack| Command::SetVolume { volume, ack })
    }

    fn set_muted(&self, muted: bool) -> Result<()> {
        self.request(|ack| Command::SetMuted { muted, ack })
    }

    fn subscribe(&self, callback: VolumeCallback) -> Result<()> {
        self.request(|ack| Command::SubscribeVolume { callback, ack })
    }
}

impl TvPowerBackend for LunaWorker {
    fn register_screen_saver_veto(&self, timestamp_secs: u64) -> Result<()> {
        self.request(|ack| Command::RegisterVeto {
            timestamp_secs,
            ack,
        })
    }
}

fn load() -> Result<Luna> {
    unsafe {
        let ls2 = open_library("libluna-service2.so.3")?;
        let glib = open_library("libglib-2.0.so.0")?;

        Ok(Luna {
            ls_register: resolve(ls2, "LSRegister")?,
            ls_register_category: resolve(ls2, "LSRegisterCategory")?,
            ls_gmain_attach: resolve(ls2, "LSGmainAttach")?,
            ls_call: resolve(ls2, "LSCall")?,
            ls_call_one_reply: resolve_optional(ls2, "LSCallOneReply"),
            ls_message_get_payload: resolve(ls2, "LSMessageGetPayload")?,
            ls_message_reply: resolve(ls2, "LSMessageReply")?,
            ls_error_init: resolve(ls2, "LSErrorInit")?,
            ls_error_free: resolve(ls2, "LSErrorFree")?,
            ls_error_print: resolve(ls2, "LSErrorPrint")?,
            g_main_loop_new: resolve(glib, "g_main_loop_new")?,
            g_main_loop_run: resolve(glib, "g_main_loop_run")?,
            g_timeout_add: resolve(glib, "g_timeout_add")?,
        })
    }
}

//! Windows power handles and the mpv-style screen saver message suppression.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context as _, Result};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::SC_SCREENSAVE,
        System::{
            Power::{
                PowerClearRequest, PowerCreateRequest, PowerRequestDisplayRequired,
                PowerRequestSystemRequired, PowerSetRequest,
            },
            Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0},
        },
        UI::{
            Shell::{DefSubclassProc, GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::{SC_MONITORPOWER, WM_NCDESTROY, WM_SYSCOMMAND},
        },
    },
    core::PWSTR,
};

pub(super) struct PlaybackPowerRequest {
    handle: HANDLE,
    // PowerCreateRequest may retain the reason pointer until CloseHandle.
    _reason: Vec<u16>,
    system_required: bool,
    display_required: bool,
    screensaver_requests: Option<Arc<AtomicUsize>>,
}

// SAFETY: Power request handles can be cleared/closed on any thread. The window
// subclass is installed only on its UI thread and consults an atomic counter;
// releasing this guard never calls a window API.
unsafe impl Send for PlaybackPowerRequest {}

impl PlaybackPowerRequest {
    pub(super) fn new(hwnd: HWND, reason: &str) -> Result<Self> {
        let mut reason = reason.encode_utf16().chain([0]).collect::<Vec<_>>();
        let context = REASON_CONTEXT {
            Version: 0, // POWER_REQUEST_CONTEXT_VERSION
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: PWSTR(reason.as_mut_ptr()),
            },
        };
        // SAFETY: The context and its NUL-terminated reason are valid. The
        // request owns the reason buffer for the entire handle lifetime.
        let handle = unsafe { PowerCreateRequest(&context) }
            .context("Could not create a playback power request")?;
        let mut request = Self {
            handle,
            _reason: reason,
            system_required: false,
            display_required: false,
            screensaver_requests: None,
        };
        // SAFETY: handle is a live power request owned by request. Drop rolls
        // back already-set requests if any later acquisition step fails.
        unsafe { PowerSetRequest(handle, PowerRequestSystemRequired) }
            .context("Could not inhibit automatic sleep")?;
        request.system_required = true;
        unsafe { PowerSetRequest(handle, PowerRequestDisplayRequired) }
            .context("Could not inhibit display power saving")?;
        request.display_required = true;
        request.screensaver_requests = Some(inhibit_screensaver(hwnd)?);
        Ok(request)
    }
}

impl Drop for PlaybackPowerRequest {
    fn drop(&mut self) {
        if let Some(requests) = &self.screensaver_requests {
            requests.fetch_sub(1, Ordering::Relaxed);
        }
        // SAFETY: This guard uniquely owns the live handle. Clear only requests
        // that were successfully set, then close it before dropping the reason.
        unsafe {
            if self.display_required
                && let Err(error) = PowerClearRequest(self.handle, PowerRequestDisplayRequired)
            {
                tracing::warn!(%error, "Could not release the playback display request");
            }
            if self.system_required
                && let Err(error) = PowerClearRequest(self.handle, PowerRequestSystemRequired)
            {
                tracing::warn!(%error, "Could not release the playback sleep request");
            }
            if let Err(error) = CloseHandle(self.handle) {
                tracing::warn!(%error, "Could not close the playback power request");
            }
        }
    }
}

const SUBCLASS_ID: usize = 1;

fn inhibit_screensaver(hwnd: HWND) -> Result<Arc<AtomicUsize>> {
    let mut data = 0;
    // SAFETY: Called on GPUI's window thread with its live HWND. The subclass
    // owns one Arc strong reference until WM_NCDESTROY.
    let requests = unsafe {
        if GetWindowSubclass(hwnd, Some(screensaver_proc), SUBCLASS_ID, Some(&mut data)).as_bool() {
            let pointer = data as *const AtomicUsize;
            Arc::increment_strong_count(pointer);
            Arc::from_raw(pointer)
        } else {
            let requests = Arc::new(AtomicUsize::new(0));
            let pointer = Arc::into_raw(requests.clone());
            if !SetWindowSubclass(hwnd, Some(screensaver_proc), SUBCLASS_ID, pointer as usize)
                .as_bool()
            {
                drop(Arc::from_raw(pointer));
                anyhow::bail!("Could not inhibit the Windows screen saver");
            }
            requests
        }
    };
    requests.fetch_add(1, Ordering::Relaxed);
    Ok(requests)
}

unsafe extern "system" fn screensaver_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: data is the Arc reference transferred when installing this
    // subclass. Windows invokes it on the window thread until WM_NCDESTROY.
    unsafe {
        if message == WM_NCDESTROY {
            let _ = RemoveWindowSubclass(hwnd, Some(screensaver_proc), subclass_id);
            drop(Arc::from_raw(data as *const AtomicUsize));
        } else if message == WM_SYSCOMMAND {
            let command = (wparam.0 & 0xfff0) as u32;
            let requests = &*(data as *const AtomicUsize);
            if requests.load(Ordering::Relaxed) > 0
                && matches!(command, SC_SCREENSAVE | SC_MONITORPOWER)
            {
                return LRESULT(0);
            }
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }
}

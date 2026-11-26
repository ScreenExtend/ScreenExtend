use std::sync::atomic::{self, AtomicBool, AtomicI32, AtomicU32};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use windows::core::{IInspectable, Interface, HSTRING};
use windows::Foundation::Metadata::ApiInformation;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureDirtyRegionMode, GraphicsCaptureItem,
    GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::UI::WindowsAndMessaging::{PostQuitMessage, PostThreadMessageW, WM_QUIT};

use super::capture::GraphicsCaptureApiHandler;
use super::d3d11::{self, create_direct3d_device, SendDirectX};
use super::frame::Frame;
use super::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings,
};

#[derive(thiserror::Error, Eq, PartialEq, Clone, Debug)]
/// Errors that can occur when configuring or running a Windows Graphics Capture session.
pub enum Error {
    /// The Windows Graphics Capture API is not available on this OS.
    #[error("The Graphics Capture API is not supported on this platform.")]
    Unsupported,
    /// Capture has already been started for this session.
    #[error("The capture has already been started.")]
    AlreadyStarted,
    /// Underlying Direct3D (D3D11) error.
    ///
    /// Wraps [`super::d3d11::Error`].
    #[error("DirectX error: {0}")]
    DirectXError(#[from] d3d11::Error),
    /// A Windows Runtime/Win32 API call failed.
    ///
    /// Wraps [`windows::core::Error`].
    #[error("Windows API error: {0}")]
    WindowsError(#[from] windows::core::Error),
}

/// Which optional `GraphicsCaptureSession` knobs this Windows build actually exposes.
///
/// Every one of these is a property that only exists from some OS build onwards, and WinRT gives
/// no version to compare against — `ApiInformation::IsPropertyPresent` is the probe. They are
/// resolved once and cached: the answer cannot change while the process runs, and each query
/// allocates two `HSTRING`s.
///
/// The rule this type exists to enforce: a missing property means *skip the setter*, never fail
/// the capture. `MinUpdateInterval` and `DirtyRegionMode` arrived in build 26100 (Windows 11
/// 24H2), so treating either as required silently pushes every Windows 10 and pre-24H2 Windows 11
/// host onto the DXGI fallback — which is the whole population minus one Windows release.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WgcFeatures {
    /// `IsCursorCaptureEnabled` — build 19041 (Windows 10 2004).
    pub cursor_toggle: bool,
    /// `IsBorderRequired` — build 20348. Microsoft documents that turning the border off may need
    /// `GraphicsCaptureAccess.RequestAccessAsync(Borderless)` consent first, and nothing here asks
    /// for it; whether an unpackaged Win32 process needs it is not settled. The value is read back
    /// after it is set and logged once per session (`wgc: session settings in effect`), which
    /// shows what the session reports, not what the compositor draws.
    pub border_toggle: bool,
    /// `IncludeSecondaryWindows` — build 22000 (Windows 11 21H2).
    pub secondary_windows: bool,
    /// `MinUpdateInterval` — build 26100 (Windows 11 24H2).
    pub min_update_interval: bool,
    /// `DirtyRegionMode` — build 26100 (Windows 11 24H2).
    pub dirty_region: bool,
    /// `Direct3D11CaptureFramePool::CreateFreeThreaded` — build 17763 (Windows 10 1809).
    pub free_threaded_pool: bool,
}

impl WgcFeatures {
    /// A stable one-line rendering for the log and for `docs/CAPABILITY-TIERS.md`: each feature
    /// name, `+` when present and `-` when absent.
    #[must_use]
    pub fn summary(&self) -> String {
        let flags = [
            ("cursor", self.cursor_toggle),
            ("border", self.border_toggle),
            ("secondary-windows", self.secondary_windows),
            ("min-update-interval", self.min_update_interval),
            ("dirty-region", self.dirty_region),
            ("free-threaded-pool", self.free_threaded_pool),
        ];
        flags
            .iter()
            .map(|(name, present)| format!("{}{name}", if *present { '+' } else { '-' }))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// True when the host is a 24H2-or-newer build, i.e. the two properties that only exist there
    /// are both present. Used only for reporting a tier name.
    #[must_use]
    pub const fn has_26100_properties(&self) -> bool {
        self.min_update_interval && self.dirty_region
    }
}

/// **The** predicate for "can this machine run Windows.Graphics.Capture". Both the capture path
/// and `windows_utils::compatibility` must go through this and nothing else.
///
/// It is `GraphicsCaptureSession::IsSupported()` alone. Upstream ANDed in
/// `IsApiContractPresentByMajor("Windows.Foundation.UniversalApiContract", 8)`, i.e. Windows 10
/// 1809, and the compatibility probe dropped that conjunct as a version-shaped proxy — so on a
/// build where the contract gate binds, the report claimed WGC while `GraphicsCaptureApi::new`
/// refused it and `wgc_features()` returned every knob absent. One of the two had to go, and it
/// is the contract check, because **the capture path uses no API newer than the contract-6 set**
/// `IsSupported()` itself belongs to: `Direct3D11CaptureFramePool::Create`,
/// `CreateCaptureSession`, `item.Closed`, `FrameArrived` and `item.Size`. Everything above that —
/// `CreateFreeThreaded` (17763), `IsCursorCaptureEnabled` (19041), `IsBorderRequired` (20348),
/// `IncludeSecondaryWindows` (22000), `MinUpdateInterval` and `DirtyRegionMode` (26100) — is
/// probed individually by [`wgc_features`] and applied only where present, which is the
/// probe-by-capability rule the contract number was standing in for.
///
/// Keep the two callers on this one function. If a future change does take a hard dependency on a
/// newer API, gate *that* API, not the whole backend.
#[inline]
#[must_use]
pub fn wgc_supported() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

fn session_property(name: &str) -> bool {
    ApiInformation::IsPropertyPresent(
        &HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession"),
        &HSTRING::from(name),
    )
    .unwrap_or(false)
}

/// Probes every optional capture knob once and caches the answer.
#[inline]
#[must_use]
pub fn wgc_features() -> WgcFeatures {
    static FEATURES: OnceLock<WgcFeatures> = OnceLock::new();
    *FEATURES.get_or_init(|| {
        if !wgc_supported() {
            return WgcFeatures::default();
        }
        WgcFeatures {
            cursor_toggle: session_property("IsCursorCaptureEnabled"),
            border_toggle: session_property("IsBorderRequired"),
            secondary_windows: session_property("IncludeSecondaryWindows"),
            min_update_interval: session_property("MinUpdateInterval"),
            dirty_region: session_property("DirtyRegionMode"),
            free_threaded_pool: ApiInformation::IsMethodPresent(
                &HSTRING::from("Windows.Graphics.Capture.Direct3D11CaptureFramePool"),
                &HSTRING::from("CreateFreeThreaded"),
            )
            .unwrap_or(false),
        }
    })
}

/// Provides a way to gracefully stop the capture session thread.
pub struct InternalCaptureControl {
    stop: Arc<AtomicBool>,
}

impl InternalCaptureControl {
    /// Constructs a new `InternalCaptureControl` struct.
    #[inline]
    #[must_use]
    pub const fn new(stop: Arc<AtomicBool>) -> Self {
        Self { stop }
    }

    /// Signals the capture thread to stop.
    #[inline]
    pub fn stop(self) {
        self.stop.store(true, atomic::Ordering::Relaxed);
    }
}

/// Manages a graphics capture session using the Windows Graphics Capture API.
pub struct GraphicsCaptureApi {
    /// The [`windows::Graphics::Capture::GraphicsCaptureItem`] to be captured (e.g., a window or
    /// monitor).
    item_with_details: GraphicsCaptureItemType,
    /// The Direct3D 11 device used for the capture.
    _d3d_device: ID3D11Device,
    /// The WinRT [`windows::Graphics::DirectX::Direct3D11::IDirect3DDevice`] wrapper.
    _direct3d_device: IDirect3DDevice,
    /// The Direct3D 11 device context.
    _d3d_device_context: ID3D11DeviceContext,
    /// The frame pool that provides frames for the capture session.
    frame_pool: Option<Arc<Direct3D11CaptureFramePool>>,
    /// The graphics capture session itself.
    session: Option<GraphicsCaptureSession>,
    /// An atomic boolean flag to signal the capture thread to stop.
    halt: Arc<AtomicBool>,
    /// A flag indicating whether the capture session is currently active.
    active: bool,
    /// The token for the `Closed` event handler, once it is registered.
    capture_closed_event_token: Option<i64>,
    /// The token for the `FrameArrived` event handler, once it is registered.
    frame_arrived_event_token: Option<i64>,
}

/// Ends the capture thread's message loop from whichever thread a delegate runs on.
///
/// On the capture thread itself (a non-free-threaded pool dispatches `FrameArrived` there)
/// `PostQuitMessage` cannot fail. Elsewhere (the item's `Closed` event, or a free-threaded pool)
/// it is `PostThreadMessageW`, and a failure is logged rather than returned: a delegate's error
/// goes to WinRT, which ignores it, so the halt flag would be set and the loop would never end,
/// leaving the thread parked in `GetMessageW` for good.
fn quit_capture_thread(thread_id: u32) {
    if unsafe { GetCurrentThreadId() } == thread_id {
        unsafe { PostQuitMessage(0) };
        return;
    }
    if let Err(e) =
        unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM::default(), LPARAM::default()) }
    {
        teprintln!("wgc: could not post WM_QUIT to the capture thread ({e}); it stays halted");
    }
}

impl GraphicsCaptureApi {
    /// Constructs a new [`GraphicsCaptureApi`] instance.
    ///
    /// For parameter details, see the type names in the signature; complex behavior is documented
    /// inline below where relevant.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn new<
        T: GraphicsCaptureApiHandler<Error = E> + Send + 'static,
        E: Send + Sync + 'static,
    >(
        d3d_device: ID3D11Device,
        d3d_device_context: ID3D11DeviceContext,
        item_with_details: GraphicsCaptureItemType,
        callback: Arc<Mutex<T>>,
        cursor_capture_settings: CursorCaptureSettings,
        draw_border_settings: DrawBorderSettings,
        secondary_window_settings: SecondaryWindowSettings,
        minimum_update_interval_settings: MinimumUpdateIntervalSettings,
        dirty_region_settings: DirtyRegionSettings,
        color_format: ColorFormat,
        frame_pool_buffers: u32,
        free_threaded: bool,
        stale_frames_dropped: Arc<AtomicU32>,
        thread_id: u32,
        result: Arc<Mutex<Option<E>>>,
    ) -> Result<Self, Error> {
        // Check support
        if !Self::is_supported()? {
            return Err(Error::Unsupported);
        }

        let features = wgc_features();

        let GraphicsCaptureItemType::Monitor((item, _)) = &item_with_details;
        let item = item.clone();

        // Create DirectX devices
        let direct3d_device = create_direct3d_device(&d3d_device)?;

        let pixel_format = DirectXPixelFormat(color_format as i32);

        let buffers = frame_pool_buffers.clamp(1, 8) as i32;
        let initial_size = item.Size()?;
        // `CreateFreeThreaded` is only asked for where it exists; a handler that wants it on a
        // build without it gets the dispatcher-queue pool it always had.
        let free_threaded = free_threaded && features.free_threaded_pool;
        let frame_pool = if free_threaded {
            Direct3D11CaptureFramePool::CreateFreeThreaded(
                &direct3d_device,
                pixel_format,
                buffers,
                initial_size,
            )?
        } else {
            Direct3D11CaptureFramePool::Create(
                &direct3d_device,
                pixel_format,
                buffers,
                initial_size,
            )?
        };
        let frame_pool = Arc::new(frame_pool);

        // Create capture session
        let session = frame_pool.CreateCaptureSession(&item)?;

        // Indicates if the capture is closed
        let halt = Arc::new(AtomicBool::new(false));

        // `Self` exists before the first delegate is registered, so every `?` from here on drops
        // it and `cleanup()` unregisters whatever was registered and closes the pool and the
        // session. Before, a failure between the registrations and the final `Ok(Self{..})` (a
        // setter refused below) skipped `cleanup` entirely and left the delegates, and the
        // handler they hold, alive with nothing to stop them.
        let mut api = Self {
            item_with_details,
            _d3d_device: d3d_device.clone(),
            _direct3d_device: direct3d_device.clone(),
            _d3d_device_context: d3d_device_context.clone(),
            frame_pool: Some(frame_pool.clone()),
            session: Some(session.clone()),
            halt: halt.clone(),
            active: false,
            frame_arrived_event_token: None,
            capture_closed_event_token: None,
        };

        // Set capture session closed event
        api.capture_closed_event_token = Some(item.Closed(&TypedEventHandler::<
            GraphicsCaptureItem,
            IInspectable,
        >::new({
            // Init
            let callback_closed = callback.clone();
            let halt_closed = halt.clone();
            let result_closed = result.clone();

            move |_, _| {
                halt_closed.store(true, atomic::Ordering::Relaxed);

                // Notify the user that the capture session is closed.
                let callback_closed = callback_closed.lock().on_closed();
                if let Err(e) = callback_closed {
                    *result_closed.lock() = Some(e);
                }

                // Stop the message loop to allow the thread to exit gracefully.
                quit_capture_thread(thread_id);

                Result::Ok(())
            }
        }))?);

        // Set frame pool frame arrived event
        api.frame_arrived_event_token = Some(frame_pool.FrameArrived(&TypedEventHandler::<
            Direct3D11CaptureFramePool,
            IInspectable,
        >::new({
            // Init. The pool is not captured here: it arrives as the event's sender, and a
            // captured clone was a reference cycle (pool -> delegate -> pool) that only an
            // explicit `RemoveFrameArrived` could break.
            let halt_frame_pool = halt.clone();
            let d3d_device_frame_pool = d3d_device.clone();
            let context = d3d_device_context.clone();
            let result_frame_pool = result;

            let last_size = Arc::new((
                AtomicI32::new(initial_size.Width),
                AtomicI32::new(initial_size.Height),
            ));
            // Failures since the last frame that reached the handler; see
            // `GraphicsCaptureApiHandler::on_frame_pool_error`.
            let pool_errors = AtomicU32::new(0);
            let callback_frame_pool = callback;
            let direct3d_device_recreate = SendDirectX::new(direct3d_device.clone());

            move |frame, _| {
                // Return early if the capture is closed
                if halt_frame_pool.load(atomic::Ordering::Relaxed) {
                    return Ok(());
                }

                // Get frame
                let Some(frame_pool) = frame.as_ref() else {
                    return Ok(());
                };
                // Drain the pool to its newest frame. The pool hands frames out oldest-first, so
                // one event per frame only holds while the consumer keeps up; the moment it does
                // not, every later event replays a backlog that is already up to
                // (buffers - 1) frame periods old. Discarding the stale ones costs nothing and
                // is the difference between "one frame behind" and "permanently behind".
                let Ok(mut frame) = frame_pool.TryGetNextFrame() else {
                    return Ok(());
                };
                let mut skipped = 0u32;
                while let Ok(newer) = frame_pool.TryGetNextFrame() {
                    let _ = frame.Close();
                    frame = newer;
                    skipped += 1;
                }
                if skipped != 0 {
                    stale_frames_dropped.fetch_add(skipped, atomic::Ordering::Relaxed);
                }

                // Everything that can fail between the pool's signal and the handler. A failure
                // is handed to the handler instead of being returned to WinRT, which ignores it.
                let acquired = (|| -> windows::core::Result<_> {
                    // Get frame content size
                    let frame_content_size = frame.ContentSize()?;

                    // Get frame surface
                    let frame_surface = frame.Surface()?;

                    // Convert surface to texture
                    let frame_dxgi_interface =
                        frame_surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
                    let frame_texture =
                        unsafe { frame_dxgi_interface.GetInterface::<ID3D11Texture2D>()? };

                    // Check if the size has been changed
                    if frame_content_size.Width != last_size.0.load(atomic::Ordering::Relaxed)
                        || frame_content_size.Height != last_size.1.load(atomic::Ordering::Relaxed)
                    {
                        let direct3d_device_recreate = &direct3d_device_recreate;
                        // Same buffer count as `Create`. Recreating with 1 quietly re-serializes
                        // capture against encode for the rest of the session, and a content-size
                        // change is exactly a resolution change on the client's virtual display.
                        // The handler is not told: the next frame it receives is simply a
                        // different size, which is what it has to check for.
                        frame_pool.Recreate(
                            &direct3d_device_recreate.0,
                            pixel_format,
                            buffers,
                            frame_content_size,
                        )?;

                        last_size
                            .0
                            .store(frame_content_size.Width, atomic::Ordering::Relaxed);
                        last_size
                            .1
                            .store(frame_content_size.Height, atomic::Ordering::Relaxed);
                    }
                    Ok((frame_surface, frame_texture))
                })();
                let (frame_surface, frame_texture) = match acquired {
                    Ok(acquired) => {
                        pool_errors.store(0, atomic::Ordering::Relaxed);
                        acquired
                    }
                    Err(error) => {
                        let consecutive = pool_errors
                            .fetch_add(1, atomic::Ordering::Relaxed)
                            .saturating_add(1);
                        let verdict = callback_frame_pool
                            .lock()
                            .on_frame_pool_error(&error, consecutive);
                        if let Err(e) = verdict {
                            *result_frame_pool.lock() = Some(e);
                            halt_frame_pool.store(true, atomic::Ordering::Relaxed);
                            quit_capture_thread(thread_id);
                        }
                        return Ok(());
                    }
                };

                // Get texture settings
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { frame_texture.GetDesc(&mut desc) }

                // Create a frame
                let mut frame = Frame::new(
                    frame,
                    &d3d_device_frame_pool,
                    frame_surface,
                    frame_texture,
                    &context,
                    desc,
                    color_format,
                    None,
                );

                // Init internal capture control
                let stop = Arc::new(AtomicBool::new(false));
                let internal_capture_control = InternalCaptureControl::new(stop.clone());

                // Send the frame to the callback struct
                let result = callback_frame_pool
                    .lock()
                    .on_frame_arrived(&mut frame, internal_capture_control);

                // If the user signals to stop or an error occurs, halt the capture.
                if stop.load(atomic::Ordering::Relaxed) || result.is_err() {
                    if let Err(e) = result {
                        *result_frame_pool.lock() = Some(e);
                    }

                    halt_frame_pool.store(true, atomic::Ordering::Relaxed);

                    // Stop the message loop to allow the thread to exit gracefully.
                    quit_capture_thread(thread_id);
                }

                Result::Ok(())
            }
        }))?);

        // Every setter below is optional on some supported Windows build. A property this host
        // does not have is skipped, never fatal: failing here is what routed every Windows 10 and
        // pre-24H2 Windows 11 host onto the DXGI fallback. A setter that exists and *fails*
        // returns through `?`, which drops `api` and runs `cleanup()`.
        if features.cursor_toggle {
            match cursor_capture_settings {
                CursorCaptureSettings::Default => (),
                CursorCaptureSettings::WithCursor => session.SetIsCursorCaptureEnabled(true)?,
                CursorCaptureSettings::WithoutCursor => session.SetIsCursorCaptureEnabled(false)?,
            };
        }

        if features.border_toggle {
            match draw_border_settings {
                DrawBorderSettings::Default => (),
                DrawBorderSettings::WithBorder => session.SetIsBorderRequired(true)?,
                DrawBorderSettings::WithoutBorder => session.SetIsBorderRequired(false)?,
            }
        }

        if features.secondary_windows {
            match secondary_window_settings {
                SecondaryWindowSettings::Default => (),
                SecondaryWindowSettings::Include => session.SetIncludeSecondaryWindows(true)?,
                SecondaryWindowSettings::Exclude => session.SetIncludeSecondaryWindows(false)?,
            }
        }

        if features.min_update_interval {
            match minimum_update_interval_settings {
                MinimumUpdateIntervalSettings::Default => (),
                MinimumUpdateIntervalSettings::Custom(duration) => {
                    session.SetMinUpdateInterval(duration.into())?;
                }
            }
        }

        if features.dirty_region {
            match dirty_region_settings {
                DirtyRegionSettings::Default => (),
                DirtyRegionSettings::ReportOnly => {
                    session.SetDirtyRegionMode(GraphicsCaptureDirtyRegionMode::ReportOnly)?
                }
                DirtyRegionSettings::ReportAndRender => {
                    session.SetDirtyRegionMode(GraphicsCaptureDirtyRegionMode::ReportAndRender)?
                }
            }
        }

        // What the session reports after the setters, once per session. `capture_features` says
        // which properties exist; this says what they hold. A getter that fails reads `?`.
        let read_back = |present: bool, value: Option<String>| -> String {
            if present {
                value.unwrap_or_else(|| "?".to_string())
            } else {
                "absent".to_string()
            }
        };
        tprintln!(
            "wgc: session settings in effect: pool={} buffers={buffers} cursor={} \
             border_required={} min_update_interval={} dirty_region_mode={}",
            if free_threaded {
                "free-threaded"
            } else {
                "dispatcher-queue"
            },
            read_back(
                features.cursor_toggle,
                session.IsCursorCaptureEnabled().ok().map(|v| v.to_string())
            ),
            read_back(
                features.border_toggle,
                session.IsBorderRequired().ok().map(|v| v.to_string())
            ),
            read_back(
                features.min_update_interval,
                session
                    .MinUpdateInterval()
                    .ok()
                    .map(|v| format!("{} us", v.Duration / 10))
            ),
            read_back(
                features.dirty_region,
                session.DirtyRegionMode().ok().map(|v| match v {
                    GraphicsCaptureDirtyRegionMode::ReportOnly => "ReportOnly".to_string(),
                    GraphicsCaptureDirtyRegionMode::ReportAndRender => {
                        "ReportAndRender".to_string()
                    }
                    other => format!("{}", other.0),
                })
            ),
        );

        Ok(api)
    }

    /// Starts the capture.
    ///
    /// # Errors
    ///
    /// - [`Error::AlreadyStarted`] when called more than once
    /// - [`Error::WindowsError`] for failures starting the capture session
    #[inline]
    pub fn start_capture(&mut self) -> Result<(), Error> {
        if self.active {
            return Err(Error::AlreadyStarted);
        }

        if let Some(session) = &self.session {
            session.StartCapture()?;
        }
        self.active = true;

        Ok(())
    }

    /// Stops the capture session and cleans up resources.
    #[inline]
    pub fn stop_capture(mut self) {
        self.cleanup();
    }

    /// Gets the halt handle.
    ///
    /// # Returns
    ///
    /// Returns an `Arc<AtomicBool>` that can be used to check if the capture is halted.
    #[inline]
    #[must_use]
    pub fn halt_handle(&self) -> Arc<AtomicBool> {
        self.halt.clone()
    }

    /// Checks if the Windows Graphics Capture API is supported.
    ///
    /// Delegates to [`wgc_supported`], which is the one predicate the compatibility report uses
    /// too — see its doc comment for why the `UniversalApiContract 8` conjunct upstream ANDed in
    /// here is gone.
    #[inline]
    pub fn is_supported() -> Result<bool, Error> {
        Ok(wgc_supported())
    }

    fn cleanup(&mut self) {
        if let Some(frame_pool) = self.frame_pool.take() {
            if let Some(token) = self.frame_arrived_event_token.take() {
                let _ = frame_pool.RemoveFrameArrived(token);
            }
            let _ = frame_pool.Close();
        }

        if let Some(session) = self.session.take() {
            let _ = session.Close();
        }

        let GraphicsCaptureItemType::Monitor((item, _)) = &self.item_with_details;

        if let Some(token) = self.capture_closed_event_token.take() {
            let _ = item.RemoveClosed(token);
        }
        self.active = false;
    }
}

impl Drop for GraphicsCaptureApi {
    fn drop(&mut self) {
        self.cleanup();
    }
}

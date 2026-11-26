use std::mem;
use std::os::windows::prelude::AsRawHandle;
use std::sync::atomic::{self, AtomicBool, AtomicU32};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};

use parking_lot::Mutex;
use windows::core::Result as WindowsResult;
use windows::Win32::Foundation::{ERROR_INVALID_THREAD_ID, HANDLE, LPARAM, WPARAM};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext};
use windows::Win32::System::Threading::{GetCurrentThreadId, GetThreadId};
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostQuitMessage, PostThreadMessageW, TranslateMessage, MSG,
    WM_QUIT,
};
use windows_future::AsyncActionCompletedHandler;

use super::d3d11::{self, create_d3d_device, AdapterInfo};
use super::frame::Frame;
use super::graphics_capture_api::{self, GraphicsCaptureApi, InternalCaptureControl};
use super::settings::{CursorCaptureSettings, GraphicsCaptureItemType, Settings};
use super::winrt::WinRT;

const fn dispatcher_queue_options() -> DispatcherQueueOptions {
    DispatcherQueueOptions {
        dwSize: mem::size_of::<DispatcherQueueOptions>() as u32,
        threadType: DQTYPE_THREAD_CURRENT,
        apartmentType: DQTAT_COM_NONE,
    }
}

fn run_message_loop<E>() -> Result<(), GraphicsCaptureApiError<E>> {
    let mut message = MSG::default();

    loop {
        match unsafe { GetMessageW(&mut message, None, 0, 0).0 } {
            -1 => return Err(GraphicsCaptureApiError::FailedToRunMessageLoop),
            0 => return Ok(()),
            _ => unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            },
        }
    }
}

fn join_capture_thread<E>(
    thread_handle: JoinHandle<Result<(), GraphicsCaptureApiError<E>>>,
) -> Result<(), CaptureControlError<E>> {
    match thread_handle.join() {
        Ok(result) => {
            result?;
            Ok(())
        }
        Err(_) => Err(CaptureControlError::FailedToJoinThread),
    }
}

#[derive(thiserror::Error, Debug)]
/// Errors that can occur while controlling a running capture session via [`CaptureControl`].
///
/// This error wraps lower-level errors from the Windows Graphics Capture pipeline, as well as
/// thread-control failures when starting/stopping the background capture thread.
pub enum CaptureControlError<E> {
    /// Joining the background capture thread failed (panic or OS-level join error).
    ///
    /// Returned by [`CaptureControl::wait`] and [`CaptureControl::stop`] if the internal thread
    /// panicked or could not be joined.
    #[error("Failed to join thread")]
    FailedToJoinThread,
    /// The [`std::thread::JoinHandle`] was already taken out of the struct (for example by calling
    /// [`CaptureControl::into_thread_handle`]) so the operation cannot proceed.
    #[error("Thread handle is taken out of the struct")]
    ThreadHandleIsTaken,
    /// Failed to post a WM_QUIT message to the capture thread to request shutdown.
    ///
    /// This can happen if the thread is no longer alive or Windows refuses the message.
    #[error("Failed to post thread message")]
    FailedToPostThreadMessage,
    /// The user-provided handler returned an error after capture stopped.
    ///
    /// This variant carries the handler's error type.
    #[error("Stopped handler error: {0}")]
    StoppedHandlerError(E),
    /// A lower-level error from the graphics capture pipeline.
    ///
    /// Wraps [`GraphicsCaptureApiError`].
    #[error("Windows capture error: {0}")]
    GraphicsCaptureApiError(#[from] GraphicsCaptureApiError<E>),
}

/// Used to control the capture session
pub struct CaptureControl<T: GraphicsCaptureApiHandler + Send + 'static, E> {
    thread_handle: Option<JoinHandle<Result<(), GraphicsCaptureApiError<E>>>>,
    halt_handle: Arc<AtomicBool>,
    callback: Arc<Mutex<T>>,
}

impl<T: GraphicsCaptureApiHandler + Send + 'static, E> CaptureControl<T, E> {
    /// Constructs a new [`CaptureControl`].
    #[inline]
    #[must_use]
    pub const fn new(
        thread_handle: JoinHandle<Result<(), GraphicsCaptureApiError<E>>>,
        halt_handle: Arc<AtomicBool>,
        callback: Arc<Mutex<T>>,
    ) -> Self {
        Self {
            thread_handle: Some(thread_handle),
            halt_handle,
            callback,
        }
    }

    /// Checks whether the capture thread has finished.
    #[inline]
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread_handle
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    /// Gets the join handle for the capture thread.
    ///
    /// The capture keeps running: once the handle has been taken, dropping this control no longer
    /// asks the thread to stop.
    #[inline]
    #[must_use]
    pub fn into_thread_handle(mut self) -> JoinHandle<Result<(), GraphicsCaptureApiError<E>>> {
        self.thread_handle.take().unwrap()
    }

    /// Gets the halt handle used to pause the capture thread.
    #[inline]
    #[must_use]
    pub fn halt_handle(&self) -> Arc<AtomicBool> {
        self.halt_handle.clone()
    }

    /// The handler behind its mutex, for calling its methods directly.
    ///
    /// **Locking this blocks for a whole frame.** The `FrameArrived` closure holds the same guard
    /// for the entire callback, and on the classic (non-ring) path that callback is a full encode.
    /// So a `lock()` here can park the calling thread for milliseconds — which is exactly what a
    /// tokio worker or an axum handler must never do. Nothing in ScreenExtend calls this today,
    /// and handler state that needs to be readable from outside should be an `Arc<Atomic*>` the
    /// handler is given at construction (as `stale_frame_counter` is) rather than the handler
    /// itself.
    #[inline]
    #[must_use]
    pub fn callback(&self) -> Arc<Mutex<T>> {
        self.callback.clone()
    }

    /// Waits for the capture thread to stop.
    ///
    /// # Errors
    ///
    /// - [`CaptureControlError::FailedToJoinThread`] when joining the internal thread fails
    /// - [`CaptureControlError::ThreadHandleIsTaken`] when the thread handle was previously taken
    ///   via [`CaptureControl::into_thread_handle`]
    #[inline]
    pub fn wait(mut self) -> Result<(), CaptureControlError<E>> {
        if let Some(thread_handle) = self.thread_handle.take() {
            join_capture_thread(thread_handle)?;
        } else {
            return Err(CaptureControlError::ThreadHandleIsTaken);
        }

        Ok(())
    }

    /// Gracefully requests the capture thread to stop and waits for it to finish.
    ///
    /// This posts a WM_QUIT to the capture thread and joins it.
    ///
    /// # Errors
    ///
    /// - [`CaptureControlError::FailedToPostThreadMessage`] when posting WM_QUIT to the thread
    ///   fails and the thread is still running
    /// - [`CaptureControlError::FailedToJoinThread`] when joining the internal thread fails
    /// - [`CaptureControlError::ThreadHandleIsTaken`] when the thread handle was previously taken
    ///   via [`CaptureControl::into_thread_handle`]
    #[inline]
    pub fn stop(mut self) -> Result<(), CaptureControlError<E>> {
        self.halt_handle.store(true, atomic::Ordering::Relaxed);

        if let Some(thread_handle) = self.thread_handle.take() {
            let handle = thread_handle.as_raw_handle();
            let handle = HANDLE(handle);
            let thread_id = unsafe { GetThreadId(handle) };

            if thread_id == 0 {
                if thread_handle.is_finished() {
                    join_capture_thread(thread_handle)?;
                    return Ok(());
                }

                return Err(CaptureControlError::FailedToPostThreadMessage);
            }

            loop {
                match unsafe {
                    PostThreadMessageW(thread_id, WM_QUIT, WPARAM::default(), LPARAM::default())
                } {
                    Ok(()) => break,
                    Err(error) => {
                        if thread_handle.is_finished() {
                            break;
                        }

                        if error.code()
                            != windows::core::HRESULT::from_win32(ERROR_INVALID_THREAD_ID.0)
                        {
                            return Err(CaptureControlError::FailedToPostThreadMessage);
                        }

                        thread::yield_now();
                    }
                }
            }

            join_capture_thread(thread_handle)?;
        } else {
            return Err(CaptureControlError::ThreadHandleIsTaken);
        }

        Ok(())
    }
}

/// How long a drop keeps re-posting `WM_QUIT` to a capture thread whose message queue does not
/// exist yet. The window is the few instructions between `start_capture` and the thread's first
/// `GetMessageW`; a thread that has not reached it in this long is not helped by a longer wait on
/// a thread that may be an async worker.
const DROP_POST_DEADLINE: std::time::Duration = std::time::Duration::from_millis(50);

/// Dropping a control without [`CaptureControl::stop`] used to detach the capture thread with
/// nothing asking it to leave: it sat in `GetMessageW` for the life of the process, holding the
/// WGC session, the pool's surfaces, the D3D11 device and, through the `FrameArrived` delegate,
/// the handler and everything it owns (an NVENC session, its encode thread, the GPU clock hold).
///
/// This only *signals*: it raises the halt flag, so no further frame reaches the handler, and
/// posts `WM_QUIT`, so the message loop ends and the thread tears the session down itself. It
/// never joins, because a control is dropped wherever its owner happens to be (a cancelled axum
/// handler, a tokio worker) and a join there would park that thread for as long as the capture
/// thread takes to finish a frame. Call [`CaptureControl::stop`] where waiting is acceptable.
impl<T: GraphicsCaptureApiHandler + Send + 'static, E> Drop for CaptureControl<T, E> {
    fn drop(&mut self) {
        let Some(thread_handle) = self.thread_handle.take() else {
            return;
        };
        self.halt_handle.store(true, atomic::Ordering::Relaxed);
        if thread_handle.is_finished() {
            return;
        }
        let thread_id = unsafe { GetThreadId(HANDLE(thread_handle.as_raw_handle())) };
        if thread_id == 0 {
            teprintln!("wgc: dropped capture control could not name its thread; it will not stop");
            return;
        }
        let deadline = std::time::Instant::now() + DROP_POST_DEADLINE;
        loop {
            match unsafe {
                PostThreadMessageW(thread_id, WM_QUIT, WPARAM::default(), LPARAM::default())
            } {
                Ok(()) => return,
                Err(error) => {
                    if thread_handle.is_finished() {
                        return;
                    }
                    let retry = error.code()
                        == windows::core::HRESULT::from_win32(ERROR_INVALID_THREAD_ID.0)
                        && std::time::Instant::now() < deadline;
                    if !retry {
                        teprintln!(
                            "wgc: dropped capture control could not post WM_QUIT ({error}); the \
                             capture thread is halted but still running"
                        );
                        return;
                    }
                    thread::yield_now();
                }
            }
        }
    }
}

#[derive(thiserror::Error, Eq, PartialEq, Clone, Debug)]
/// Errors that can occur while initializing and running the Windows Graphics Capture pipeline.
pub enum GraphicsCaptureApiError<E> {
    /// Joining the worker thread failed (panic or OS-level join error).
    #[error("Failed to join thread")]
    FailedToJoinThread,
    /// Failed to initialize the Windows Runtime for multithreaded apartment.
    ///
    /// Occurs when `RoInitialize(RO_INIT_MULTITHREADED)` returns an error other than `S_FALSE`.
    #[error("Failed to initialize WinRT")]
    FailedToInitWinRT,
    /// Creating the dispatcher queue controller for the message loop failed.
    #[error("Failed to create dispatcher queue controller")]
    FailedToCreateDispatcherQueueController,
    /// Shutting down the dispatcher queue failed.
    #[error("Failed to shut down dispatcher queue")]
    FailedToShutdownDispatcherQueue,
    /// Registering the dispatcher queue completion handler failed.
    #[error("Failed to set dispatcher queue completed handler")]
    FailedToSetDispatcherQueueCompletedHandler,
    /// The Windows message loop for the capture thread failed.
    #[error("Failed to run the capture thread message loop")]
    FailedToRunMessageLoop,
    /// The free-threaded capture worker exited before publishing its control handles.
    #[error("Failed to initialize the capture thread")]
    FailedToStartCaptureThread,
    /// The provided item could not be converted into a `GraphicsCaptureItem`.
    ///
    /// This happens when
    /// [`super::settings::TryIntoCaptureItemWithDetails::try_into_capture_item_with_details`]
    /// fails for the item passed in [`super::settings::Settings`].
    #[error("Failed to convert item to `GraphicsCaptureItem`")]
    ItemConvertFailed,
    /// Underlying Direct3D (D3D11) error.
    ///
    /// Wraps [`super::d3d11::Error`].
    #[error("DirectX error: {0}")]
    DirectXError(#[from] d3d11::Error),
    /// Error produced by the Windows Graphics Capture API wrapper.
    ///
    /// Wraps [`super::graphics_capture_api::Error`].
    #[error("Graphics capture error: {0}")]
    GraphicsCaptureApiError(graphics_capture_api::Error),
    /// Error returned by the user handler when constructing it via
    /// [`GraphicsCaptureApiHandler::new`].
    #[error("New handler error: {0}")]
    NewHandlerError(E),
    /// Error returned by the user handler during frame processing via
    /// [`GraphicsCaptureApiHandler::on_frame_arrived`] or from
    /// [`GraphicsCaptureApiHandler::on_closed`].
    #[error("Frame handler error: {0}")]
    FrameHandlerError(E),
}

/// The context provided to the capture handler.
pub struct Context<Flags> {
    /// The flags that are retrieved from the settings.
    pub flags: Flags,
    /// The Direct3D device.
    pub device: ID3D11Device,
    /// The Direct3D device context.
    pub device_context: ID3D11DeviceContext,
    /// Which adapter the device above was created on.
    pub adapter: AdapterInfo,
}

/// Trait implemented by types that handle graphics capture events.
pub trait GraphicsCaptureApiHandler: Sized {
    /// The type of flags used to get the values from the settings.
    type Flags;

    /// The type of error that can occur during capture. The error will be returned from the
    /// [`CaptureControl`] and [`GraphicsCaptureApiHandler::start`] functions.
    type Error: Send + Sync;

    /// Starts the capture and takes control of the current thread.
    #[inline]
    fn start<T: TryInto<GraphicsCaptureItemType>>(
        settings: Settings<Self::Flags, T>,
    ) -> Result<(), GraphicsCaptureApiError<Self::Error>>
    where
        Self: Send + 'static,
        <Self as GraphicsCaptureApiHandler>::Flags: Send,
    {
        // Initialize WinRT
        let _winrt = WinRT::new().map_err(|_| GraphicsCaptureApiError::FailedToInitWinRT)?;

        // Create a dispatcher queue for the current thread
        let controller = unsafe {
            CreateDispatcherQueueController(dispatcher_queue_options())
                .map_err(|_| GraphicsCaptureApiError::FailedToCreateDispatcherQueueController)?
        };

        // Get current thread ID
        let thread_id = unsafe { GetCurrentThreadId() };

        // Convert the item before building the handler or the device: a monitor handle that went
        // stale fails here, and building them first cost a whole encoder per stale-monitor retry.
        let item = settings
            .item
            .try_into()
            .map_err(|_| GraphicsCaptureApiError::ItemConvertFailed)?;

        // Create Direct3D device and context
        let (d3d_device, d3d_device_context, adapter) = create_d3d_device(settings.gpu_preference)?;

        // Start capture
        let result = Arc::new(Mutex::new(None));

        let ctx = Context {
            flags: settings.flags,
            device: d3d_device.clone(),
            device_context: d3d_device_context.clone(),
            adapter,
        };

        let callback = Arc::new(Mutex::new(
            Self::new(ctx).map_err(GraphicsCaptureApiError::NewHandlerError)?,
        ));
        let (buffers, stale_frames, cursor, free_threaded) = {
            let handler = callback.lock();
            (
                handler.frame_pool_buffers(),
                handler.stale_frame_counter().unwrap_or_default(),
                handler
                    .cursor_capture_override()
                    .unwrap_or(settings.cursor_capture_settings),
                handler.frame_pool_free_threaded(),
            )
        };

        let mut capture = GraphicsCaptureApi::new(
            d3d_device,
            d3d_device_context,
            item,
            callback,
            cursor,
            settings.draw_border_settings,
            settings.secondary_window_settings,
            settings.minimum_update_interval_settings,
            settings.dirty_region_settings,
            settings.color_format,
            buffers,
            free_threaded,
            stale_frames,
            thread_id,
            result.clone(),
        )
        .map_err(GraphicsCaptureApiError::GraphicsCaptureApiError)?;
        capture
            .start_capture()
            .map_err(GraphicsCaptureApiError::GraphicsCaptureApiError)?;

        // Message loop
        run_message_loop()?;

        // Shut down dispatcher queue
        let async_action = controller
            .ShutdownQueueAsync()
            .map_err(|_| GraphicsCaptureApiError::FailedToShutdownDispatcherQueue)?;

        async_action
            .SetCompleted(&AsyncActionCompletedHandler::new(
                move |_, _| -> WindowsResult<()> {
                    unsafe { PostQuitMessage(0) };
                    Ok(())
                },
            ))
            .map_err(|_| GraphicsCaptureApiError::FailedToSetDispatcherQueueCompletedHandler)?;

        // Final message loop
        run_message_loop()?;

        // Stop capture
        capture.stop_capture();

        // Check handler result
        let result = result.lock().take();
        if let Some(e) = result {
            return Err(GraphicsCaptureApiError::FrameHandlerError(e));
        }

        Ok(())
    }

    /// Starts the capture without taking control of the current thread.
    #[inline]
    fn start_free_threaded<T: TryInto<GraphicsCaptureItemType> + Send + 'static>(
        settings: Settings<Self::Flags, T>,
    ) -> Result<CaptureControl<Self, Self::Error>, GraphicsCaptureApiError<Self::Error>>
    where
        Self: Send + 'static,
        <Self as GraphicsCaptureApiHandler>::Flags: Send,
    {
        let (halt_sender, halt_receiver) = mpsc::channel::<Arc<AtomicBool>>();
        let (callback_sender, callback_receiver) = mpsc::channel::<Arc<Mutex<Self>>>();

        // Named so a WPR/ETW trace or a debugger can attribute the thread that runs every frame.
        let builder = thread::Builder::new().name("wgc-capture".to_string());
        let spawned = builder.spawn(
            move || -> Result<(), GraphicsCaptureApiError<Self::Error>> {
                // Initialize WinRT
                let _winrt =
                    WinRT::new().map_err(|_| GraphicsCaptureApiError::FailedToInitWinRT)?;

                // Create a dispatcher queue for the current thread
                let controller = unsafe {
                    CreateDispatcherQueueController(dispatcher_queue_options()).map_err(|_| {
                        GraphicsCaptureApiError::FailedToCreateDispatcherQueueController
                    })?
                };

                // Get current thread ID
                let thread_id = unsafe { GetCurrentThreadId() };

                // Convert the item before building the handler or the device: a monitor handle
                // that went stale fails here, and building them first cost a whole encoder (an
                // NVENC session, its ring thread, the GPU clock hold) per stale-monitor retry.
                let item = settings
                    .item
                    .try_into()
                    .map_err(|_| GraphicsCaptureApiError::ItemConvertFailed)?;

                // Create direct3d device and context
                let (d3d_device, d3d_device_context, adapter) =
                    create_d3d_device(settings.gpu_preference)?;

                // Start capture
                let result = Arc::new(Mutex::new(None));

                let ctx = Context {
                    flags: settings.flags,
                    device: d3d_device.clone(),
                    device_context: d3d_device_context.clone(),
                    adapter,
                };

                let callback = Arc::new(Mutex::new(
                    Self::new(ctx).map_err(GraphicsCaptureApiError::NewHandlerError)?,
                ));
                let (buffers, stale_frames, cursor, free_threaded) = {
                    let handler = callback.lock();
                    (
                        handler.frame_pool_buffers(),
                        handler.stale_frame_counter().unwrap_or_default(),
                        handler
                            .cursor_capture_override()
                            .unwrap_or(settings.cursor_capture_settings),
                        handler.frame_pool_free_threaded(),
                    )
                };

                let mut capture = GraphicsCaptureApi::new(
                    d3d_device,
                    d3d_device_context,
                    item,
                    callback.clone(),
                    cursor,
                    settings.draw_border_settings,
                    settings.secondary_window_settings,
                    settings.minimum_update_interval_settings,
                    settings.dirty_region_settings,
                    settings.color_format,
                    buffers,
                    free_threaded,
                    stale_frames,
                    thread_id,
                    result.clone(),
                )
                .map_err(GraphicsCaptureApiError::GraphicsCaptureApiError)?;

                capture
                    .start_capture()
                    .map_err(GraphicsCaptureApiError::GraphicsCaptureApiError)?;

                // Send halt handle
                let halt_handle = capture.halt_handle();
                halt_sender
                    .send(halt_handle)
                    .map_err(|_| GraphicsCaptureApiError::FailedToStartCaptureThread)?;

                // Send callback
                callback_sender
                    .send(callback)
                    .map_err(|_| GraphicsCaptureApiError::FailedToStartCaptureThread)?;

                // Message loop
                run_message_loop()?;

                // Shutdown dispatcher queue
                let async_action = controller
                    .ShutdownQueueAsync()
                    .map_err(|_| GraphicsCaptureApiError::FailedToShutdownDispatcherQueue)?;

                async_action
                    .SetCompleted(&AsyncActionCompletedHandler::new(
                        move |_, _| -> Result<(), windows::core::Error> {
                            unsafe { PostQuitMessage(0) };
                            Ok(())
                        },
                    ))
                    .map_err(|_| {
                        GraphicsCaptureApiError::FailedToSetDispatcherQueueCompletedHandler
                    })?;

                // Final message loop
                run_message_loop()?;

                // Stop capture
                capture.stop_capture();

                // Check handler result
                let result = result.lock().take();
                if let Some(e) = result {
                    return Err(GraphicsCaptureApiError::FrameHandlerError(e));
                }

                Ok(())
            },
        );
        let Ok(thread_handle) = spawned else {
            return Err(GraphicsCaptureApiError::FailedToStartCaptureThread);
        };

        let Ok(halt_handle) = halt_receiver.recv() else {
            match thread_handle.join() {
                Ok(Err(error)) => return Err(error),
                Ok(Ok(())) => return Err(GraphicsCaptureApiError::FailedToStartCaptureThread),
                Err(_) => {
                    return Err(GraphicsCaptureApiError::FailedToJoinThread);
                }
            }
        };

        let Ok(callback) = callback_receiver.recv() else {
            match thread_handle.join() {
                Ok(Err(error)) => return Err(error),
                Ok(Ok(())) => return Err(GraphicsCaptureApiError::FailedToStartCaptureThread),
                Err(_) => {
                    return Err(GraphicsCaptureApiError::FailedToJoinThread);
                }
            }
        };

        Ok(CaptureControl::new(thread_handle, halt_handle, callback))
    }

    /// Function that will be called to create the struct. The flags can be
    /// passed from settings.
    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error>;

    /// How deep the WinRT frame pool should be for this handler.
    ///
    /// Two is what OBS and Sunshine use for a consumer that encodes inline: one buffer the
    /// compositor is writing, one we hold. A handler that hands the frame to another thread wants
    /// one more so the copy and the encode can overlap. More than that only buys latency, because
    /// the pool is a queue and a queue that fills is a queue you are behind.
    #[inline]
    fn frame_pool_buffers(&self) -> u32 {
        2
    }

    /// Optional counter the pool increments for every frame it discards as stale while draining
    /// to the newest one. Returning `None` means nobody is watching and the count is thrown away.
    #[inline]
    fn stale_frame_counter(&self) -> Option<Arc<AtomicU32>> {
        None
    }

    /// Lets a handler overrule `Settings::cursor_capture_settings` once it has been constructed.
    ///
    /// The cursor setting is the one capture property whose right value is not knowable until the
    /// handler exists: a handler that draws the pointer itself wants
    /// [`CursorCaptureSettings::WithoutCursor`], but whether it *can* is only decided while it
    /// builds its encoder, and getting that wrong in the other direction means a stream with no
    /// pointer at all. Returning `None` — the default, and what every handler that does not care
    /// does — leaves the settings value alone.
    ///
    /// Read once, alongside [`Self::frame_pool_buffers`], before the session is configured.
    #[inline]
    fn cursor_capture_override(&self) -> Option<CursorCaptureSettings> {
        None
    }

    /// Whether the frame pool is created with `CreateFreeThreaded` rather than `Create`.
    ///
    /// `false`, the default, dispatches `FrameArrived` through the capture thread's
    /// `DispatcherQueue` and message pump, so every callback runs on the one thread the handler
    /// was built and tuned on. `true` raises it directly on a WinRT worker thread, which skips
    /// that hop; the handler then cannot assume which thread it is on and has to tune each thread
    /// it finds itself on. Read once, alongside [`Self::frame_pool_buffers`], and honoured only
    /// where the method exists.
    #[inline]
    fn frame_pool_free_threaded(&self) -> bool {
        false
    }

    /// Called when the pool signalled a frame it then could not hand over: `ContentSize`,
    /// `Surface`, the DXGI interface or a `Recreate` failed. Those errors used to be returned to
    /// WinRT, which ignores a delegate's result, so they were invisible, and a `Recreate` that
    /// keeps failing (a removed device, video memory exhausted at a larger size) failed on every
    /// frame for the rest of the session.
    ///
    /// `consecutive` counts failures since the last frame that did reach
    /// [`Self::on_frame_arrived`]. Returning `Err` halts the capture exactly as an
    /// `on_frame_arrived` error does. The default logs the first failure of a run and carries on.
    #[inline]
    fn on_frame_pool_error(
        &mut self,
        error: &windows::core::Error,
        consecutive: u32,
    ) -> Result<(), Self::Error> {
        if consecutive == 1 {
            teprintln!("wgc: the frame pool could not deliver a frame: {error}");
        }
        Ok(())
    }

    /// Called every time a new frame is available.
    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error>;

    /// Optional handler called when the capture item (usually a window) closes.
    #[inline]
    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, PM_NOREMOVE};

    use super::*;

    struct Idle;

    impl GraphicsCaptureApiHandler for Idle {
        type Flags = ();
        type Error = String;

        fn new(_ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
            Ok(Self)
        }

        fn on_frame_arrived(
            &mut self,
            _frame: &mut Frame,
            _capture_control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// wgc-01: a control dropped without `stop()` used to leave its thread in `GetMessageW` for
    /// the life of the process, holding everything the capture owned. The drop must end the loop
    /// without joining.
    #[test]
    fn dropping_a_control_ends_its_capture_thread() {
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let thread_handle =
            thread::spawn(move || -> Result<(), GraphicsCaptureApiError<String>> {
                let mut message = MSG::default();
                // Creates this thread's message queue before the control can post to it.
                let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
                let _ = ready_tx.send(());
                let result = run_message_loop::<String>();
                let _ = done_tx.send(());
                result
            });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the test thread started");
        let halt = Arc::new(AtomicBool::new(false));
        let control = CaptureControl::<Idle, String>::new(
            thread_handle,
            Arc::clone(&halt),
            Arc::new(Mutex::new(Idle)),
        );
        drop(control);
        assert!(
            halt.load(atomic::Ordering::Relaxed),
            "the drop raises the halt flag"
        );
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the capture thread is still parked in its message loop"
        );
    }
}

use baseview::dpi::{LogicalSize as BaseviewLogicalSize, Size};
use baseview::gl::GlConfig;
use baseview::host::{Host, HostCallbacks, HostMainThreadCaller};
use baseview::{
    Error as BaseviewError, Event, HandlerError, Window, WindowContext,
    WindowHandler as BaseviewWindowHandler, WindowSettings, WindowSize,
};
use crossbeam::atomic::AtomicCell;
use nice_plug_core::context::gui::{GuiContext, ParamSetter};
use nice_plug_core::editor::dpi::NativeSize;
use nice_plug_core::editor::ParentWindowHandle as NiceParentWindowHandle;
use nice_plug_core::editor::{Editor, EditorHandle, HostMethods, ResizeHint, SpawnedEditor};
use nice_plug_core::params::persist::PersistentField;
use once_cell::unsync::OnceCell;
use slint::platform::femtovg_renderer::FemtoVGRenderer;
use slint::platform::WindowAdapter;
use slint::platform::WindowEvent;
use slint::{LogicalPosition, PhysicalSize, SharedString};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

pub use baseview::{DropData, DropEffect, EventStatus, MouseEvent};

// Re export slint so users can access it
pub use slint;

use serde::{Deserialize, Serialize};

type EventLoopHandler<T> = dyn Fn(&WindowHandler<T>, ParamSetter, &WindowContext) + Send + Sync;
type SetupHandler<T> = dyn Fn(&WindowHandler<T>, &WindowContext) + Send + Sync;

/// Window size and scale state persisted via nice-plug's `#[persist]` mechanism.
///
/// Put this in your params struct so the host can save and restore the window size:
///
/// ```rust,ignore
/// #[derive(Params)]
/// struct MyParams {
///     #[persist = "editor-state"]
///     editor_state: Arc<SlintEditorState>,
/// }
/// ```
#[derive(Debug, Serialize, Deserialize)]
pub struct SlintEditorState {
    #[serde(with = "nice_plug_core::params::persist::serialize_atomic_cell")]
    pub size: AtomicCell<(u32, u32)>,
    #[serde(
        default = "default_scale_factor",
        with = "nice_plug_core::params::persist::serialize_atomic_cell"
    )]
    scale_factor: AtomicCell<f64>,
}

fn default_width() -> u32 {
    400
}
fn default_height() -> u32 {
    300
}

fn default_scale_factor() -> AtomicCell<f64> {
    AtomicCell::new(1.0)
}

impl<'a> PersistentField<'a, SlintEditorState> for Arc<SlintEditorState> {
    fn set(&self, new_value: SlintEditorState) {
        self.size.store(new_value.size.load());
        self.scale_factor.store(new_value.scale_factor.load());
    }

    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&SlintEditorState) -> R,
    {
        f(self)
    }
}

impl Default for SlintEditorState {
    fn default() -> Self {
        Self {
            size: AtomicCell::new((default_width(), default_height())),
            scale_factor: default_scale_factor(),
        }
    }
}

impl SlintEditorState {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            size: AtomicCell::new((width, height)),
            scale_factor: default_scale_factor(),
        }
    }

    /// Returns a `(width, height)` pair for the current size of the GUI in logical pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size.load()
    }

    /// Returns the last observed window scale factor, or `1.0` before one is observed.
    pub fn scale_factor(&self) -> f64 {
        let scale_factor = self.scale_factor.load();
        if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        }
    }
}

fn native_size(state: &SlintEditorState) -> NativeSize<u32> {
    let (width, height) = state.size();
    NativeSize::from_size(
        Size::Logical(BaseviewLogicalSize::new(width as f64, height as f64)),
        state.scale_factor(),
    )
}

fn resize_hint(resizable: bool) -> ResizeHint {
    if resizable {
        ResizeHint::RESIZABLE
    } else {
        ResizeHint::NON_RESIZABLE
    }
}

/// The nice-plug [`Editor`] implementation for Slint UIs.
///
/// Build one with [`SlintEditor::new`], optionally chaining
/// [`with_setup`][Self::with_setup] and
/// [`with_event_loop`][Self::with_event_loop] to register callbacks and sync
/// parameters each frame. Editors are fixed-size unless
/// [`with_resizable`][Self::with_resizable] is enabled.
///
/// ```rust,ignore
/// fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
///     Some(Box::new(
///         SlintEditor::new(self.params.editor_state.clone(), || gui::AppWindow::new())
///             .with_setup({
///                 let params = self.params.clone();
///                 move |handler, _window_context| {
///                     let component = handler.component();
///                     let context = handler.context().clone();
///                     let params = params.clone();
///                     component.on_gain_changed(move |value| {
///                         let setter = context.param_setter();
///                         setter.begin_set_parameter(&params.gain);
///                         setter.set_parameter_normalized(&params.gain, value);
///                         setter.end_set_parameter(&params.gain);
///                     });
///                 }
///             })
///             .with_event_loop({
///                 let params = self.params.clone();
///                 move |handler, _setter, _window_context| {
///                     handler.component().set_gain(params.gain.value());
///                 }
///             }),
///     ))
/// }
/// ```
pub struct SlintEditor<T: slint::ComponentHandle> {
    component_factory: Arc<dyn Fn() -> Result<T, slint::PlatformError> + Send + Sync>,
    state: Arc<SlintEditorState>,
    event_loop_handler: Arc<EventLoopHandler<T>>,
    setup_handler: Arc<SetupHandler<T>>,
    resizable: bool,
}

impl<T: slint::ComponentHandle + 'static> SlintEditor<T> {
    /// Create an editor from persisted state and a component factory closure.
    pub fn new<F>(state: Arc<SlintEditorState>, factory: F) -> Self
    where
        F: Fn() -> Result<T, slint::PlatformError> + 'static + Send + Sync,
    {
        Self {
            component_factory: Arc::new(factory),
            state,
            event_loop_handler: Arc::new(|_, _, _| {}),
            setup_handler: Arc::new(|_, _| {}),
            resizable: false,
        }
    }

    /// Set the handler called once when the window opens, before the event loop
    /// starts. Use it to register Slint callbacks for UI → plugin communication.
    pub fn with_setup<F>(mut self, handler: F) -> Self
    where
        F: Fn(&WindowHandler<T>, &WindowContext) + 'static + Send + Sync,
    {
        self.setup_handler = Arc::new(handler);
        self
    }

    /// Set the handler called every frame. Use it to push parameter values to the UI
    /// (plugin → UI); register UI → plugin callbacks in [`with_setup`][Self::with_setup].
    pub fn with_event_loop<F>(mut self, handler: F) -> Self
    where
        F: Fn(&WindowHandler<T>, ParamSetter, &WindowContext) + 'static + Send + Sync,
    {
        self.event_loop_handler = Arc::new(handler);
        self
    }

    /// Allow the host or user to resize this editor. Editors are fixed-size by default.
    ///
    /// When enabled, hosts can resize the window and [`WindowHandler::request_resize`]
    /// can request a negotiated size change from the GUI thread.
    pub fn with_resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }
}

/// OpenGL interface implementation for baseview.
///
/// Delegates GL context management and symbol resolution to baseview's
/// `GlContext`, which handles the platform-specific details (WGL on Windows,
/// CGL on macOS, dlsym on Unix).
///
/// The renderer runs on the GUI thread, so the non-`Send` `GlContext` can be
/// stored directly.
#[derive(Clone)]
struct BaseviewOpenGLInterface {
    gl_context: baseview::gl::GlContext,
}

unsafe impl slint::platform::femtovg_renderer::OpenGLInterface for BaseviewOpenGLInterface {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // SAFETY: called on the GUI thread by the FemtoVG renderer before each
        // render pass.
        unsafe { self.gl_context.make_current() }.map_err(|e| e.to_string().into())
    }

    fn swap_buffers(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.gl_context
            .swap_buffers()
            .map_err(|e| e.to_string().into())
    }

    fn resize(
        &self,
        _width: core::num::NonZeroU32,
        _height: core::num::NonZeroU32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // The GL framebuffer follows the window size; nothing to do here.
        Ok(())
    }

    fn get_proc_address(&self, name: &std::ffi::CStr) -> *const core::ffi::c_void {
        self.gl_context.get_proc_address(name)
    }
}

// Thread-local storage for the current adapter
// Holds the active adapter so that BaseviewSlintPlatform::create_window_adapter can
// return it.  Updated each time a window is opened.
thread_local! {
    static CURRENT_ADAPTER: RefCell<Option<Rc<BaseviewSlintAdapter>>> = const { RefCell::new(None) };
}

/// Platform implementation for Slint
struct BaseviewSlintPlatform;

impl slint::platform::Platform for BaseviewSlintPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        CURRENT_ADAPTER.with(|adapter| {
            adapter
                .borrow()
                .clone()
                .map(|a| a as Rc<dyn WindowAdapter>)
                .ok_or_else(|| slint::PlatformError::Other("No adapter set".into()))
        })
    }
}

/// Custom WindowAdapter that bridges baseview and Slint
struct BaseviewSlintAdapter {
    window: slint::Window,
    renderer: OnceCell<FemtoVGRenderer>,
    /// Physical size in actual pixels (for the OpenGL framebuffer)
    physical_size: RefCell<PhysicalSize>,
    /// Scale factor (e.g., 2.0 on Retina displays)
    scale_factor: RefCell<f32>,
    gl_interface: BaseviewOpenGLInterface,
}

impl BaseviewSlintAdapter {
    fn new(
        physical_width: u32,
        physical_height: u32,
        scale_factor: f32,
        gl_context: baseview::gl::GlContext,
    ) -> Rc<Self> {
        Rc::new_cyclic(|weak_self| {
            let window = slint::Window::new(weak_self.clone() as _);
            Self {
                window,
                renderer: OnceCell::new(),
                physical_size: RefCell::new(PhysicalSize::new(physical_width, physical_height)),
                scale_factor: RefCell::new(scale_factor),
                gl_interface: BaseviewOpenGLInterface { gl_context },
            }
        })
    }

    /// Update the size and scale factor (called when window is resized or scale changes)
    fn update_size(&self, physical_width: u32, physical_height: u32, scale_factor: f32) {
        *self.physical_size.borrow_mut() = PhysicalSize::new(physical_width, physical_height);
        *self.scale_factor.borrow_mut() = scale_factor;
    }
}

impl WindowAdapter for BaseviewSlintAdapter {
    fn window(&self) -> &slint::Window {
        &self.window
    }

    fn size(&self) -> PhysicalSize {
        *self.physical_size.borrow()
    }

    fn renderer(&self) -> &dyn slint::platform::Renderer {
        self.renderer.get_or_init(|| {
            FemtoVGRenderer::new(self.gl_interface.clone())
                .expect("Failed to create FemtoVG renderer")
        })
    }

    fn request_redraw(&self) {
        // baseview handles redraws in on_frame
    }
}

/// Per-window state, passed to the event loop handler each frame.
pub struct WindowHandler<T: slint::ComponentHandle> {
    context: GuiContext,
    event_loop_handler: Arc<EventLoopHandler<T>>,
    setup_handler: Arc<SetupHandler<T>>,
    scale_factor: RefCell<f32>,
    pub state: Arc<SlintEditorState>,
    last_cursor_pos: RefCell<LogicalPosition>,
    window_shown: RefCell<bool>,
    component: T,
    adapter: Rc<BaseviewSlintAdapter>,
    window_context: WindowContext,
    prevent_key_event_propagation: RefCell<bool>,
    resizable: bool,
}

impl<T: slint::ComponentHandle> WindowHandler<T> {
    /// Handle a size or scale factor change reported by baseview.
    fn handle_window_size(&self, size: baseview::WindowSize) {
        let scale = size.scale_factor as f32;

        *self.scale_factor.borrow_mut() = scale;
        self.state.scale_factor.store(size.scale_factor);

        // Update adapter with physical size
        self.adapter
            .update_size(size.physical.width, size.physical.height, scale);

        // Update our logical size tracking
        self.state
            .size
            .store((size.logical.width as u32, size.logical.height as u32));

        // Notify Slint of the new size (logical)
        if size.logical.width > 0.0 && size.logical.height > 0.0 {
            self.adapter
                .window
                .dispatch_event(slint::platform::WindowEvent::Resized {
                    size: slint::LogicalSize::new(
                        size.logical.width as f32,
                        size.logical.height as f32,
                    ),
                });
        }

        // Also set the scale factor on the Slint window
        self.adapter
            .window
            .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
    }

    /// Convert a physical baseview position to a logical Slint position.
    fn to_logical_position(
        &self,
        position: baseview::dpi::PhysicalPosition<f64>,
    ) -> LogicalPosition {
        let scale = *self.scale_factor.borrow() as f64;
        LogicalPosition::new((position.x / scale) as f32, (position.y / scale) as f32)
    }

    pub fn component(&self) -> &T {
        &self.component
    }

    pub fn window(&self) -> &slint::Window {
        &self.adapter.window
    }

    pub fn context(&self) -> &GuiContext {
        &self.context
    }

    pub fn set_parameter_normalized(
        &self,
        param: &impl nice_plug_core::params::Param,
        normalized: f32,
    ) {
        self.context
            .param_setter()
            .set_parameter_normalized(param, normalized);
    }

    pub fn begin_set_parameter(&self, param: &impl nice_plug_core::params::Param) {
        self.context.param_setter().begin_set_parameter(param);
    }

    pub fn end_set_parameter(&self, param: &impl nice_plug_core::params::Param) {
        self.context.param_setter().end_set_parameter(param);
    }

    pub fn set_prevent_key_event_propagation(&self, is_enabled: bool) {
        *self.prevent_key_event_propagation.borrow_mut() = is_enabled;
    }

    /// Request a logical window size. The host may deny the resize; in that case
    /// baseview reports the reverted size through `resized()`.
    pub fn request_resize(
        &self,
        width: u32,
        height: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.resize_requester().request_resize(width, height)
    }

    /// Returns a cloneable resize requester for use in Slint callbacks.
    pub fn resize_requester(&self) -> ResizeRequester {
        ResizeRequester {
            window_context: self.window_context.clone(),
            resizable: self.resizable,
        }
    }
}

/// A cloneable GUI-thread handle for requesting a host-negotiated resize.
#[derive(Clone)]
pub struct ResizeRequester {
    window_context: WindowContext,
    resizable: bool,
}

impl ResizeRequester {
    /// Request a logical window size. The host may deny the resize; in that case
    /// baseview reports the reverted size through `resized()`.
    pub fn request_resize(
        &self,
        width: u32,
        height: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if width == 0 || height == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "resize dimensions must be nonzero",
            )
            .into());
        }
        if !self.resizable {
            return Err(
                std::io::Error::other("programmatic resizing is disabled for this editor").into(),
            );
        }
        self.window_context
            .resize(Size::Logical(BaseviewLogicalSize::new(
                width as f64,
                height as f64,
            )))
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
    }
}

impl<T: slint::ComponentHandle + 'static> BaseviewWindowHandler for WindowHandler<T> {
    fn on_frame(&self) -> Result<(), HandlerError> {
        // On first frame: show the component and run the setup handler.
        // The GL context was made current during window creation and is
        // re-ensured by the FemtoVG interface before every render.
        if !*self.window_shown.borrow() {
            self.component.show().expect("Failed to show component");
            *self.window_shown.borrow_mut() = true;

            // This fires once, allowing users to register parameter update callbacks for UI -> plugin one time before the event loop starts
            (self.setup_handler)(self, &self.window_context);
        }

        // Call custom event loop handler
        let setter = self.context.param_setter();
        (self.event_loop_handler)(self, setter, &self.window_context);

        // Update Slint timers and animations
        slint::platform::update_timers_and_animations();

        // Render the component. The FemtoVG backend ensures the GL context is
        // current via our interface and swaps buffers itself.
        if let Some(renderer) = self.adapter.renderer.get() {
            renderer
                .render()
                .map_err(|e| HandlerError::from_boxed(Box::new(e)))?;
        }

        Ok(())
    }

    fn resized(&self, new_size: baseview::WindowSize) -> Result<(), HandlerError> {
        self.handle_window_size(new_size);
        Ok(())
    }

    fn on_event(&self, event: Event) -> EventStatus {
        match event {
            Event::Mouse(mouse_event) => {
                // Convert baseview mouse event to Slint event
                let slint_event = match mouse_event {
                    baseview::MouseEvent::CursorMoved { position, .. } => {
                        let pos = self.to_logical_position(position);
                        *self.last_cursor_pos.borrow_mut() = pos;
                        WindowEvent::PointerMoved { position: pos }
                    }
                    baseview::MouseEvent::ButtonPressed { button, .. } => {
                        let slint_button = match button {
                            baseview::MouseButton::Left => {
                                slint::platform::PointerEventButton::Left
                            }
                            baseview::MouseButton::Right => {
                                slint::platform::PointerEventButton::Right
                            }
                            baseview::MouseButton::Middle => {
                                slint::platform::PointerEventButton::Middle
                            }
                            _ => return EventStatus::Ignored,
                        };
                        WindowEvent::PointerPressed {
                            button: slint_button,
                            position: *self.last_cursor_pos.borrow(),
                        }
                    }
                    baseview::MouseEvent::ButtonReleased { button, .. } => {
                        let slint_button = match button {
                            baseview::MouseButton::Left => {
                                slint::platform::PointerEventButton::Left
                            }
                            baseview::MouseButton::Right => {
                                slint::platform::PointerEventButton::Right
                            }
                            baseview::MouseButton::Middle => {
                                slint::platform::PointerEventButton::Middle
                            }
                            _ => return EventStatus::Ignored,
                        };
                        WindowEvent::PointerReleased {
                            button: slint_button,
                            position: *self.last_cursor_pos.borrow(),
                        }
                    }
                    baseview::MouseEvent::WheelScrolled { delta, .. } => {
                        let (delta_x, delta_y) = match delta {
                            baseview::ScrollDelta::Lines { x, y } => (x * 20.0, y * 20.0),
                            baseview::ScrollDelta::Pixels { x, y } => (x, y),
                        };
                        WindowEvent::PointerScrolled {
                            position: *self.last_cursor_pos.borrow(),
                            delta_x,
                            delta_y,
                        }
                    }
                    baseview::MouseEvent::CursorLeft => WindowEvent::PointerExited,
                    _ => return EventStatus::Ignored,
                };
                self.adapter.window.dispatch_event(slint_event);
                EventStatus::Captured
            }
            Event::Keyboard(key_event) => {
                let text: SharedString = if let keyboard_types::Key::Character(string) =
                    key_event.key
                {
                    string.into()
                } else {
                    match key_event.code {
                        keyboard_types::Code::Enter => slint::platform::Key::Return.into(),
                        keyboard_types::Code::Tab => slint::platform::Key::Tab.into(),
                        keyboard_types::Code::Space => slint::platform::Key::Space.into(),
                        keyboard_types::Code::Backspace => slint::platform::Key::Backspace.into(),
                        keyboard_types::Code::Escape => slint::platform::Key::Escape.into(),
                        keyboard_types::Code::ArrowUp => slint::platform::Key::UpArrow.into(),
                        keyboard_types::Code::ArrowDown => slint::platform::Key::DownArrow.into(),
                        keyboard_types::Code::ArrowLeft => slint::platform::Key::LeftArrow.into(),
                        keyboard_types::Code::ArrowRight => slint::platform::Key::RightArrow.into(),
                        keyboard_types::Code::ShiftLeft => slint::platform::Key::Shift.into(),
                        keyboard_types::Code::ShiftRight => slint::platform::Key::ShiftR.into(),
                        keyboard_types::Code::ControlLeft => slint::platform::Key::Control.into(),
                        keyboard_types::Code::ControlRight => slint::platform::Key::ControlR.into(),
                        keyboard_types::Code::AltLeft => slint::platform::Key::Alt.into(),
                        keyboard_types::Code::AltRight => slint::platform::Key::AltGr.into(),
                        keyboard_types::Code::MetaLeft => slint::platform::Key::Meta.into(),
                        keyboard_types::Code::MetaRight => slint::platform::Key::MetaR.into(),
                        _ => "".into(),
                    }
                };

                if text.is_empty() {
                    return EventStatus::Ignored;
                }

                match key_event.state {
                    keyboard_types::KeyState::Down => {
                        if key_event.repeat {
                            self.adapter
                                .window
                                .dispatch_event(WindowEvent::KeyPressRepeated { text });
                        } else {
                            self.adapter
                                .window
                                .dispatch_event(WindowEvent::KeyPressed { text });
                        }
                    }
                    keyboard_types::KeyState::Up => {
                        self.adapter
                            .window
                            .dispatch_event(WindowEvent::KeyReleased { text });
                    }
                }

                if *self.prevent_key_event_propagation.borrow() {
                    EventStatus::Captured
                } else {
                    EventStatus::Ignored
                }
            }
            Event::Window(window_event) => {
                match window_event {
                    baseview::WindowEvent::Focused => {
                        self.adapter
                            .window
                            .dispatch_event(WindowEvent::WindowActiveChanged(true));
                    }
                    baseview::WindowEvent::Unfocused => {
                        self.adapter
                            .window
                            .dispatch_event(WindowEvent::WindowActiveChanged(false));
                    }
                    baseview::WindowEvent::WillClose => {
                        self.adapter
                            .window
                            .dispatch_event(WindowEvent::CloseRequested);
                    }
                    _ => {}
                }
                EventStatus::Ignored
            }
            _ => EventStatus::Ignored,
        }
    }
}

/// The handle returned by [`SlintEditor::spawn`]. The wrapper uses it to
/// control the baseview window opened for the editor.
pub struct SlintEditorHandle {
    resizable: bool,
}

impl EditorHandle for SlintEditorHandle {
    type Window = Window;
    type Error = BaseviewError;

    fn run_until_closed(window: Self::Window) -> Result<(), Self::Error> {
        window.run_until_closed()
    }

    fn set_parent(
        &self,
        parent: NiceParentWindowHandle,
        window: &Self::Window,
    ) -> Result<(), Self::Error> {
        window.set_parent(baseview::ParentWindowHandle::from_window(&parent))
    }

    fn show(&self, window: &Self::Window) -> Result<(), Self::Error> {
        window.show()
    }

    fn hide(&self, window: &Self::Window) -> Result<(), Self::Error> {
        window.hide()
    }

    fn set_size(
        &self,
        new_size: NativeSize<u32>,
        window: &Self::Window,
    ) -> Result<(), Self::Error> {
        window.resize(new_size)
    }

    fn adjust_size(
        &self,
        new_size: NativeSize<u32>,
        _window: &Self::Window,
    ) -> Option<NativeSize<u32>> {
        self.resizable.then_some(new_size)
    }

    fn set_fallback_scale_factor(
        &self,
        scale_factor: f64,
        window: &Self::Window,
    ) -> Result<(), Self::Error> {
        // Persist the resulting scale only when baseview reports the actual
        // size/scale through `resized()`.
        window.suggest_fallback_scale_factor(scale_factor)
    }

    fn host_main_thread_callback(&self, window: &Self::Window) {
        window.host_main_thread_callback();
    }

    fn param_value_changed(&self, _id: &str, _normalized_value: f32) {}

    fn param_modulation_changed(&self, _id: &str, _modulation_offset: f32) {}
}

/// Adapts nice-plug's re-implementation of baseview's host callbacks to the
/// traits baseview expects on its windows.
struct HostCallbackAdapter {
    host: Box<dyn nice_plug_core::editor::HostCallbacks>,
}

impl HostCallbacks for HostCallbackAdapter {
    fn request_resize(&mut self, new_size: WindowSize) -> Result<(), HandlerError> {
        self.host
            .request_resize(new_size.physical.into(), new_size.scale_factor)
            .map_err(HandlerError::from_boxed)
    }

    fn destroyed(&mut self) {
        self.host.destroyed();
    }
}

struct HostMainThreadCallerAdapter {
    host: Box<dyn nice_plug_core::editor::HostMainThreadCaller>,
}

impl HostMainThreadCaller for HostMainThreadCallerAdapter {
    fn call_main_thread(&mut self) {
        self.host.call_main_thread();
    }
}

impl<T: slint::ComponentHandle + 'static> Editor for SlintEditor<T> {
    type Handle = SlintEditorHandle;

    fn spawn(
        &self,
        parent: Option<NiceParentWindowHandle>,
        wait_for_parent: bool,
        fallback_scale_factor: Option<f64>,
        gui_context: GuiContext,
        host: Option<HostMethods>,
    ) -> Result<SpawnedEditor<Self::Handle>, Box<dyn std::error::Error>> {
        let (width, height) = self.state.size();

        let settings = WindowSettings::new()
            .with_title("Plug-in")
            .with_size(BaseviewLogicalSize::new(width as f64, height as f64))
            .with_parent(parent.as_ref())
            .with_wait_for_parent(wait_for_parent)
            .with_fallback_scale_factor(fallback_scale_factor.or(Some(self.state.scale_factor())))
            .with_resizable(self.resizable)
            // Request OpenGL context for FemtoVG rendering
            .with_gl_config(Some(GlConfig {
                version: (3, 2),
                ..Default::default()
            }));

        let host = host.map(|host| {
            Host::new()
                .with_callbacks(HostCallbackAdapter {
                    host: host.callbacks,
                })
                .with_main_thread(HostMainThreadCallerAdapter {
                    host: host.main_thread_caller,
                })
        });

        let state = self.state.clone();
        let resizable = self.resizable;
        let event_loop_handler = self.event_loop_handler.clone();
        let setup_handler = self.setup_handler.clone();
        let component_factory = self.component_factory.clone();

        let window = Window::create_with_host(
            settings,
            move |window_context| {
                // Make the GL context current so that renderer creation during
                // component initialization (Slint may call renderer() eagerly) has
                // a valid context.
                let gl_context = window_context
                    .gl_context()
                    .expect("window must have an OpenGL context");
                unsafe { gl_context.make_current() }
                    .map_err(|e| HandlerError::from_boxed(Box::new(e)))?;

                // Create the Slint window adapter with the current physical size.
                let size = window_context.size();
                let scale = size.scale_factor as f32;
                state.scale_factor.store(size.scale_factor);
                let adapter = BaseviewSlintAdapter::new(
                    size.physical.width,
                    size.physical.height,
                    scale,
                    gl_context.clone(),
                );

                // Register this adapter so BaseviewSlintPlatform::create_window_adapter
                // returns it.
                CURRENT_ADAPTER.with(|current| {
                    *current.borrow_mut() = Some(adapter.clone());
                });

                // Install our platform on first open; ignored (returns Err) on
                // subsequent opens since Slint only allows setting the platform
                // once per process.
                let _ = slint::platform::set_platform(Box::new(BaseviewSlintPlatform));

                let component = component_factory()
                    .unwrap_or_else(|e| panic!("Failed to create Slint component: {}", e));

                // Defer show() until on_frame so the GL context is current when
                // FemtoVG first renders.

                Ok(WindowHandler {
                    context: gui_context,
                    event_loop_handler,
                    setup_handler,
                    scale_factor: RefCell::new(scale),
                    state,
                    last_cursor_pos: RefCell::new(LogicalPosition::new(0.0, 0.0)),
                    window_shown: RefCell::new(false),
                    component,
                    adapter,
                    window_context,
                    prevent_key_event_propagation: RefCell::new(false),
                    resizable,
                })
            },
            host,
        )?;

        Ok(SpawnedEditor {
            handle: SlintEditorHandle {
                resizable: self.resizable,
            },
            window,
        })
    }

    fn size(&self) -> NativeSize<u32> {
        native_size(&self.state)
    }

    fn resize_hint(&self) -> ResizeHint {
        resize_hint(self.resizable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_persisted_state_defaults_scale_factor_to_one() {
        let state: SlintEditorState = serde_json::from_str(r#"{"size":[320, 200]}"#).unwrap();

        assert_eq!(state.size(), (320, 200));
        assert_eq!(state.scale_factor(), 1.0);
    }

    #[test]
    fn observed_scale_factor_survives_persistence_round_trip() {
        let state = SlintEditorState::new(320, 200);
        state.scale_factor.store(1.75);

        let serialized = serde_json::to_string(&state).unwrap();
        let restored: SlintEditorState = serde_json::from_str(&serialized).unwrap();

        assert_eq!(restored.size(), (320, 200));
        assert_eq!(restored.scale_factor(), 1.75);
    }

    #[test]
    fn persisted_scale_factor_converts_logical_size_to_native_size() {
        let state = SlintEditorState::new(320, 200);
        state.scale_factor.store(2.0);

        let size = native_size(&state);
        #[cfg(target_os = "macos")]
        assert_eq!((size.width, size.height), (320, 200));
        #[cfg(not(target_os = "macos"))]
        assert_eq!((size.width, size.height), (640, 400));
    }

    #[test]
    fn invalid_persisted_scale_factor_falls_back_to_one() {
        let state = SlintEditorState::new(320, 200);
        state.scale_factor.store(f64::NAN);

        assert_eq!(state.scale_factor(), 1.0);
        let size = native_size(&state);
        assert_eq!((size.width, size.height), (320, 200));
    }

    #[test]
    fn resize_hint_is_opt_in() {
        assert_eq!(resize_hint(false), ResizeHint::NON_RESIZABLE);
        assert_eq!(resize_hint(true), ResizeHint::RESIZABLE);
    }
}

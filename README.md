# nice-plug-slint

An adapter for using [Slint](https://slint.dev/) GUIs with [nice-plug](https://codeberg.org/RustAudio/nice-plug) audio plugins. It uses baseview for windowing and FemtoVG (OpenGL) for rendering.

## Example

I took the liberty of creating a simple gain knob VST example project using nice-plug and Nice-Plug-Slint.

please see that here: [Gain Knob](https://github.com/aidan729/Gain-Knob)

## Usage

Add the dependency:

```toml
[dependencies]
nice_plug_slint = { git = "https://github.com/aidan729/nice-plug-slint" }
```

In your plugin:

```rust
use nice_plug_slint::{SlintEditor, SlintEditorState};
use std::sync::Arc;

#[derive(Params)]
struct MyParams {
    #[persist = "editor-state"]
    editor_state: Arc<SlintEditorState>,
}

impl Default for MyParams {
  fn default() -> Self {
    Self {
      editor_state: Arc::new(SlintEditorState::new(400, 300)),
    }
  }
}

fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
    Some(Box::new(
        SlintEditor::new(params.editor_state.clone(), || gui::AppWindow::new())
            .with_setup({
                let params = self.params.clone();
                move |handler, _window_context| {
                    let component = handler.component();
                    let setter = handler.context().param_setter();
                    let params = params.clone();

                    // Register UI -> plugin callbacks once when the window opens
                    component.on_gain_changed(move |value| {
                        setter.begin_set_parameter(&params.gain);
                        setter.set_parameter_normalized(&params.gain, value);
                        setter.end_set_parameter(&params.gain);
                    });
                }
            })
            .with_event_loop({
                let params = self.params.clone();
                move |handler, _setter, _window_context| {
                    // Push parameter values to the UI each frame
                    handler.component().set_gain(params.gain.unmodulated_normalized_value());
                }
            }),
    ))
}
```

## API

### `SlintEditorState`

Holds the window size. Construct with `Arc::new(SlintEditorState::new(w, h))`. It should be stored on your params struct with the `#[persist]` attribute to persist the state across sessions.

### `SlintEditor`

Created with `SlintEditor::new(state, factory)`.

The first argument is the editor state which comes from the params struct. See [SlintEditorState](#slinteditorstate).

The second argument is the factory closure which is called each time the window is opened.

- `.with_setup(handler)` - called once when the window opens, before the event loop starts. Use this to register UI → plugin callbacks.
- `.with_event_loop(handler)` - called every frame. Use this to push parameter values to the UI (plugin → UI). Both handlers receive a `baseview::WindowContext` alongside the handler.

### `WindowHandler`

Passed to the event loop handler. Gives you access to:

- `.component()` - the Slint component
- `.window()` - the Slint window
- `.context()` - nice-plug's `GuiContext`; call `.param_setter()` on it to change parameters

Programmatic resizing is not currently supported in this version.

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for more detail on how the Slint/baseview bridge works internally.

## License

ISC

## Credits

- [nice-plug](https://codeberg.org/RustAudio/nice-plug)
- [Slint](https://slint.dev/) UI toolkit
- [baseview](https://github.com/RustAudio/baseview) for windowing

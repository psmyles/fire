//! The macOS menu bar (D16).
//!
//! A Mac app without a menu bar reads as broken and, more concretely, cannot be quit: ⌘Q is the
//! menu's, not the window's, so without one there is no way out but Force Quit. This builds the
//! minimum that makes Fire behave like a Mac app — the application menu, a File menu, and a Window
//! menu — and routes the two items that are *Fire's* rather than AppKit's back into the same
//! [`KeyAction`] path a keystroke takes.
//!
//! Everything else here is a `muda` **predefined** item, which is deliberate: those map onto
//! AppKit's own responder-chain selectors (`terminate:`, `hide:`, `performMiniaturize:` …), so
//! they behave exactly as macOS users expect, need no routing of ours, and cannot desynchronise
//! from app state because they never touch it.
//!
//! **Accelerators here intercept keys before winit ever sees them.** That is why only the two
//! app items carry one, and why each is given the chord the user has actually bound to that
//! action rather than a hardcoded ⌘O/⌘W: a menu accelerator that disagreed with the keybind would
//! silently shadow it, and the binding would look broken with no way to tell why.
//!
//! For the same reason there is no "Close Window" item, tempting as the Mac convention is: muda's
//! predefined one hardcodes ⌘W, which is already Fire's `CloseImage` chord on both OSes (D9), and
//! two items claiming one accelerator leaves AppKit to pick — silently killing whichever it does
//! not. Closing the *image* while keeping the window is a real command here and it keeps the
//! binding it has on Windows; the red button still closes the window.

use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{AboutMetadata, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use winit::event_loop::EventLoopProxy;

use crate::app::AppEvent;
use crate::keybinds::{KeyAction, KeyChord, Keybinds};

/// Menu-item ids. Only the items Fire performs itself need one; the predefined items are AppKit's.
const ID_OPEN: &str = "fire.open";
const ID_CLOSE: &str = "fire.close";

/// Build the menu bar, install it as the application's, and forward its events into the event
/// loop. Must run on the main thread, after the event loop exists (`NSApplication` has to be up).
///
/// The returned [`Menu`] must be kept alive for the life of the process: dropping it takes the
/// menu bar with it.
pub fn install(proxy: EventLoopProxy<AppEvent>, binds: &Keybinds, product: &str) -> Option<Menu> {
    let about = AboutMetadata {
        name: Some(product.to_string()),
        version: Some(env!("FIRE_VERSION").to_string()),
        copyright: Some(env!("FIRE_COPYRIGHT").to_string()),
        ..Default::default()
    };

    let app_menu = Submenu::with_items(
        product,
        true,
        &[
            &PredefinedMenuItem::about(Some(&format!("About {product}")), Some(about)),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(Some(&format!("Hide {product}"))),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(Some(&format!("Quit {product}"))),
        ],
    )
    .ok()?;

    let file_menu = Submenu::with_items(
        "File",
        true,
        &[
            &MenuItem::with_id(
                ID_OPEN,
                "Open…",
                true,
                accelerator_for(binds, KeyAction::OpenFile),
            ),
            &MenuItem::with_id(
                ID_CLOSE,
                "Close Image",
                true,
                accelerator_for(binds, KeyAction::CloseImage),
            ),
        ],
    )
    .ok()?;

    // No Full Screen item: `Viewer` owns the full-screen state (it drives whether the chrome is
    // drawn), and AppKit's `toggleFullScreen:` would change it behind winit's back and leave that
    // flag stale. Fire's own binding keeps the two in step.
    let window_menu = Submenu::with_items(
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(None),
        ],
    )
    .ok()?;

    let menu = Menu::with_items(&[&app_menu, &file_menu, &window_menu]).ok()?;
    menu.init_for_nsapp();

    // muda delivers on its own channel; hand each event to the event loop so it is processed on
    // the main thread with everything else, in order, rather than racing the viewer's state.
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some(action) = action_for(&event.id) {
            // A closed event loop means the app is exiting; the menu outliving it is not an error.
            let _ = proxy.send_event(AppEvent::MenuCommand(action));
        }
    }));

    Some(menu)
}

/// The [`KeyAction`] an item id stands for.
fn action_for(id: &MenuId) -> Option<KeyAction> {
    match id.as_ref() {
        ID_OPEN => Some(KeyAction::OpenFile),
        ID_CLOSE => Some(KeyAction::CloseImage),
        _ => None,
    }
}

/// The user's own chord for `action`, as a menu accelerator — so the menu shows, and reserves,
/// exactly what is bound. `None` (no accelerator, item still works by clicking) if nothing is
/// bound or the chord has no menu equivalent.
fn accelerator_for(binds: &Keybinds, action: KeyAction) -> Option<Accelerator> {
    // The first chord is the primary one, the same one the tooltips show.
    let chord = binds.chords(action).first()?;
    Some(Accelerator::new(Some(modifiers(chord)), code(chord)?))
}

fn modifiers(chord: &KeyChord) -> Modifiers {
    let mut m = Modifiers::empty();
    // `Primary` is ⌘ here, by the same definition the keybinds use (D9).
    if chord.primary {
        m |= Modifiers::META;
    }
    if chord.shift {
        m |= Modifiers::SHIFT;
    }
    if chord.alt {
        m |= Modifiers::ALT;
    }
    m
}

/// winit's physical `KeyCode` as `keyboard-types`' `Code`. Both are UI Events code names, so this
/// is a rename rather than a mapping — but it is spelled out for the keys the menu can carry
/// rather than done by string parsing, so an unmappable chord is `None` instead of a panic.
fn code(chord: &KeyChord) -> Option<Code> {
    use winit::keyboard::KeyCode as K;
    Some(match chord.key {
        K::KeyA => Code::KeyA,
        K::KeyB => Code::KeyB,
        K::KeyC => Code::KeyC,
        K::KeyD => Code::KeyD,
        K::KeyE => Code::KeyE,
        K::KeyF => Code::KeyF,
        K::KeyG => Code::KeyG,
        K::KeyH => Code::KeyH,
        K::KeyI => Code::KeyI,
        K::KeyJ => Code::KeyJ,
        K::KeyK => Code::KeyK,
        K::KeyL => Code::KeyL,
        K::KeyM => Code::KeyM,
        K::KeyN => Code::KeyN,
        K::KeyO => Code::KeyO,
        K::KeyP => Code::KeyP,
        K::KeyQ => Code::KeyQ,
        K::KeyR => Code::KeyR,
        K::KeyS => Code::KeyS,
        K::KeyT => Code::KeyT,
        K::KeyU => Code::KeyU,
        K::KeyV => Code::KeyV,
        K::KeyW => Code::KeyW,
        K::KeyX => Code::KeyX,
        K::KeyY => Code::KeyY,
        K::KeyZ => Code::KeyZ,
        _ => return None,
    })
}

//! UI framework: tabs are [`View`]s, everything on top (dialogs, menus, fullscreen tools) is a [`Popup`].

pub mod bg;
pub mod diff_view;
pub mod editor;
pub mod popups;
pub mod views;
pub mod widgets;

use ratatui::crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;

/// (key, description) shown in the status bar and the `?` help overlay.
pub type KeyHelp = (&'static str, &'static str);

/// A top-level tab.
///
/// Global keys handled by the app before the view sees them (unless [`captures_input`](View::captures_input)):
/// `1`-`9` switch tabs, `[` / `]` previous/next tab, `?` help, `:` action palette, `q` quit, `F5` refresh.
/// `Ctrl+C` always quits. Everything else (including Tab/Shift+Tab for pane focus) belongs to the view.
pub trait View {
    fn title(&self) -> String;
    /// Reload data from git. Called on first show and after every `Action::Refresh` (lazily for hidden tabs).
    /// Slow loads should start a [`bg::Background`] job here and apply its result in [`poll`](View::poll),
    /// so the UI never waits on git.
    fn refresh(&mut self, ctx: &Ctx) -> anyhow::Result<()>;
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx);
    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action;
    /// Mouse events with absolute coordinates. Returning `Action::None` for a scroll event makes the app
    /// fall back to sending Up/Down keys.
    fn handle_mouse(&mut self, _ev: MouseEvent, _ctx: &Ctx) -> Action {
        Action::None
    }
    fn help(&self) -> Vec<KeyHelp> {
        Vec::new()
    }
    /// True while a text field inside the view has focus, so global single-letter keys are not stolen.
    fn captures_input(&self) -> bool {
        false
    }
    /// Short badge in the tab title (e.g. number of conflicts).
    fn badge(&self) -> Option<String> {
        None
    }
    /// Called about ten times a second, visible or not: pick up results of background loads.
    fn poll(&mut self, _ctx: &Ctx) {}
    /// True while a background load started by `refresh` is running (shown as a spinner).
    fn loading(&self) -> bool {
        false
    }
}

/// Anything drawn above the tabs. Receives all keys while on top. Close by returning `Action::ClosePopup`.
pub trait Popup {
    /// `area` is the whole screen; the popup picks its own rectangle (see [`widgets::centered`]).
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx);
    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action;
    fn handle_mouse(&mut self, _ev: MouseEvent, _ctx: &Ctx) -> Action {
        Action::None
    }
    /// Called after `Action::Refresh`.
    fn refresh(&mut self, _ctx: &Ctx) {}
    /// Called about ten times a second, for popups waiting on background work; may close itself
    /// or open something else by returning an action.
    fn poll(&mut self, _ctx: &Ctx) -> Action {
        Action::None
    }
    fn help(&self) -> Vec<KeyHelp> {
        Vec::new()
    }
    /// Gets every key, Ctrl+C too (the built-in editor, which must not lose unsaved text).
    fn captures_everything(&self) -> bool {
        false
    }
}

/// An entry in the `:` action palette.
pub struct PaletteItem {
    pub name: &'static str,
    pub run: fn(&Ctx) -> Action,
}

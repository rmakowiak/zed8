//! A one-line text input that calls back on Enter.

use editor::{Editor, actions::Newline};
use gpui::{App, AppContext as _, Context, Entity, Subscription, Window};
use std::rc::Rc;

/// The input and the subscriptions that keep its Enter handlers alive.
/// A single-line editor has no Enter binding of its own: Enter reaches it as
/// `menu::Confirm`. `editor::Newline` is handled too, for other keymaps.
pub fn single_line<T: 'static>(
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut Context<T>,
    on_submit: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> (Entity<Editor>, [Subscription; 2]) {
    let input = cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor
    });
    let owner = cx.entity().downgrade();
    let on_submit = Rc::new(on_submit);
    let submit = move |window: &mut Window, cx: &mut App| {
        let on_submit = on_submit.clone();
        owner.update(cx, |owner, cx| on_submit(owner, window, cx)).ok();
    };
    let subscriptions = input.update(cx, |input, _| {
        let confirm = submit.clone();
        [
            input.register_action(move |_: &menu::Confirm, window, cx| confirm(window, cx)),
            input.register_action(move |_: &Newline, window, cx| submit(window, cx)),
        ]
    });
    (input, subscriptions)
}

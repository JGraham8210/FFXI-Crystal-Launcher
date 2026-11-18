//! The picking controls on the real window, headless: a real pointer drag over the rail's
//! checkboxes paints them (saved once, on release), and the header checkbox and the Select menu
//! reach the core and draw back.
use super::*;
use crate::selection::Header;
use std::collections::BTreeSet;

/// Each account row's checkbox, as its centre in window coordinates, in row order (the header's
/// checkbox, the same size, is left out).
fn pick_boxes(window: &slint::Window) -> Vec<slint::LogicalPosition> {
    let inner = WindowInner::from_pub(window);
    inner.ensure_tree_instantiated();
    let start = ItemRc::new_root(inner.component());
    let mut item = start.clone();
    let mut found: Vec<slint::LogicalPosition> = Vec::new();
    for _ in 0..100_000 {
        if item.is_visible() {
            if let Some(area) = item.downcast::<i_slint_core::items::TouchArea>() {
                let geometry = item.geometry();
                let at = item.map_to_window(geometry.origin);
                let size = (geometry.width(), geometry.height());
                if format!("{:?}", area.as_pin_ref().mouse_cursor()).contains("Pointer")
                    && (17.0..19.0).contains(&size.0) && (17.0..19.0).contains(&size.1) && at.x < 360.0 {
                    let centre = slint::LogicalPosition::new(at.x + size.0 / 2.0, at.y + size.1 / 2.0);
                    // The Check's own hit area sits under the paint overlay at the same place.
                    if !found.iter().any(|seen| (seen.y - centre.y).abs() < 1.0) { found.push(centre); }
                }
            }
        }
        item = item.next_focus_item();
        if item == start { break; }
    }
    found.sort_by(|a, b| a.y.total_cmp(&b.y));
    found.remove(0);
    found
}

#[test]
fn a_pointer_drag_over_the_checkboxes_paints_them_and_saves_once_on_release() {
    use slint::platform::{PointerEventButton, WindowEvent};
    crate::privacy::set(false);
    crate::i18n::set(Lang::En);
    let (ui, window) = main_window();
    let mut app = app(Page::Home, None, false, false);
    for row in &mut app.accounts { row.selected = false; }
    app.settings.selected_accounts = Some(BTreeSet::new());
    render(&ui, &window, &app);
    let boxes = pick_boxes(ui.window());
    assert!(boxes.len() >= 3, "a checkbox per row: {boxes:?}");
    let dispatch = |event| ui.window().dispatch_event(event);
    dispatch(WindowEvent::PointerMoved { position: boxes[0] });
    dispatch(WindowEvent::PointerPressed { position: boxes[0], button: PointerEventButton::Left });
    pump(&ui, &window, &mut app);
    assert!(app.selection.painting() && app.accounts[0].selected, "the press starts the paint");
    assert!(ui.global::<crate::slint_host::Picking>().get_painting());
    let steps = 10;
    for step in 1..=steps {
        let y = boxes[0].y + (boxes[2].y - boxes[0].y) * step as f32 / steps as f32;
        dispatch(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(boxes[0].x, y) });
        pump(&ui, &window, &mut app);
    }
    let picked: Vec<bool> = app.accounts.iter().map(|row| row.selected).collect();
    assert_eq!(&picked[..3], [true, true, true], "every row crossed took the value");
    assert_eq!(app.settings.selected_accounts, Some(BTreeSet::new()), "nothing is saved mid-drag");
    dispatch(WindowEvent::PointerReleased { position: boxes[2], button: PointerEventButton::Left });
    pump(&ui, &window, &mut app);
    assert!(!app.selection.painting() && !ui.global::<crate::slint_host::Picking>().get_painting());
    assert_eq!(app.settings.selected_accounts.as_ref().map(BTreeSet::len), Some(3), "saved once, on release");
}

#[test]
fn the_header_checkbox_and_the_select_menu_reach_the_core_and_draw_back() {
    crate::privacy::set(false);
    crate::i18n::set(Lang::En);
    let (ui, window) = main_window();
    let mut app = app(Page::Home, None, false, false);
    for row in &mut app.accounts { row.selected = false; }
    render(&ui, &window, &app);
    let picking = ui.global::<crate::slint_host::Picking>();
    assert_eq!(picking.get_state(), 0);
    picking.invoke_toggle_all();
    pump(&ui, &window, &mut app);
    assert_eq!(app.header_state(), Header::All);
    assert_eq!(picking.get_state(), 2);
    // Select menu entry 2 is Invert; "Save selection as" stores the picks as a group.
    picking.invoke_pick(2, false);
    pump(&ui, &window, &mut app);
    assert_eq!(picking.get_state(), 0);
    picking.invoke_pick(0, false);
    picking.invoke_save_group("Everyone".into());
    pump(&ui, &window, &mut app);
    assert_eq!(app.settings.account_groups.len(), 1);
    assert_eq!(slint::Model::row_count(&picking.get_groups()), 1);
    picking.invoke_pick(1, false);
    pump(&ui, &window, &mut app);
    assert_eq!(picking.get_state(), 0);
    // Entry 6 is the first group; Ctrl adds (here to nothing picked).
    picking.invoke_pick(6, true);
    pump(&ui, &window, &mut app);
    assert_eq!(picking.get_state(), 2);
    // The menu itself draws its entries and the group.
    picking.set_open(true);
    let shown = render(&ui, &window, &app);
    for word in ["All", "None", "Invert", "In game", "Signed in", "Not signed in", "Everyone", "Save selection as…"] {
        assert!(shown.iter().any(|text| text == word), "{word} in {shown:?}");
    }
    picking.set_open(false);
}

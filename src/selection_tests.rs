use super::*;
use std::collections::BTreeSet;

fn keys(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("k{i}")).collect()
}

fn group(name: &str, members: &[&str]) -> Group {
    Group { name: name.into(), keys: members.iter().map(|k| (*k).to_owned()).collect() }
}

#[test]
fn paint_gives_every_crossed_row_the_value_the_first_box_changed_to() {
    let mut picked = vec![false, false, true, false, false, false];
    // The first box goes off to on: the rows crossed turn on, including the one already on.
    let mut paint = Paint::start(&mut picked, 1);
    assert_eq!(picked, [false, true, true, false, false, false]);
    // A fast drag skips rows: the ones between are filled too.
    paint.over(&mut picked, 4);
    assert_eq!(picked, [false, true, true, true, true, false]);
    // Going back over rows keeps what they took.
    paint.over(&mut picked, 2);
    assert_eq!(picked, [false, true, true, true, true, false]);
    // The other way: the first box goes on to off, and rows above it clear.
    let mut picked = vec![true; 5];
    let mut paint = Paint::start(&mut picked, 3);
    paint.over(&mut picked, 0);
    assert_eq!(picked, [false, false, false, false, true]);
}

#[test]
fn a_range_fills_in_either_direction_and_stays_inside_the_list() {
    let mut picked = vec![false; 5];
    fill(&mut picked, 1, 3, true);
    assert_eq!(picked, [false, true, true, true, false]);
    fill(&mut picked, 3, 0, false);
    assert_eq!(picked, [false, false, false, false, false]);
    fill(&mut picked, 3, 99, true);
    assert_eq!(picked, [false, false, false, true, true]);
    fill(&mut [], 0, 3, true);
}

#[test]
fn the_header_checkbox_picks_all_unless_all_are_picked() {
    assert_eq!(header(&[]), Header::None);
    assert_eq!(header(&[false, false]), Header::None);
    assert_eq!(header(&[true, false]), Header::Some);
    assert_eq!(header(&[true, true]), Header::All);
    for (start, after) in [([false, false, false], [true; 3]), ([true, false, true], [true; 3]), ([true; 3], [false; 3])] {
        let mut picked = start;
        header_click(&mut picked);
        assert_eq!(picked, after);
    }
}

#[test]
fn the_select_menu_replaces_the_picks_and_ctrl_adds_to_them() {
    let keys = keys(4);
    let facts = [
        Facts { in_game: true, signed_in: true },
        Facts { in_game: false, signed_in: true },
        Facts { in_game: false, signed_in: false },
        Facts { in_game: true, signed_in: true },
    ];
    let groups = [group("Crafters", &["k2", "k3", "gone"])];
    let run = |start: [bool; 4], what, add| {
        let mut picked = start;
        pick(&mut picked, &keys, &facts, &groups, what, add);
        picked
    };
    let some = [true, false, false, false];
    assert_eq!(run(some, Pick::All, false), [true; 4]);
    assert_eq!(run(some, Pick::None, false), [false; 4]);
    assert_eq!(run(some, Pick::Invert, false), [false, true, true, true]);
    assert_eq!(run(some, Pick::InGame, false), [true, false, false, true]);
    assert_eq!(run(some, Pick::SignedIn, false), [true, true, false, true]);
    assert_eq!(run(some, Pick::NotSignedIn, false), [false, false, true, false]);
    assert_eq!(run(some, Pick::Group(0), false), [false, false, true, true]);
    // Ctrl adds to what is picked; None adds nothing; Invert is the same either way.
    assert_eq!(run([false, true, false, false], Pick::InGame, true), [true, true, false, true]);
    assert_eq!(run(some, Pick::Group(0), true), [true, false, true, true]);
    assert_eq!(run(some, Pick::None, true), some);
    assert_eq!(run(some, Pick::Invert, true), [false, true, true, true]);
    // A group that was deleted since the menu drew changes nothing.
    assert_eq!(run(some, Pick::Group(5), false), some);
    assert_eq!(Pick::from_entry(5), Pick::NotSignedIn);
    assert_eq!(Pick::from_entry(8), Pick::Group(2));
}

#[test]
fn groups_save_rename_delete_and_follow_the_accounts() {
    let mut groups = Vec::new();
    assert_eq!(save_group(&mut groups, "   ", keys(2)), None);
    assert_eq!(save_group(&mut groups, " Crafters ", keys(2)), Some(0));
    assert_eq!(save_group(&mut groups, "Mules", keys(1)), Some(1));
    // The same name (any case) is replaced, not doubled.
    assert_eq!(save_group(&mut groups, "crafters", keys(3)), Some(0));
    assert_eq!(groups.len(), 2);
    assert_eq!((groups[0].name.as_str(), groups[0].keys.len()), ("Crafters", 3));
    assert_eq!(rename_group(&mut groups, 1, "Mules"), Ok(()));
    assert_eq!(rename_group(&mut groups, 1, "CRAFTERS"), Err(RenameError::Taken));
    assert_eq!(rename_group(&mut groups, 1, " "), Err(RenameError::Empty));
    assert_eq!(rename_group(&mut groups, 7, "x"), Err(RenameError::Missing));
    assert_eq!(rename_group(&mut groups, 1, "Alts"), Ok(()));
    assert_eq!(groups[1].name, "Alts");
    rename_key(&mut groups, "k0", "login");
    assert_eq!(groups[0].keys, ["login", "k1", "k2"]);
    assert_eq!(groups[1].keys, ["login"]);
    prune(&mut groups, &BTreeSet::from(["login".to_owned(), "k2".to_owned()]));
    assert_eq!(groups[0].keys, ["login", "k2"]);
    assert_eq!(groups[1].keys, ["login"]);
    delete_group(&mut groups, 0);
    delete_group(&mut groups, 9);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].name, "Alts");
}

/// The preview's accounts, extended to `count` by copies with their own logins.
fn app_of(count: usize) -> App {
    let mut app = crate::tests::app();
    let mut values: Vec<_> = app.accounts.iter().map(|row| row.account.clone()).collect();
    for i in values.len()..count {
        let mut extra = values[0].clone();
        extra.label = format!("Extra {i}");
        extra.pol_id = format!("XYZ{i:05}");
        values.push(extra);
    }
    app.replace_accounts(values);
    app
}

fn rows(app: &App) -> Vec<bool> {
    app.accounts.iter().map(|row| row.selected).collect()
}

fn key(app: &App, i: usize) -> String {
    app.accounts[i].key.clone()
}

#[test]
fn a_paint_drag_saves_once_on_release_and_a_cancel_puts_the_picks_back() {
    let mut app = app_of(6);
    for row in &mut app.accounts {
        row.selected = false;
    }
    app.settings.selected_accounts = Some(BTreeSet::new());
    let first = key(&app, 0);
    app.selection_command(Command::Press(first, false));
    app.selection_command(Command::Over(key(&app, 2)));
    assert_eq!(&rows(&app)[..4], [true, true, true, false]);
    assert!(app.selection.painting());
    assert_eq!(app.settings.selected_accounts, Some(BTreeSet::new()), "nothing is saved while painting");
    app.finish_paint();
    assert!(!app.selection.painting());
    assert_eq!(app.settings.selected_accounts.as_ref().map(BTreeSet::len), Some(3));
    // A second drag, abandoned: back to the three.
    let last = key(&app, 3);
    app.selection_command(Command::Press(last, false));
    app.selection_command(Command::Over(key(&app, 1)));
    assert!(rows(&app)[1..4].iter().all(|on| *on));
    app.cancel_paint();
    assert_eq!(&rows(&app)[..4], [true, true, true, false]);
    assert_eq!(app.settings.selected_accounts.as_ref().map(BTreeSet::len), Some(3));
}

#[test]
fn shift_click_sets_the_range_from_the_last_toggled_row_to_the_clicked_value() {
    let mut app = app_of(6);
    for row in &mut app.accounts {
        row.selected = false;
    }
    app.toggle_account(key(&app, 0), true);
    app.selection_command(Command::Press(key(&app, 3), true));
    assert_eq!(&rows(&app)[..4], [true, true, true, true]);
    assert!(!app.selection.painting(), "a range is not a drag");
    // The clicked box was off, so the range takes on; now row 1 is on, so the next range (from
    // row 3, the last toggled) clears.
    app.selection_command(Command::Press(key(&app, 1), true));
    assert_eq!(&rows(&app)[..4], [true, false, false, false], "from row 3 back to row 1, cleared");
    assert_eq!(app.settings.selected_accounts.as_ref().map(BTreeSet::len), Some(rows(&app).iter().filter(|on| **on).count()));
    // Without an anchor in the list, Shift is a plain click.
    app.selection.anchor = Some("deleted".into());
    app.selection_command(Command::Press(key(&app, 0), true));
    assert!(app.selection.painting());
    app.finish_paint();
}

#[test]
fn paint_and_reorder_ignore_each_other() {
    let mut app = app_of(6);
    let before = rows(&app);
    app.account_drag.source = Some(key(&app, 0));
    app.selection_command(Command::Press(key(&app, 1), false));
    assert!(!app.selection.painting());
    assert_eq!(rows(&app), before);
    app.account_drag.source = None;
    app.selection_command(Command::Press(key(&app, 1), false));
    assert!(app.selection.painting());
    // A second press while painting is ignored, and Over without a paint does nothing.
    app.selection_command(Command::Press(key(&app, 2), false));
    app.finish_paint();
    let after = rows(&app);
    app.selection_command(Command::Over(key(&app, 0)));
    assert_eq!(rows(&app), after);
}

#[test]
fn menu_picks_groups_and_the_keyboard_extend_save_the_selection() {
    let mut app = app_of(6);
    for row in &mut app.accounts {
        row.selected = false;
    }
    app.selection_command(Command::Extend(key(&app, 1), key(&app, 2)));
    assert_eq!(&rows(&app)[..4], [false, true, true, false]);
    app.selection_command(Command::SaveGroup("Two".into()));
    assert_eq!(app.settings.account_groups, [Group { name: "Two".into(), keys: vec![key(&app, 1), key(&app, 2)] }]);
    // The header checkbox: some picked becomes all, all becomes none.
    app.selection_command(Command::ToggleAll);
    assert!(rows(&app).iter().all(|on| *on));
    assert_eq!(app.header_state(), Header::All);
    app.selection_command(Command::ToggleAll);
    assert!(rows(&app).iter().all(|on| !on));
    assert_eq!(app.header_state(), Header::None);
    app.selection_command(Command::Pick(Pick::None, false));
    assert!(rows(&app).iter().all(|on| !on));
    assert_eq!(app.settings.selected_accounts, Some(BTreeSet::new()));
    app.selection_command(Command::Pick(Pick::Group(0), false));
    assert_eq!(&rows(&app)[..4], [false, true, true, false]);
    app.selection_command(Command::Pick(Pick::Invert, false));
    assert_eq!(&rows(&app)[..4], [true, false, false, true]);
    app.selection_command(Command::RenameGroup(0, "Two".into()));
    app.selection_command(Command::SaveGroup("Other".into()));
    app.selection_command(Command::RenameGroup(1, "two".into()));
    assert!(app.status.contains("two"), "{}", app.status);
    assert_eq!(app.settings.account_groups[1].name, "Other");
    app.selection_command(Command::DeleteGroup(0));
    assert_eq!(app.settings.account_groups.len(), 1);
}

/// The footer states its reach: Play counts the picked accounts that can start, Shut down the
/// picked games that can end (a picked account that is already playing is no Play).
#[test]
fn the_footer_buttons_count_the_eligible_part_of_the_selection() {
    use std::sync::{atomic::AtomicBool, Arc};
    let mut app = app_of(6);
    for (i, row) in app.accounts.iter_mut().enumerate() {
        row.selected = i < 3;
        row.busy = false;
    }
    let startable = app.selected_count();
    assert_eq!(app.shutdown_count(true), 0);
    // One of the three picked is in game; another game runs that is not picked.
    for i in [0, 3] {
        let row = &mut app.accounts[i];
        row.pid = Some(100 + i as u32);
        row.game_running = Some(Arc::new(AtomicBool::new(true)));
        row.alive = Some(Arc::new(AtomicBool::new(true)));
    }
    assert_eq!(app.shutdown_count(true), 1);
    assert_eq!(app.shutdown_count(false), 2);
    assert_eq!(app.selected_count(), startable - 1, "the picked game is no longer startable");
    assert_eq!(shutdown_label(false, app.shutdown_count(true)), "Shut down 1");
    assert_eq!(shutdown_label(true, 0), crate::i18n::t("Shutting down…"));
}

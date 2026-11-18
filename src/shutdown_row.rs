//! An account row's side of Crystal's Shut down: a game it was used for ends as the user's
//! close (no offer, no relaunch, no "View log", "FFXI closed."), an in-world game still open
//! after the graceful wait offers Close now, and a game on its fatal network dialog shows
//! Connection lost.
use crate::launch_log::ExitReport;
use crate::session_worker::game_shutdown::Refusal;
use crate::i18n::t;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};

/// The shut-down state a row keeps beside its session.
#[derive(Default)]
pub(crate) struct Closing {
    /// The session's "the game shows its fatal network dialog" flag (`friends::Snapshot`).
    pub connection_lost: Option<Arc<AtomicBool>>,
    /// Crystal's Shut down (or Close now) was used for the running game: its end is the user's.
    pub used: bool,
    /// The graceful shut-down was accepted in the world but the game stayed open: the row's
    /// action is Close now (the instant path), never an automatic kill.
    pub close_now: bool,
}

impl crate::AccountRow {
    /// The running game shows its fatal network dialog.
    pub(crate) fn connection_lost(&self) -> bool {
        self.is_playing() && self.closing.connection_lost.as_ref().is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    /// The row's game ended: whether it was the user's close through Shut down. The row's
    /// status says "FFXI closed." then, whatever the report says (the game log keeps it).
    pub(crate) fn shutdown_end(&mut self, report: Option<&ExitReport>) -> bool {
        let used = std::mem::take(&mut self.closing.used);
        self.closing.close_now = false;
        self.closing.connection_lost = None;
        if used && report.is_some() {
            self.problem = false;
            self.exit = None;
            self.status = t("FFXI closed.").into();
        }
        used
    }

    /// A shut-down's result for this row's game.
    pub(crate) fn shutdown_finished(&mut self, result: Result<(), Refusal>, sign_in_after_game: bool, preview: bool) {
        self.shutting_down = false;
        match result {
            Err(refusal) => {
                // Still open in the world: the user's close stands, and Close now finishes it.
                self.closing.used = refusal.close_now;
                self.closing.close_now = refusal.close_now;
                self.problem = true;
                self.status = refusal.message;
            }
            Ok(()) if self.game_seen => {
                self.problem = false;
                self.status = if sign_in_after_game { t("FFXI closed. Returning to PlayOnline…") } else { t("FFXI and PlayOnline closed.") }.into();
                // The monitor publishes the exit report before `alive = false`. Let Tick consume
                // both, whether it runs before or after this result, so automatic sign-in is
                // scheduled once with the actual exit reason.
                if preview { self.alive = Some(Arc::new(AtomicBool::new(false))); }
            }
            Ok(()) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::account_row::{row_state, row_view, RowState};
    use crate::i18n::t;
    use crate::Message;
    use std::sync::{atomic::AtomicBool, Arc};

    fn running(app: &mut crate::App) {
        for (i, row) in app.accounts.iter_mut().enumerate() {
            row.pid = Some(100 + i as u32);
            row.busy = false;
            row.game_running = Some(Arc::new(AtomicBool::new(true)));
            row.alive = Some(Arc::new(AtomicBool::new(true)));
        }
    }

    /// A fatal network dialog shows Connection lost with an error lamp and Shut down, and goes
    /// back to In game when the flag clears.
    #[test]
    fn a_game_on_its_fatal_dialog_shows_connection_lost_with_shut_down() {
        crate::i18n::set(crate::i18n::Lang::En);
        let mut app = crate::tests::app();
        running(&mut app);
        let flag = Arc::new(AtomicBool::new(false));
        app.accounts[0].closing.connection_lost = Some(flag.clone());
        assert_eq!(row_state(&app, &app.accounts[0]), RowState::Playing);
        flag.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(row_state(&app, &app.accounts[0]), RowState::ConnectionLost);
        let row = row_view(&app, &app.accounts[0]);
        assert_eq!(row.label, t("Connection lost"));
        assert!(matches!(&row.action, Some((label, Message::ShutdownAccount(_), false)) if *label == t("Shut down")));
        // No game: no Connection lost.
        app.accounts[0].game_running = Some(Arc::new(AtomicBool::new(false)));
        assert_ne!(row_state(&app, &app.accounts[0]), RowState::ConnectionLost);
    }

    /// Shut down ends the game as the user's close even when the report says it lost its
    /// connection, and a graceful wait that ran out offers Close now, which takes the instant path.
    #[test]
    fn shut_down_ends_neutrally_and_a_stuck_close_offers_close_now() {
        use crate::session_worker::game_shutdown::{Mode, Refusal};
        crate::i18n::set(crate::i18n::Lang::En);
        let mut app = crate::tests::app();
        running(&mut app);
        let key = app.accounts[0].key.clone();
        let generation = app.accounts[0].launch_generation;
        assert_eq!(app.update(Message::ShutdownAccount(key.clone())).units(), 1);
        assert!(app.accounts[0].closing.used);
        let stuck = Refusal { message: "FFXI or PlayOnline is still open.".into(), close_now: true };
        let _ = app.update(Message::GameShutdownFinished(key.clone(), generation, Err(stuck)));
        let row = row_view(&app, &app.accounts[0]);
        assert_eq!(row.label, t("In game"));
        assert_eq!(row.detail, "FFXI or PlayOnline is still open.", "the row keeps its reason");
        assert!(matches!(&row.action, Some((label, Message::CloseNow(k), false)) if *label == t("Close now") && *k == key));
        assert_eq!(app.update(Message::CloseNow(key.clone())).units(), 1);
        assert!(app.accounts[0].shutting_down && app.accounts[0].shutdown_mode == Mode::Instant);
        let _ = app.update(Message::GameShutdownFinished(key.clone(), generation, Ok(())));
        // The host ends; its report says it lost its connection.
        let lost = crate::launch_log::classify_loader_end(&crate::launch_log::LoaderEnd::Disconnected, std::time::Duration::from_secs(900));
        *app.accounts[0].outcome.get_or_insert_with(Default::default).lock().unwrap() = Some(lost.clone());
        app.accounts[0].alive.as_ref().unwrap().store(false, std::sync::atomic::Ordering::Release);
        let _ = app.update(Message::Tick);
        let row = &app.accounts[0];
        assert!(!row.problem && !row.closing.used && !row.closing.close_now);
        assert!(row.status_text().starts_with(t("FFXI closed.")), "{}", row.status_text());
        assert_ne!(row_state(&app, &app.accounts[0]), RowState::Problem, "no View log");
        assert_eq!(crate::features::recovery::classify(Some(&lost), true, false), crate::features::recovery::EndKind::UserClosed);
        // A refusal that is not a stuck close leaves no Close now and no user's close behind.
        running(&mut app);
        let key = app.accounts[1].key.clone();
        let generation = app.accounts[1].launch_generation;
        let _ = app.update(Message::ShutdownAccount(key.clone()));
        let _ = app.update(Message::GameShutdownFinished(key, generation, Err("Game refused shutdown".into())));
        assert!(!app.accounts[1].closing.used && !app.accounts[1].closing.close_now && app.accounts[1].problem);
        assert!(matches!(row_view(&app, &app.accounts[1]).action, Some((_, Message::ShutdownAccount(_), _))));
    }
}

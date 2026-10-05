//! Explicit, target-bound confirmation for removing local channel subscriptions.

use super::AppController;

impl AppController {
    /// Removes only the channel named by the visible confirmation, never a new selection.
    pub(super) fn confirm_unsubscribe(&mut self, channel_id: &str) {
        if self.view.error_popup.is_some() {
            return;
        }
        let Some(popup) = self
            .view
            .unsubscribe_popup
            .as_ref()
            .filter(|popup| popup.channel_id == channel_id)
            .cloned()
        else {
            return;
        };
        if self.apply_local_subscription_change(popup.channel_id, popup.channel_name, None, false) {
            self.view.unsubscribe_popup = None;
        }
    }

    /// Cancels without changing OPML, cached videos, or automatic-download opt-ins.
    pub(super) fn dismiss_unsubscribe(&mut self) {
        if self.view.unsubscribe_popup.take().is_some() {
            self.view.status_line = "Unsubscribe cancelled".to_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;

    /// Creates two saved channels and one selected channel without any live provider.
    fn fixture() -> (tempfile::TempDir, AppController) {
        let temporary = crate::test_support::canonical_tempdir("unsubscribe confirmation");
        let config = Config::for_dir(temporary.path().join("youta"));
        let mut tree = SubscriptionTree::default();
        assert!(tree.subscribe_youtube_channel("First channel", "UCfirst"));
        assert!(tree.subscribe_youtube_channel("Second channel", "UCsecond"));
        assert!(tree.set_youtube_channel_auto_download("UCfirst", true));
        subscriptions::save(&config, &tree).unwrap();
        let mut controller =
            AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
        controller.view.details = Some(channel("UCfirst", "First channel"));
        (temporary, controller)
    }

    /// Matches the channel-only Details action offered by both frontends.
    fn channel(id: &str, name: &str) -> DetailView {
        DetailView {
            channel_id: id.to_owned(),
            channel_name: name.to_owned(),
            channel_subscribed: true,
            channel_auto_download: id == "UCfirst",
            ..DetailView::default()
        }
    }

    /// Opening, repeating, cancelling, or sending a stale confirmation cannot mutate OPML.
    #[test]
    fn unsubscribe_confirmation_cancel_preserves_subscription_and_download_opt_in() {
        let (_temporary, mut controller) = fixture();
        let original = std::fs::read(controller.config.subscriptions_file()).unwrap();
        controller.dispatch(UiAction::ToggleSubscription);
        let popup = controller.view.unsubscribe_popup.clone().unwrap();
        assert_eq!(popup.channel_id, "UCfirst");
        assert_eq!(popup.channel_name, "First channel");
        controller.dispatch(UiAction::ToggleSubscription);
        assert_eq!(controller.view.unsubscribe_popup.as_ref(), Some(&popup));
        assert_eq!(
            std::fs::read(controller.config.subscriptions_file()).unwrap(),
            original
        );
        controller.dispatch(UiAction::DismissUnsubscribe);
        assert!(controller.view.unsubscribe_popup.is_none());
        controller.dispatch(UiAction::ConfirmUnsubscribe {
            channel_id: "UCfirst".to_owned(),
        });
        assert_eq!(
            std::fs::read(controller.config.subscriptions_file()).unwrap(),
            original
        );
        assert!(
            controller
                .subscription_tree
                .youtube_channel_auto_download("UCfirst")
        );
        assert!(controller.view.details.as_ref().unwrap().channel_subscribed);
        assert!(
            controller
                .view
                .details
                .as_ref()
                .unwrap()
                .channel_auto_download
        );
    }

    /// An explicit confirmation remains bound to its rendered channel during background changes.
    #[test]
    fn unsubscribe_confirmation_uses_captured_channel_and_preserves_external_edits() {
        let (_temporary, mut controller) = fixture();
        controller.dispatch(UiAction::ToggleSubscription);
        controller.view.details = Some(channel("UCsecond", "Second channel"));
        let mut external = subscriptions::load(&controller.config).unwrap();
        assert!(external.subscribe_youtube_channel("External channel", "UCexternal"));
        subscriptions::save(&controller.config, &external).unwrap();
        controller.dispatch(UiAction::ConfirmUnsubscribe {
            channel_id: "UCfirst".to_owned(),
        });
        let saved = subscriptions::load(&controller.config).unwrap();
        assert!(!saved.contains_youtube_channel("UCfirst"));
        assert!(saved.contains_youtube_channel("UCsecond"));
        assert!(saved.contains_youtube_channel("UCexternal"));
        assert!(controller.view.details.as_ref().unwrap().channel_subscribed);
        assert!(controller.view.unsubscribe_popup.is_none());
        let saved_bytes = std::fs::read(controller.config.subscriptions_file()).unwrap();
        controller.dispatch(UiAction::ConfirmUnsubscribe {
            channel_id: "UCfirst".to_owned(),
        });
        assert_eq!(
            std::fs::read(controller.config.subscriptions_file()).unwrap(),
            saved_bytes
        );
    }

    /// A delayed click from a cancelled dialog must not confirm a different channel.
    #[test]
    fn unsubscribe_confirmation_rejects_a_different_channel_id() {
        let (_temporary, mut controller) = fixture();
        controller.dispatch(UiAction::ToggleSubscription);
        controller.dispatch(UiAction::DismissUnsubscribe);
        controller.view.details = Some(channel("UCsecond", "Second channel"));
        controller.dispatch(UiAction::ToggleSubscription);
        controller.dispatch(UiAction::ConfirmUnsubscribe {
            channel_id: "UCfirst".to_owned(),
        });
        assert_eq!(
            controller
                .view
                .unsubscribe_popup
                .as_ref()
                .unwrap()
                .channel_id,
            "UCsecond"
        );
        assert_eq!(
            subscriptions::load(&controller.config)
                .unwrap()
                .subscription_count(),
            2
        );
        controller.dispatch(UiAction::ConfirmUnsubscribe {
            channel_id: "UCsecond".to_owned(),
        });
        assert!(
            controller
                .subscription_tree
                .contains_youtube_channel("UCfirst")
        );
        assert!(
            !controller
                .subscription_tree
                .contains_youtube_channel("UCsecond")
        );
    }

    /// Malformed external OPML is never overwritten; the dialog remains available to retry.
    #[test]
    fn unsubscribe_confirmation_failure_preserves_memory_and_can_be_retried() {
        let (_temporary, mut controller) = fixture();
        let original = std::fs::read(controller.config.subscriptions_file()).unwrap();
        controller.dispatch(UiAction::ToggleSubscription);
        let malformed = b"<opml><body><outline";
        std::fs::write(controller.config.subscriptions_file(), malformed).unwrap();
        controller.dispatch(UiAction::ConfirmUnsubscribe {
            channel_id: "UCfirst".to_owned(),
        });
        assert_eq!(
            std::fs::read(controller.config.subscriptions_file()).unwrap(),
            malformed
        );
        assert!(
            controller
                .subscription_tree
                .contains_youtube_channel("UCfirst")
        );
        assert!(controller.view.details.as_ref().unwrap().channel_subscribed);
        assert!(controller.view.unsubscribe_popup.is_some());
        assert!(controller.view.error_popup.is_some());
        std::fs::write(controller.config.subscriptions_file(), original).unwrap();
        controller.dispatch(UiAction::ConfirmUnsubscribe {
            channel_id: "UCfirst".to_owned(),
        });
        assert!(
            subscriptions::load(&controller.config)
                .unwrap()
                .contains_youtube_channel("UCfirst")
        );
        controller.dispatch(UiAction::DismissErrorPopup);
        controller.dispatch(UiAction::ConfirmUnsubscribe {
            channel_id: "UCfirst".to_owned(),
        });
        assert!(
            !subscriptions::load(&controller.config)
                .unwrap()
                .contains_youtube_channel("UCfirst")
        );
    }

    /// A channel removed externally stays removed; confirmation never acts as a toggle.
    #[test]
    fn unsubscribe_confirmation_does_not_resubscribe_an_already_removed_channel() {
        let (_temporary, mut controller) = fixture();
        controller.dispatch(UiAction::ToggleSubscription);
        let mut external = subscriptions::load(&controller.config).unwrap();
        assert!(external.unsubscribe_youtube_channel("UCfirst"));
        subscriptions::save(&controller.config, &external).unwrap();
        let original = std::fs::read(controller.config.subscriptions_file()).unwrap();
        controller.dispatch(UiAction::ConfirmUnsubscribe {
            channel_id: "UCfirst".to_owned(),
        });
        assert_eq!(
            std::fs::read(controller.config.subscriptions_file()).unwrap(),
            original
        );
        assert!(!controller.view.details.as_ref().unwrap().channel_subscribed);
        assert!(
            !controller
                .view
                .details
                .as_ref()
                .unwrap()
                .channel_auto_download
        );
        assert!(controller.view.unsubscribe_popup.is_none());
    }

    /// Confirming a captured channel cannot replace an unrelated source's visible rows.
    #[test]
    fn unsubscribe_confirmation_keeps_background_navigation_intact() {
        let (_temporary, mut controller) = fixture();
        controller.dispatch(UiAction::ToggleSubscription);
        controller.view.screen = Screen::Local;
        controller.view.rows = vec![RowView {
            title: "Local track".to_owned(),
            ..RowView::default()
        }];
        controller.dispatch(UiAction::ConfirmUnsubscribe {
            channel_id: "UCfirst".to_owned(),
        });
        assert_eq!(controller.view.screen, Screen::Local);
        assert_eq!(controller.view.rows[0].title, "Local track");
    }
}

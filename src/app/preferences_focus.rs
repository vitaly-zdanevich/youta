//! Draft-only keyboard focus for the shared preferences popup.

use super::AppController;
use crate::view::{PreferencesField, UiAction};

impl AppController {
    /// Moves within the supported control list without altering preference values.
    pub(super) fn move_preferences_focus(&mut self, direction: i32) {
        let Some(popup) = self.view.preferences_popup.as_mut() else {
            return;
        };
        let fields = popup.available_fields();
        let current = fields
            .iter()
            .position(|field| *field == popup.selected_field)
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(direction as isize)
            .min(fields.len().saturating_sub(1));
        if let Some(field) = fields.get(next) {
            popup.selected_field = *field;
        }
    }

    /// Accepts only controls that can be edited in this popup and build.
    pub(super) fn select_preferences_field(&mut self, field: PreferencesField) {
        if let Some(popup) = self.view.preferences_popup.as_mut()
            && popup.available_fields().contains(&field)
        {
            popup.selected_field = field;
        }
    }

    /// Keeps keyboard focus on the control most recently clicked or activated by a letter.
    pub(super) fn synchronize_preferences_action_focus(&mut self, action: &UiAction) {
        if let Some(field) = PreferencesField::from_action(action) {
            self.select_preferences_field(field);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::keymap::{Key, KeyPress, key_action};
    use crate::persistence::StateStore;
    use crate::view::UiController;

    /// Opens an isolated draft without a player or provider fixture.
    fn controller() -> (tempfile::TempDir, AppController) {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::for_dir(directory.path().join("youta"));
        let mut controller =
            AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
        controller.open_preferences();
        controller
            .view
            .preferences_popup
            .as_mut()
            .unwrap()
            .environment_override = None;
        (directory, controller)
    }

    #[test]
    fn preferences_arrows_move_focus_and_space_changes_only_the_focused_draft() {
        let (_directory, mut controller) = controller();
        let before = controller.view.preferences_popup.clone().unwrap();
        let down = key_action(KeyPress::new(Key::Down), &controller.view, None, None).unwrap();
        assert_eq!(down, UiAction::MovePreferencesFocus(1));
        controller.dispatch(down);
        let mut expected = before.clone();
        expected.selected_field = PreferencesField::PlaybackHistory;
        assert_eq!(controller.view.preferences_popup.as_ref(), Some(&expected));
        let space =
            key_action(KeyPress::new(Key::Char(' ')), &controller.view, None, None).unwrap();
        assert_eq!(space, UiAction::TogglePlaybackHistorySaving);
        controller.dispatch(space);
        expected.save_playback_history = !before.save_playback_history;
        assert_eq!(controller.view.preferences_popup.as_ref(), Some(&expected));
        assert_eq!(
            controller.config.persistence.save_playback_history,
            before.save_playback_history
        );
        assert_eq!(
            key_action(KeyPress::new(Key::Enter), &controller.view, None, None),
            Some(UiAction::SubmitPreferences)
        );
        assert_eq!(
            key_action(KeyPress::new(Key::Left), &controller.view, None, None),
            None
        );
    }

    #[test]
    fn preferences_focus_skips_unsupported_controls_and_stays_on_mouse_or_letter_actions() {
        let (_directory, mut controller) = controller();
        let popup = controller.view.preferences_popup.as_mut().unwrap();
        popup.sponsorblock_supported = false;
        popup.nyan_cat_supported = false;
        popup.auto_download_supported = false;
        popup.archive_playback_supported = false;
        popup.video_summary_supported = false;
        popup.youtube_provider_settings_supported = false;
        let fields = popup.available_fields();
        assert!(!fields.contains(&PreferencesField::SponsorBlock));
        assert!(!fields.contains(&PreferencesField::HourlyDownloads));
        for field in fields.iter().skip(1) {
            controller.dispatch(UiAction::MovePreferencesFocus(1));
            assert_eq!(
                controller
                    .view
                    .preferences_popup
                    .as_ref()
                    .unwrap()
                    .selected_field,
                *field
            );
        }
        controller.dispatch(UiAction::MovePreferencesFocus(i32::MAX));
        assert_eq!(
            controller
                .view
                .preferences_popup
                .as_ref()
                .unwrap()
                .selected_field,
            *fields.last().unwrap()
        );
        controller.dispatch(UiAction::MovePreferencesFocus(i32::MIN));
        assert_eq!(
            controller
                .view
                .preferences_popup
                .as_ref()
                .unwrap()
                .selected_field,
            PreferencesField::SubscriptionsLayout
        );
        controller.dispatch(UiAction::ToggleLocalFolderSizes);
        assert_eq!(
            controller
                .view
                .preferences_popup
                .as_ref()
                .unwrap()
                .selected_field,
            PreferencesField::LocalFolderSizes
        );
        controller.dispatch(UiAction::SelectPreferencesField(
            PreferencesField::SponsorBlock,
        ));
        assert_eq!(
            controller
                .view
                .preferences_popup
                .as_ref()
                .unwrap()
                .selected_field,
            PreferencesField::LocalFolderSizes
        );
        assert_eq!(
            key_action(KeyPress::new(Key::Char('a')), &controller.view, None, None),
            Some(UiAction::ToggleSkipAdvertisementChapters)
        );
    }
}

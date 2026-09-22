//! Linux-only colors for Youta's existing native menu widgets.
//!
//! Styling is local to the menu tree: it must not change GTK settings, other
//! windows, web content, native actions, or the user's desktop theme.

use gtk::prelude::*;

/// A private class applied only to the native menu tree, never the whole screen.
const MENU_CLASS: &str = "youta-native-menu";

/// Explicit backgrounds prevent light theme gradients from covering the palette.
///
/// Inner boxes and labels remain transparent so the menu item's selection stays
/// visible behind its text. User-priority accessibility CSS can still override
/// this application-priority provider.
const MENU_CSS: &[u8] = br"
.youta-native-menu {
    color: #ffffff;
    background-color: transparent;
    background-image: none;
    text-shadow: none;
}
menubar.youta-native-menu,
menu.youta-native-menu,
menuitem.youta-native-menu {
    background-color: #000000;
    box-shadow: none;
}
menuitem.youta-native-menu:hover,
menuitem.youta-native-menu:selected,
menuitem.youta-native-menu:focus,
menuitem.youta-native-menu:active {
    color: #ffffff;
    background-color: #303030;
}
.youta-native-menu:disabled {
    color: #8c8c8c;
}
";

/// Applies the app's dark palette to one native menu bar and its attached menus.
///
/// Must be called on GTK's main thread after the native menu has been installed.
///
/// # Errors
///
/// Returns a GTK parsing error if the embedded stylesheet cannot be loaded.
pub(crate) fn apply(menu_bar: &gtk::MenuBar) -> Result<(), gtk::glib::Error> {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(MENU_CSS)?;
    style_widget(menu_bar.upcast_ref(), &provider);
    Ok(())
}

/// Installs the provider per widget; GTK context providers do not reach children.
///
/// Submenus are attached to menu items but are not ordinary container children,
/// so visit them explicitly. Keeping the existing widgets preserves native
/// accelerators, selection behavior, action signals, and the tray's own styling.
fn style_widget(widget: &gtk::Widget, provider: &gtk::CssProvider) {
    let context = widget.style_context();
    context.add_class(MENU_CLASS);
    context.add_provider(provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    if let Some(container) = widget.downcast_ref::<gtk::Container>() {
        for child in container.children() {
            style_widget(&child, provider);
        }
    }
    if let Some(item) = widget.downcast_ref::<gtk::MenuItem>()
        && let Some(submenu) = item.submenu()
    {
        style_widget(&submenu, provider);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gtk::prelude::*;
    use gtk::{CssProvider, Menu, MenuBar, MenuItem, StateFlags, Widget};

    /// Reads resolved CSS colors instead of merely checking provider text.
    fn color(widget: &impl IsA<Widget>, property: &str, state: StateFlags) -> gtk::gdk::RGBA {
        widget
            .style_context()
            .style_property_for_state(property, state)
            .get()
            .expect("resolved GTK color property")
    }

    /// RGB channels must match without relying on exact floating-point serialization.
    fn assert_gray(color: &gtk::gdk::RGBA, channel: f64) {
        for actual in [color.red(), color.green(), color.blue()] {
            assert!(
                (actual - channel).abs() < 0.01,
                "expected gray {channel}, got {color:?}"
            );
        }
        assert!(
            (color.alpha() - 1.0).abs() < 0.01,
            "color must be opaque: {color:?}"
        );
    }

    /// One GTK-initializing test keeps the native objects on one harness thread.
    #[test]
    #[ignore = "requires a GTK display; run under xvfb-run"]
    fn native_menu_colors_are_scoped_readable_and_preserve_activation() {
        gtk::init().expect("GTK display; run this test under xvfb-run --test-threads=1");
        let menu_bar = MenuBar::new();
        let file = MenuItem::with_label("File");
        let submenu = Menu::new();
        let play = MenuItem::new();
        // Muda's native items can contain a box around their accelerator label.
        let play_contents = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let play_label = gtk::AccelLabel::new("Play");
        play_contents.pack_start(&play_label, true, true, 0);
        play.add(&play_contents);
        let disabled = MenuItem::with_label("Unavailable");
        disabled.set_sensitive(false);
        submenu.append(&play);
        submenu.append(&disabled);
        file.set_submenu(Some(&submenu));
        menu_bar.append(&file);
        let unrelated = gtk::Label::new(Some("Unrelated content"));
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.pack_start(&menu_bar, false, false, 0);
        container.pack_start(&unrelated, false, false, 0);

        // Supply a deterministic light starting theme on these fixture widgets
        // only. No process-wide screen provider or desktop setting is touched.
        let baseline = CssProvider::new();
        baseline.load_from_data(b"* { color: #000000; background-color: #ffffff; background-image: none; transition: none; }")
			.expect("fixture CSS");
        for widget in [
            menu_bar.clone().upcast::<Widget>(),
            file.clone().upcast(),
            submenu.clone().upcast(),
            play.clone().upcast(),
            disabled.clone().upcast(),
            unrelated.clone().upcast(),
            file.child().expect("File label"),
            play.child().expect("Play label"),
            play_label.clone().upcast(),
            disabled.child().expect("disabled label"),
        ] {
            widget
                .style_context()
                .add_provider(&baseline, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION - 1);
        }
        let unrelated_foreground = color(&unrelated, "color", StateFlags::NORMAL);
        let unrelated_background = color(&unrelated, "background-color", StateFlags::NORMAL);
        assert_gray(
            &color(&menu_bar, "background-color", StateFlags::NORMAL),
            1.0,
        );
        let activations = Rc::new(Cell::new(0));
        let observed = activations.clone();
        play.connect_activate(move |_| observed.set(observed.get() + 1));

        super::apply(&menu_bar).expect("menu CSS");
        for widget in [
            menu_bar.upcast_ref::<Widget>(),
            submenu.upcast_ref(),
            play.upcast_ref(),
        ] {
            assert_gray(&color(widget, "background-color", StateFlags::NORMAL), 0.0);
        }
        for widget in [
            file.child().unwrap(),
            play.child().unwrap(),
            play_label.upcast(),
        ] {
            assert_gray(&color(&widget, "color", StateFlags::NORMAL), 1.0);
            assert!(
                color(&widget, "background-color", StateFlags::NORMAL).alpha() < 0.01,
                "label backgrounds must not conceal menu-item selection"
            );
        }
        for state in [
            StateFlags::PRELIGHT,
            StateFlags::SELECTED,
            StateFlags::FOCUSED,
        ] {
            assert_gray(&color(&play, "background-color", state), 48.0 / 255.0);
            assert_gray(&color(&play, "color", state), 1.0);
        }
        assert_gray(
            &color(&disabled, "color", StateFlags::INSENSITIVE),
            140.0 / 255.0,
        );
        assert_gray(
            &color(&disabled.child().unwrap(), "color", StateFlags::INSENSITIVE),
            140.0 / 255.0,
        );
        assert_eq!(
            color(&unrelated, "color", StateFlags::NORMAL),
            unrelated_foreground
        );
        assert_eq!(
            color(&unrelated, "background-color", StateFlags::NORMAL),
            unrelated_background
        );
        play.activate();
        assert_eq!(activations.get(), 1, "native activation remains connected");
        assert_eq!(
            file.submenu(),
            Some(submenu.upcast()),
            "the original detached submenu remains attached"
        );
    }
}

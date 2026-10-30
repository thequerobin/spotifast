//! Soloist is the first player offered after sign-in, with a built-in fallback.
use crate::{app::App, i18n::gettext, model::Action, soloist::Status, theme};

/// A native playback icon, not a Spotify or Soloist brand mark.
fn badge(ui: &mut egui::Ui, palette: &theme::Palette, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), size / 2.0, palette.surface_active);
    theme::paint_icon(ui, theme::Icon::Headphones, rect, size * 0.52, palette.text);
}

pub fn onboarding(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let locale = app.locale;
    let welcome = app.soloist_onboarding == crate::settings::SoloistOnboarding::Welcome;
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(palette.window))
        .show(ui, |ui| {
            let rect = ui.max_rect();
            fastframe_titlebar::drag(ui, egui::Rect::from_min_size(rect.min,
                egui::vec2(rect.width(), theme::TOP_BAR_HEIGHT + theme::titlebar_inset(ui.ctx()))));
            super::widgets::paint_vertical_gradient(ui, rect,
                super::blend(palette.window, palette.accent, 0.08), palette.window);
            egui::ScrollArea::vertical().id_salt("soloist-onboarding").show(ui, |ui| {
                ui.add_space(((rect.height() - if welcome { 500.0 } else { 650.0 }) / 2.0).max(48.0));
                ui.vertical_centered(|ui| {
                    ui.set_max_width(540.0_f32.min(rect.width() - 48.0));
                    egui::Frame::new()
                        .fill(palette.panel)
                        .stroke(egui::Stroke::new(1.0, palette.outline))
                        .corner_radius(20)
                        .inner_margin(28)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                badge(ui, &palette, 56.0);
                                ui.add_space(16.0);
                                theme::text(ui, gettext(locale, "Listen with Spotify Soloist"), theme::semibold(25.0), palette.text);
                                ui.add_space(8.0);
                                ui.label(gettext(locale, "Spotify's official player for listening outside the Spotify app."));
                                ui.add_space(16.0);
                                if welcome {
                                    ui.label(gettext(locale, "Requires Spotify Premium. Spotifast handles downloads and updates."));
                                    ui.add_space(16.0);
                                    if theme::pill_button(ui, &palette, &gettext(locale, "Set up Soloist"), true).clicked() {
                                        app.actions.push(Action::StartSoloistSetup);
                                    }
                                } else {
                                    controls(app, ui, true);
                                    if app.settings.soloist_enabled && app.soloist_device_id.is_some()
                                        && matches!(app.soloist_status, Status::Ready { .. })
                                        && theme::pill_button(ui, &palette, &gettext(locale, "Start listening"), true).clicked()
                                    {
                                        app.actions.push(Action::FinishSoloistSetup);
                                    }
                                }
                                ui.add_space(18.0);
                                ui.separator();
                                ui.add_space(10.0);
                                if theme::soft_button(ui, &palette, None, &gettext(locale, "Use the built-in player"), false).clicked() {
                                    app.actions.push(Action::UseBuiltInPlayer);
                                }
                            });
                        });
                });
                ui.add_space(32.0);
            });
        });
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let locale = app.locale;
    let palette = app.palette;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 14.0;
        badge(ui, &palette, 48.0);
        ui.vertical(|ui| {
            if matches!(app.soloist_status, Status::Ready { .. }) && app.soloist_device_id.is_some()
            {
                ui.label(egui::RichText::new(gettext(locale, "Enabled on this computer")).strong());
                theme::subtle(ui, &palette, &gettext(locale, "Updates automatically."));
            } else {
                ui.label(gettext(
                    locale,
                    "Spotify's player, with lossless support. Requires Premium.",
                ));
            }
        });
        if app.settings.soloist_enabled {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::pill_button(ui, &palette, &gettext(locale, "Turn off Soloist"), false)
                    .clicked()
                {
                    app.actions.push(Action::SetSoloist(false));
                }
            });
        }
    });
    ui.add_space(10.0);
    controls(app, ui, false);
}

fn controls(app: &mut App, ui: &mut egui::Ui, onboarding: bool) {
    let locale = app.locale;
    let palette = app.palette;
    if !crate::soloist::install::supported() {
        ui.label(gettext(locale, "Spotify has not published Soloist for this platform. The built-in player remains available."));
        return;
    }
    if !app.is_connected() {
        ui.label(gettext(
            locale,
            "Sign in to Spotify before setting up Soloist.",
        ));
        return;
    }
    match &app.soloist_status {
        Status::Off | Status::NeedsKey | Status::Failed(_) => {
            if let Status::Failed(error) = &app.soloist_status {
                ui.label(error);
                ui.add_space(8.0);
            }
            if theme::soft_button(
                ui,
                &palette,
                None,
                &gettext(locale, "1. Get your Soloist key from Spotify"),
                false,
            )
            .clicked()
            {
                app.actions.push(Action::OpenUrl(
                    "https://developer.spotify.com/dashboard/soloist".into(),
                ));
            }
            ui.add_space(8.0);
            ui.add(
                egui::TextEdit::singleline(&mut app.soloist_key)
                    .password(true)
                    .desired_width(ui.available_width())
                    .hint_text(gettext(locale, "Paste your personal Soloist key")),
            );
            ui.add_space(8.0);
            let retry = app.settings.soloist_enabled
                && app.soloist_key.is_empty()
                && !matches!(app.soloist_status, Status::NeedsKey);
            let label = if retry {
                gettext(locale, "Retry with saved key")
            } else {
                gettext(locale, "2. Download and enable Soloist")
            };
            ui.add_enabled_ui(retry || !app.soloist_key.trim().is_empty(), |ui| {
                if theme::pill_button(ui, &palette, &label, true).clicked() {
                    app.actions.push(if retry {
                        Action::RetrySoloist
                    } else {
                        Action::SetSoloist(true)
                    });
                }
            });
            if !onboarding
                && matches!(app.soloist_status, Status::Off)
                && app.soloist_key.trim().is_empty()
                && theme::soft_button(
                    ui,
                    &palette,
                    None,
                    &gettext(locale, "Enable with saved key"),
                    false,
                )
                .clicked()
            {
                app.actions.push(Action::RetrySoloist);
            }
        }
        Status::Starting => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(gettext(locale, "Starting Soloist…"));
            });
        }
        Status::Downloading { received, total } => {
            ui.label(gettext(locale, "Downloading Soloist from Spotify…"));
            let progress = if *total == 0 {
                0.0
            } else {
                *received as f32 / *total as f32
            };
            ui.add(
                egui::ProgressBar::new(progress)
                    .show_percentage()
                    .animate(true),
            );
        }
        Status::Pairing { name, .. } => {
            ui.label(egui::RichText::new(gettext(locale, "3. Pair once in Spotify")).strong());
            ui.label(gettext(
                locale,
                "Open Spotify with the same account and network. In its device picker, choose:",
            ));
            ui.label(egui::RichText::new(name).strong());
        }
        Status::Ready { .. } => {
            if app.soloist_device_id.is_some() {
                if onboarding {
                    ui.label(
                        egui::RichText::new(gettext(locale, "Enabled on this computer")).strong(),
                    );
                    theme::subtle(ui, &palette, &gettext(locale, "Updates automatically."));
                }
            } else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(gettext(locale, "Verifying Soloist…"));
                });
                ui.label(gettext(
                    locale,
                    "Open Spotify with the same account and network. In its device picker, choose:",
                ));
                if let Status::Ready { name, .. } = &app.soloist_status {
                    ui.label(name);
                }
            }
            if app.soloist_update.is_some() {
                theme::subtle(
                    ui,
                    &palette,
                    &gettext(
                        locale,
                        "Update ready. It will install when Soloist is inactive.",
                    ),
                );
            }
        }
    }
    ui.add_space(8.0);
    if let Status::Ready {
        version, expires, ..
    } = &app.soloist_status
    {
        let date = jiff::Timestamp::from_second((*expires).min(i64::MAX as u64) as i64)
            .ok()
            .map(|at| at.to_zoned(jiff::tz::TimeZone::system()).date().to_string())
            .unwrap_or_default();
        theme::subtle(
            ui,
            &palette,
            &gettext(locale, "Version {version} · expires {date}")
                .replace("{version}", version)
                .replace("{date}", &date),
        );
    }
    if matches!(app.soloist_status, Status::Ready { .. }) {
        theme::subtle(
            ui,
            &palette,
            &gettext(locale, "Spotify manages audio quality."),
        );
    }
    if ui
        .link(gettext(locale, "Setup, privacy and limitations"))
        .clicked()
    {
        app.actions
            .push(Action::OpenUrl("https://spotifast.rocks/soloist/".into()));
    }
}

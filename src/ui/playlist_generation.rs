//! Native form and history. Views only emit actions; the runtime owns jobs.
use crate::app::App;
use crate::i18n::gettext;
use crate::model::Action;
use crate::playlist_generation::jobs::{Request, Stage, State};
use crate::playlist_generation::{KINDS, Parameters, Period, Source, default_name, kind};

#[derive(Default)]
pub struct Draft {
    pub kind: String,
    pub period: Period,
    pub parameters: Parameters,
    pub name: String,
    pub public: bool,
    suggested: String,
}

/// Returns true when the existing manual creation form should be drawn.
pub fn creation(app: &mut App, ui: &mut egui::Ui) -> bool {
    let locale = app.locale;
    let account = app.user_id().map(str::to_owned);
    let draft = &mut app.generation_draft;
    egui::ComboBox::from_id_salt("generation-kind")
        .width(ui.available_width())
        .wrap()
        .selected_text(KINDS.iter().find(|k| k.id == draft.kind).map_or_else(
            || gettext(locale, "Manual playlist").into_owned(),
            |k| gettext(locale, k.label).into_owned(),
        ))
        .show_ui(ui, |ui| {
            ui.selectable_value(
                &mut draft.kind,
                String::new(),
                gettext(locale, "Manual playlist"),
            );
            for kind in KINDS {
                ui.selectable_value(
                    &mut draft.kind,
                    kind.id.to_owned(),
                    gettext(locale, kind.label),
                );
            }
        });
    ui.add_space(10.0);
    if draft.kind.is_empty() {
        return true;
    }
    if kind(&draft.kind).is_ok_and(|kind| kind.source == Source::FollowedReleases) {
        ui.label(gettext(locale, "Uses album/single release dates and excludes liked songs. Only tracks by followed artists are included."));
    }
    let release_dates = kind(&draft.kind).is_ok_and(|kind| kind.source == Source::FollowedReleases);
    ui.label(if release_dates {
        gettext(
            locale,
            "Period: YYYY, MM/YYYY or DD/MM/YYYY. Leave blank for all releases.",
        )
    } else {
        gettext(
            locale,
            "Period (UTC): YYYY, MM/YYYY or DD/MM/YYYY. Leave blank for all songs.",
        )
    });
    ui.horizontal(|ui| {
        ui.label(gettext(locale, "From"));
        ui.add(egui::TextEdit::singleline(&mut draft.period.start).desired_width(100.0));
        ui.label(gettext(locale, "To"));
        ui.add(egui::TextEdit::singleline(&mut draft.period.end).desired_width(100.0));
    });
    let validation = kind(&draft.kind).and_then(|strategy| {
        for field in strategy.fields {
            ui.label(gettext(locale, field.label));
            let value = draft.parameters.entry(field.key.to_owned()).or_default();
            ui.add(egui::TextEdit::singleline(value).hint_text(gettext(locale, field.hint)));
        }
        (strategy.validate)(&draft.parameters)?;
        default_name(&draft.kind, &draft.period)
    });
    if let Ok(suggestion) = &validation {
        if draft.suggested.is_empty() || draft.name == draft.suggested {
            draft.name.clone_from(suggestion);
        }
        draft.suggested.clone_from(suggestion);
    }
    ui.label(gettext(locale, "Name"));
    ui.text_edit_singleline(&mut draft.name);
    ui.checkbox(&mut draft.public, gettext(locale, "Public playlist"));
    if let Err(error) = &validation {
        ui.colored_label(app.palette.danger, error);
    }
    let valid = validation.is_ok()
        && !draft.name.trim().is_empty()
        && draft.name.chars().count() <= 100
        && account.is_some();
    if ui
        .add_enabled(
            valid,
            egui::Button::new(gettext(locale, "Start generation")),
        )
        .clicked()
    {
        app.actions.push(Action::Generation(Request::Start {
            account: account.unwrap_or_default(),
            kind: draft.kind.clone(),
            period: draft.period.clone(),
            parameters: draft.parameters.clone(),
            name: draft.name.trim().into(),
            public: draft.public,
        }));
    }
    ui.label(gettext(
        locale,
        "Progress is saved after every batch. Quitting pauses processing until the next launch.",
    ));
    if ui.button(gettext(locale, "Close")).clicked() {
        app.actions.push(Action::CloseDialog);
    }
    false
}

fn clock(seconds: u64) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

pub fn history(app: &mut App, ui: &mut egui::Ui) {
    let snapshot = app.backend.generation.snapshot();
    let locale = app.locale;
    if let Some(error) = snapshot.error {
        ui.colored_label(app.palette.danger, error);
    }
    let account = app.user_id().unwrap_or_default().to_owned();
    let jobs: Vec<_> = snapshot
        .jobs
        .iter()
        .filter(|job| job.account == account)
        .collect();
    if jobs.is_empty() {
        return;
    }
    ui.separator();
    ui.label(gettext(locale, "Playlist generations"));
    for job in jobs.into_iter().rev() {
        ui.push_id(&job.id, |ui| {
                ui.label(egui::RichText::new(&job.name).strong());
                let stage = match job.stage {
                    Stage::FollowedArtists => gettext(locale, "Reading followed artists"),
                    Stage::ArtistReleases => gettext(locale, "Reading artist releases"),
                    Stage::ReleaseTracks => gettext(locale, "Checking release tracks and likes"),
                    Stage::Albums => gettext(locale, "Reading saved albums"),
                    Stage::Tracks => gettext(locale, "Reading liked songs"),
                    Stage::Create => gettext(locale, "Creating playlist"),
                    Stage::Add => gettext(locale, "Adding songs"),
                    Stage::Done => gettext(locale, "Finished"),
                };
                let state = match job.state {
                    State::Pending => gettext(locale, "Pending"), State::Running => gettext(locale, "Running"),
                    State::Paused => gettext(locale, "Paused"), State::Retrying => gettext(locale, "Retrying"),
                    State::Completed => gettext(locale, "Completed"), State::Failed => gettext(locale, "Failed"),
                    State::Cancelled => gettext(locale, "Cancelled"),
                };
                ui.label(format!("{state}: {stage}"));
                if let Some(total) = job.artists_total.filter(|total| *total > 0) {
                    ui.label(gettext(locale, "Artists: {artists}/{total}")
                        .replace("{artists}", &job.artists_checked.to_string()).replace("{total}", &total.to_string()));
                }
                if let Some(progress) = job.progress {
                    ui.add(egui::ProgressBar::new(progress).show_percentage());
                } else if matches!(job.state, State::Pending | State::Running | State::Retrying) {
                    ui.horizontal(|ui| { ui.spinner(); ui.label(gettext(locale, "Discovering total work…")); });
                }
                ui.label(gettext(locale, "Analyzed: {analyzed}/{total} · Albums: {albums} · Selected: {selected} · Added: {added}")
                    .replace("{analyzed}", &job.analyzed.to_string())
                    .replace("{total}", &job.tracks_total.map_or_else(|| "?".into(), |n| n.to_string()))
                    .replace("{albums}", &job.albums_checked.to_string())
                    .replace("{selected}", &job.selected.to_string()).replace("{added}", &job.added.to_string()));
                ui.label(gettext(locale, "Elapsed: {elapsed} · Estimated remaining: {remaining}")
                    .replace("{elapsed}", &clock(job.elapsed))
                    .replace("{remaining}", &job.eta.map_or_else(|| "?".into(), clock)));
                if let Some(error) = &job.error { ui.label(error); }
                ui.horizontal_wrapped(|ui| {
                    for (label, target, visible) in [
                        (gettext(locale, "Pause"), State::Paused, matches!(job.state, State::Pending | State::Running | State::Retrying)),
                        (gettext(locale, "Resume"), State::Pending, matches!(job.state, State::Paused | State::Failed)),
                        (gettext(locale, "Cancel"), State::Cancelled, !matches!(job.state, State::Completed | State::Cancelled)),
                    ] {
                        if visible && ui.button(label).clicked() {
                            app.actions.push(Action::Generation(Request::Control { id: job.id.clone(), state: target }));
                        }
                    }
                    if let Some(id) = &job.playlist_id {
                        ui.hyperlink_to(gettext(locale, "Open in Spotify"), format!("https://open.spotify.com/playlist/{id}"));
                    }
                });
                ui.separator();
            });
    }
}

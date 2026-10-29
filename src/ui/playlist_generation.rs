//! Native form and history. Views only emit actions; the runtime owns jobs.
use crate::app::App;
use crate::i18n::{Locale, gettext};
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
        gettext(locale, "Release period")
    } else {
        gettext(locale, "Liked songs period (UTC)")
    });
    ui.small(gettext(
        locale,
        "Select a year, then optionally a month and day. Leave both years as Any for all dates. A start alone selects that whole period.",
    ));
    ui.add_space(8.0);
    date_selector(
        ui,
        locale,
        "generation-start",
        gettext(locale, "From").as_ref(),
        &mut draft.period.start,
    );
    ui.add_space(12.0);
    date_selector(
        ui,
        locale,
        "generation-end",
        gettext(locale, "To").as_ref(),
        &mut draft.period.end,
    );
    ui.add_space(16.0);
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
    ui.add_space(8.0);
    ui.checkbox(&mut draft.public, gettext(locale, "Public playlist"));
    if let Err(error) = &validation {
        ui.colored_label(app.palette.danger, error);
    }
    let valid = validation.is_ok()
        && !draft.name.trim().is_empty()
        && draft.name.chars().count() <= 100
        && account.is_some();
    ui.add_space(12.0);
    ui.small(if release_dates {
        gettext(locale, "Estimated API cost: high and variable (artists, releases and track pages). Cached discoveries reduce repeated work.")
    } else {
        gettext(locale, "Estimated API cost: medium; about one request per 50 library entries, plus playlist batches. Exact size is not known yet.")
    });
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

/// UI order is year/month/day; persisted periods retain their existing format.
#[derive(Debug, Default, PartialEq, Eq)]
struct DateSelection {
    year: Option<u16>,
    month: Option<u16>,
    day: Option<u16>,
}

impl DateSelection {
    fn from_period(value: &str) -> Self {
        let parts: Vec<_> = value.split('/').collect();
        match parts.as_slice() {
            [year] => Self {
                year: year.parse().ok(),
                ..Self::default()
            },
            [month, year] => Self {
                year: year.parse().ok(),
                month: month.parse().ok(),
                day: None,
            },
            [day, month, year] => Self {
                year: year.parse().ok(),
                month: month.parse().ok(),
                day: day.parse().ok(),
            },
            _ => Self::default(),
        }
    }

    fn days_in_month(&self) -> u16 {
        self.year
            .zip(self.month)
            .and_then(|(year, month)| jiff::civil::Date::new(year as i16, month as i8, 1).ok())
            .map_or(31, |date| date.days_in_month() as u16)
    }

    fn normalize(&mut self) {
        if self.year.is_none() {
            self.month = None;
        }
        if self.month.is_none() {
            self.day = None;
        }
        if let Some(day) = self.day {
            self.day = Some(day.min(self.days_in_month()));
        }
    }

    fn period_value(&self) -> String {
        match (self.year, self.month, self.day) {
            (Some(year), Some(month), Some(day)) => format!("{day:02}/{month:02}/{year:04}"),
            (Some(year), Some(month), None) => format!("{month:02}/{year:04}"),
            (Some(year), _, _) => format!("{year:04}"),
            _ => String::new(),
        }
    }
}

fn date_selector(ui: &mut egui::Ui, locale: Locale, id: &str, label: &str, value: &mut String) {
    ui.push_id(id, |ui| {
        ui.label(egui::RichText::new(label).strong());
        let mut date = DateSelection::from_period(value);
        let last_year = (jiff::Zoned::now().year() as u16 + 1).max(date.year.unwrap_or(0));
        ui.columns(3, |columns| {
            date_component(
                &mut columns[0],
                locale,
                "year",
                gettext(locale, "Year").as_ref(),
                &mut date.year,
                last_year,
            );
            date.normalize();
            columns[1].add_enabled_ui(date.year.is_some(), |ui| {
                date_component(
                    ui,
                    locale,
                    "month",
                    gettext(locale, "Month").as_ref(),
                    &mut date.month,
                    12,
                );
            });
            date.normalize();
            let days = date.days_in_month();
            columns[2].add_enabled_ui(date.month.is_some(), |ui| {
                date_component(
                    ui,
                    locale,
                    "day",
                    gettext(locale, "Day").as_ref(),
                    &mut date.day,
                    days,
                );
            });
        });
        date.normalize();
        *value = date.period_value();
    });
}

fn date_component(
    ui: &mut egui::Ui,
    locale: Locale,
    id: &str,
    label: &str,
    value: &mut Option<u16>,
    max: u16,
) {
    ui.label(label);
    let year = id == "year";
    let mut index = value.map_or(0, |value| {
        usize::from(if year { max + 1 - value } else { value })
    });
    let response = egui::ComboBox::from_id_salt(id)
        .width(ui.available_width())
        .show_index(ui, &mut index, usize::from(max) + 1, |index| {
            if index == 0 {
                gettext(locale, "Any").into_owned()
            } else if year {
                format!("{:04}", usize::from(max) + 1 - index)
            } else {
                format!("{index:02}")
            }
        });
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, ui.is_enabled(), label)
    });
    *value = (index > 0).then(|| {
        if year {
            max + 1 - index as u16
        } else {
            index as u16
        }
    });
}

fn clock(seconds: u64) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

pub fn history(app: &mut App, ui: &mut egui::Ui) {
    diagnostics(app, ui);
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
                    Stage::ArtistReleases => gettext(locale, "Finding releases for the period"),
                    Stage::ReleaseTracks => gettext(locale, "Checking release tracks and likes"),
                    Stage::Albums => gettext(locale, "Reading saved albums"),
                    Stage::Tracks => gettext(locale, "Reading liked songs"),
                    Stage::Create => gettext(locale, "Creating playlist"),
                    Stage::Add => gettext(locale, "Adding songs"),
                    Stage::Done => gettext(locale, "Finished"),
                };
                let state = match job.state {
                    State::Pending => gettext(locale, "Pending"), State::Running => gettext(locale, "Running"),
                    State::Paused => gettext(locale, "Paused"), State::PausedQuota => gettext(locale, "Paused: Spotify quota"), State::Retrying => gettext(locale, "Retrying"),
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
                if job.release_source {
                    ui.label(gettext(locale, "Unique releases checked: {checked} · Within period: {matched}")
                        .replace("{checked}", &job.albums_checked.to_string())
                        .replace("{matched}", &job.matched_releases.to_string()));
                    ui.label(gettext(locale, "Songs analyzed: {analyzed}/{total} · Selected: {selected} · Added: {added}")
                        .replace("{analyzed}", &job.analyzed.to_string())
                        .replace("{total}", &job.tracks_total.map_or_else(|| "?".into(), |n| n.to_string()))
                        .replace("{selected}", &job.selected.to_string()).replace("{added}", &job.added.to_string()));
                } else {
                ui.label(gettext(locale, "Analyzed: {analyzed}/{total} · Albums: {albums} · Selected: {selected} · Added: {added}")
                    .replace("{analyzed}", &job.analyzed.to_string())
                    .replace("{total}", &job.tracks_total.map_or_else(|| "?".into(), |n| n.to_string()))
                    .replace("{albums}", &job.albums_checked.to_string())
                    .replace("{selected}", &job.selected.to_string()).replace("{added}", &job.added.to_string()));
                }
                ui.label(gettext(locale, "Elapsed: {elapsed} · Estimated remaining: {remaining}")
                    .replace("{elapsed}", &clock(job.elapsed))
                    .replace("{remaining}", &job.eta.map_or_else(|| "?".into(), clock)));
                if let Some(error) = &job.error { ui.label(error); }
                ui.horizontal_wrapped(|ui| {
                    for (label, target, visible) in [
                        (gettext(locale, "Pause"), State::Paused, matches!(job.state, State::Pending | State::Running | State::Retrying)),
                        (gettext(locale, "Resume"), State::Pending, matches!(job.state, State::Paused | State::PausedQuota | State::Failed)),
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

/// A local diagnostic view, no requests or disk access on the UI thread.
fn diagnostics(app: &mut App, ui: &mut egui::Ui) {
    let snapshot = app.backend.activity().cost.snapshot();
    let locale = app.locale;
    ui.collapsing(gettext(locale, "API cost manager"), |ui| {
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        ui.label(gettext(locale, "Official remaining quota: not provided by Spotify"));
        ui.label(format!("{}: {} / {}", gettext(locale, "Requests in the last 30 seconds / local budget"), snapshot.recent_requests, snapshot.internal_budget));
        ui.label(format!("{}: {:.2}/s", gettext(locale, "Effective dispatch rate (last 30 seconds)"), snapshot.recent_requests as f32 / 30.0));
        ui.label(format!("{}: {} / {} / {}", gettext(locale, "Queued (high / medium / low)"), snapshot.queued[0], snapshot.queued[1], snapshot.queued[2]));
        ui.label(format!("{}: {}", gettext(locale, "In flight"), snapshot.in_flight));
        ui.label(format!("{}: {}s", gettext(locale, "Global cooldown remaining"), snapshot.cooldown_seconds));
        let reads = snapshot.cache_hits + snapshot.cache_misses;
        ui.label(format!("{}: {} / {}", gettext(locale, "Catalog cache hits / reads"), snapshot.cache_hits, reads));
        quota_notice(app, ui, &snapshot);
        egui::ScrollArea::vertical().id_salt("api-cost-endpoints").max_height(180.0).show(ui, |ui| {
        for (endpoint, metrics) in &snapshot.endpoints {
            ui.label(format!("{endpoint}: {} calls; 429 rate/quota {}/{}; 401/403/5xx {}/{}/{}; network {}; mean {}ms",
                metrics.requests, metrics.rate_limits, metrics.quota_limits, metrics.unauthorized, metrics.forbidden,
                metrics.server_errors, metrics.network_errors, metrics.latency_ms / metrics.requests.max(1)));
        }
        });
        ui.small(gettext(locale, "Limits and cost are local estimates. Other applications and devices can consume Spotify quota too."));
    });
}

/// Also available before sign-in, so a persisted circuit cannot lock the user
/// out of its explicit recovery control after restarting.
pub(crate) fn quota_notice(
    app: &mut App,
    ui: &mut egui::Ui,
    snapshot: &crate::api::cost::Snapshot,
) {
    let locale = app.locale;
    if let Some(error) = &snapshot.storage_error {
        ui.label(error);
    }
    if snapshot.quota_blocked {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));
        ui.label(gettext(locale, "Spotify quota exhausted. Jobs remain checkpointed. No reset time is known. Check the developer dashboard before trying again."));
        ui.hyperlink_to(
            gettext(locale, "Spotify developer dashboard"),
            crate::ui::login::DASHBOARD_URL,
        );
        if ui.button(gettext(locale, "Allow quota recheck")).clicked() {
            app.actions.push(Action::Generation(Request::RecheckQuota));
        }
        ui.small(gettext(
            locale,
            "Then resume paused jobs. A new quota rejection will pause requests again.",
        ));
    }
}

#[cfg(test)]
mod date_selector_tests {
    use super::DateSelection;
    use crate::playlist_generation::Period;

    #[test]
    fn date_rows_keep_columns_aligned_at_narrow_and_normal_widths() {
        for width in [300.0, 420.0] {
            for dark in [false, true] {
                let ctx = egui::Context::default();
                ctx.set_visuals(if dark {
                    egui::Visuals::dark()
                } else {
                    egui::Visuals::light()
                });
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 400.0),
                    )),
                    ..Default::default()
                };
                let mut output = ctx.run_ui(input, |ui| {
                    let right = ui.max_rect().right();
                    let mut start = "29/02/2024".to_owned();
                    let mut end = String::new();
                    super::date_selector(
                        ui,
                        crate::i18n::Locale::English,
                        "start",
                        "From",
                        &mut start,
                    );
                    ui.add_space(12.0);
                    super::date_selector(ui, crate::i18n::Locale::English, "end", "To", &mut end);
                    assert!(
                        ui.min_rect().right() <= right + 0.5,
                        "date selectors overflow at {width}"
                    );
                    assert_eq!(start, "29/02/2024");
                    assert_eq!(end, "");
                });
                output.textures_delta.clear();
                let labels: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|shape| {
                        if let egui::epaint::Shape::Text(text) = &shape.shape {
                            let label = text.galley.text();
                            if matches!(label, "Year" | "Month" | "Day") {
                                return Some((label, text.pos));
                            }
                        }
                        None
                    })
                    .collect();
                assert_eq!(labels.len(), 6);
                for row in labels.as_chunks::<3>().0 {
                    assert_eq!([row[0].0, row[1].0, row[2].0], ["Year", "Month", "Day"]);
                    assert!(row[0].1.x < row[1].1.x && row[1].1.x < row[2].1.x);
                    assert!((row[0].1.y - row[1].1.y).abs() < 0.5);
                    assert!((row[1].1.y - row[2].1.y).abs() < 0.5);
                }
                for column in 0..3 {
                    assert!((labels[column].1.x - labels[column + 3].1.x).abs() < 0.5);
                }
            }
        }
    }

    #[test]
    fn selectors_preserve_existing_period_precision_and_bounds() {
        for value in ["", "2024", "02/2024", "29/02/2024"] {
            let date = DateSelection::from_period(value);
            assert_eq!(date.period_value(), value);
            let period = Period {
                start: date.period_value(),
                end: String::new(),
            };
            assert!(period.bounds().is_ok());
        }
    }

    #[test]
    fn changing_year_or_month_clamps_the_day_to_the_calendar() {
        for (value, year, month, expected) in [
            ("29/02/2024", 2025, 2, "28/02/2025"),
            ("31/01/2024", 2024, 4, "30/04/2024"),
            ("31/01/2024", 2024, 2, "29/02/2024"),
            ("29/02/2000", 1900, 2, "28/02/1900"),
        ] {
            let mut date = DateSelection::from_period(value);
            date.year = Some(year);
            date.month = Some(month);
            date.normalize();
            assert_eq!(date.period_value(), expected);
        }
    }

    #[test]
    fn clearing_a_component_clears_more_precise_components() {
        let mut date = DateSelection::from_period("29/02/2024");
        date.month = None;
        date.normalize();
        assert_eq!(date.period_value(), "2024");
        assert_eq!(date.day, None);
        date.year = None;
        date.normalize();
        assert_eq!(date, DateSelection::default());
    }
}

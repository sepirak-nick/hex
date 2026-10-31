//! The speech model: the local transcription picker, a modal listing every
//! model with the saved language's recommendations first, and the Language
//! select below the model in Settings. Choosing a model asks the service to
//! download, verify, and load it (`ChooseModel`) with the saved language, or
//! the model's default language when it does not support that one; changing
//! only the language prepares the same model again. Progress comes back in
//! the service snapshot, and the service saves the selection when it is
//! ready. Downloaded models other than the active one can be deleted after a
//! confirmation.

use rune::{El, TextEdit, col, el, row, text};
use rune_kit::{Badge, ControlSize, IconButton, IconButtonVariant, Progress, TextInput};

use super::controls::{ButtonKind, Paint, button, modal_scrim_transition, modal_transition};
use super::lists::error_panel;
use super::{Request, Ui};
use crate::app_settings::AppSettings;
use crate::desktop_i18n::{tr, tr_fill, tr_named};
use crate::elevenlabs::{self, KEY_ENVIRONMENT, KeySource};
use crate::transcription_models::{
    self, ModelDefinition, ModelPreparationStage, ModelRuntime, TranscriptionModelId,
    TranscriptionSelection, language_name,
};

const MODAL_WIDTH: f32 = 620.0;
const MODAL_HEIGHT: f32 = 520.0;

/// A model is the active selection only once it is installed.
fn transcription_selection_is_active(
    selection: &TranscriptionSelection,
    model: &ModelDefinition,
    installed: bool,
) -> bool {
    installed && selection.model == model.id
}

/// The language a model choice replaced because the model does not support
/// it; Settings names the change until the next choice.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct LanguageFallback {
    pub(super) model: TranscriptionModelId,
    pub(super) from: String,
}

#[derive(Default)]
pub(super) struct Picker {
    pub(super) open: bool,
    /// The model whose Delete is armed.
    deletion: Option<TranscriptionModelId>,
    deletion_error: Option<String>,
    /// Installed models for fixtures; live windows ask the filesystem.
    pub(super) installed: Option<Vec<TranscriptionModelId>>,
    pub(super) fallback: Option<LanguageFallback>,
    /// The cloud model's API key as typed; cleared once saved.
    key_edit: TextEdit,
    /// Where the key in use comes from, read as the picker opens.
    key_source: Option<KeySource>,
    key_error: Option<String>,
}

impl Picker {
    pub(super) fn installed(&self, model: &ModelDefinition, language: &str) -> bool {
        match &self.installed {
            Some(installed) => installed.contains(&model.id),
            None => transcription_models::is_installed(model, language),
        }
    }

    pub(super) fn selected_installed(&self, settings: &AppSettings) -> bool {
        transcription_models::validate(&settings.transcription)
            .is_ok_and(|model| self.installed(model, &settings.transcription.language))
    }

    fn clear_deletion(&mut self) {
        self.deletion = None;
        self.deletion_error = None;
    }
}

pub(super) fn open(ui: &mut Ui) {
    super::explainer::close(ui);
    super::sound_sets::close(ui);
    super::vocabulary::close(ui);
    ui.open = None;
    ui.picker.open = true;
    ui.picker.clear_deletion();
    if ui.live() {
        ui.picker.key_source = Some(elevenlabs::key_source());
    }
}

pub(super) fn close(ui: &mut Ui) {
    ui.picker.open = false;
    ui.picker.clear_deletion();
    forget_typed_key(ui);
}

fn forget_typed_key(ui: &mut Ui) {
    ui.picker.key_edit = TextEdit::new("");
    ui.picker.key_error = None;
}

/// Saves the typed key to its owner-only file. It never reaches settings.
fn save_key(ui: &mut Ui) {
    let saved = if ui.live() {
        elevenlabs::save_key(&ui.picker.key_edit.text)
    } else {
        Ok(())
    };
    match saved {
        Ok(()) => {
            forget_typed_key(ui);
            ui.picker.key_source = Some(if ui.live() {
                elevenlabs::key_source()
            } else {
                KeySource::File
            });
        }
        Err(error) => ui.picker.key_error = Some(error.to_string()),
    }
}

fn remove_key(ui: &mut Ui) {
    let removed = if ui.live() {
        elevenlabs::remove_key()
    } else {
        Ok(())
    };
    match removed {
        Ok(()) => {
            forget_typed_key(ui);
            ui.picker.key_source = Some(if ui.live() {
                elevenlabs::key_source()
            } else {
                KeySource::Missing
            });
        }
        Err(error) => ui.picker.key_error = Some(error.to_string()),
    }
}

/// The cloud card's key row: a masked field to add one, or where it's from.
fn key_row(ui: &Ui, p: Paint) -> El {
    let note = |key: &str, words: &str| {
        text(words)
            .key(key)
            .font(p.k.ty.small)
            .color(p.muted())
            .grow(1.0)
            .shrink()
    };
    let mut row = row().key("api-key").gap(8.0).center_y();
    match ui.picker.key_source.unwrap_or(KeySource::Missing) {
        KeySource::Environment => {
            row = row.child(note("key-source", &format!("From {KEY_ENVIRONMENT}")));
        }
        KeySource::File => {
            row = row.child(note("key-source", "Key saved")).child(button(
                p,
                "remove-key",
                "Remove",
                ButtonKind::Secondary,
                |ui: &mut Ui, _| remove_key(ui),
            ));
        }
        KeySource::Missing => {
            row = row
                .child(
                    TextInput::new(&ui.picker.key_edit)
                        .secure()
                        .placeholder("ElevenLabs API key")
                        .on_edit(|ui: &mut Ui| &mut ui.picker.key_edit)
                        .on_submit(|ui: &mut Ui, _| save_key(ui))
                        .build(p.k)
                        .key("key-field")
                        .grow(1.0)
                        .shrink(),
                )
                .child(button(
                    p,
                    "save-key",
                    "Save",
                    ButtonKind::Secondary,
                    |ui: &mut Ui, _| save_key(ui),
                ));
        }
    }
    row
}

fn cancel(ui: &mut Ui) {
    if ui.preparing.is_some() {
        ui.send(Request::CancelModel);
    }
}

/// Asks for `model` with the saved language when it supports it, else with
/// its default language, which Settings then names.
fn choose(ui: &mut Ui, model: &'static ModelDefinition) {
    ui.picker.clear_deletion();
    let current = &ui.settings.transcription;
    let installed = ui.picker.installed(model, &current.language);
    if transcription_selection_is_active(current, model, installed) {
        close(ui);
        return;
    }
    let language = model.language_for(&current.language).to_owned();
    ui.picker.fallback = (language != current.language).then(|| LanguageFallback {
        model: model.id,
        from: current.language.clone(),
    });
    ui.send(Request::ChooseModel {
        model: model.id,
        language,
    });
}

/// Prepares the saved model again for `language`, which installs or loads
/// whatever that language needs (Apple Speech's locales).
pub(super) fn choose_language(ui: &mut Ui, language: &str) {
    ui.open = None;
    let current = &ui.settings.transcription;
    if current.language == language {
        return;
    }
    ui.picker.fallback = None;
    ui.send(Request::ChooseModel {
        model: current.model,
        language: language.to_owned(),
    });
}

/// The fallback's note, once the service saved the model it was chosen for.
pub(super) fn fallback_note(ui: &Ui) -> Option<String> {
    let fallback = ui.picker.fallback.as_ref()?;
    let selection = &ui.settings.transcription;
    (selection.model == fallback.model && selection.language != fallback.from).then(|| {
        tr_named(
            "{model} doesn't support {previous}, so the language changed to {language}",
            &[
                (
                    "model",
                    transcription_models::definition(fallback.model).name,
                ),
                ("previous", language_name(&fallback.from)),
                ("language", language_name(&selection.language)),
            ],
        )
    })
}

fn delete(ui: &mut Ui, model: &'static ModelDefinition) {
    ui.picker.deletion_error = None;
    if ui.picker.deletion != Some(model.id) {
        ui.picker.deletion = Some(model.id);
        return;
    }
    ui.picker.deletion = None;
    if !ui.live() {
        return;
    }
    match transcription_models::remove_installed(model) {
        Ok(()) => tracing::info!(model = ?model.id, "deleted transcription model"),
        Err(error) => ui.picker.deletion_error = Some(error.to_string()),
    }
}

/// The picker's scrim and panel over the window, when open: the scrim fades
/// and the panel scales in as one surface ([`modal_transition`]).
pub(super) fn modal(ui: &Ui, p: Paint) -> Option<[El; 2]> {
    if !ui.picker.open {
        return None;
    }
    let c = &p.k.c;
    let language = ui.settings.transcription.language.clone();
    let mut models = col().key("models").gap(8.0);
    let transcription = ui.view.as_ref().map(|view| &view.transcription);
    if let Some(error) = transcription.and_then(|t| t.error.clone()) {
        models = models.child(error_panel(
            p,
            "install-error",
            tr("Model could not be installed."),
            &error,
        ));
    }
    if let Some(error) = &ui.picker.deletion_error {
        models = models.child(error_panel(
            p,
            "delete-error",
            tr("Model could not be deleted."),
            error,
        ));
    }
    for (index, (model, recommendation)) in transcription_models::picker_models(&language)
        .into_iter()
        .enumerate()
    {
        models = models.child(model_card(ui, p, index, model, recommendation, &language));
    }
    // The title says local until a cloud model is on offer.
    let title =
        if transcription_models::definition(TranscriptionModelId::ElevenLabsScribeV2).available() {
            "Transcription"
        } else {
            tr("Local transcription")
        };
    let header = row()
        .key("picker-header")
        .h(52.0)
        .pl(20.0)
        .pr(12.0)
        .center_y()
        .child(
            text(title)
                .key("title")
                .font(p.k.ty.body_medium)
                .color(p.text())
                .grow(1.0),
        )
        .child(
            IconButton::new(p.k.glyph.close)
                .variant(IconButtonVariant::Ghost)
                .control_size(ControlSize::Regular)
                .label(tr("Close"))
                .on_press(close)
                .build(p.k)
                .key("close-picker"),
        );
    let mut panel = col()
        .key("transcription-picker")
        .role("dialog")
        .w(MODAL_WIDTH)
        .h(MODAL_HEIGHT)
        .radius(p.k.radius.xl)
        .fill(c.bg_base)
        .border(p.hair, c.border_weak)
        // Children sit inside the border, so their fills never paint over it.
        .padding(p.hair)
        .clip_soft(0.0)
        // Presses inside the panel stay inside.
        .on_press(|_: &mut Ui| {})
        .child(header)
        .child(el().key("rule").h(p.hair).w_full().fill(c.border_weak))
        .child(
            col()
                .key("model-column")
                .grow(1.0)
                .shrink()
                .w_full()
                .pt(12.0)
                .child(super::scroll_body("model-scroll", models.px(16.0))),
        );
    for shadow in &p.k.elevation.overlay {
        panel = panel.box_shadow(*shadow);
    }
    let layer = |key: &str| {
        el().key(key)
            .role("none")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
    };
    Some([
        layer("picker-scrim")
            .fill(c.scrim)
            .on_press(close)
            .transition(modal_scrim_transition(p)),
        layer("picker-layer")
            .center()
            .transition(modal_transition(p))
            // Keys stay inside, Escape or a press outside closes, and focus
            // returns to the control that opened it.
            .child(panel.modal(true, close)),
    ])
}

enum Status {
    Active,
    Available {
        installed: bool,
    },
    Preparing {
        label: String,
        progress: Option<f32>,
    },
}

fn model_card(
    ui: &Ui,
    p: Paint,
    index: usize,
    model: &'static ModelDefinition,
    recommendation: Option<transcription_models::Recommendation>,
    language: &str,
) -> El {
    let c = &p.k.c;
    let installed = ui.picker.installed(model, model.language_for(language));
    let transcription = ui.view.as_ref().map(|view| &view.transcription);
    let preparing = transcription.and_then(|t| t.preparing) == Some(model.id);
    let active = transcription_selection_is_active(&ui.settings.transcription, model, installed);
    let status = if preparing {
        let transcription = transcription.expect("preparing comes from the snapshot");
        match transcription.preparation_stage {
            _ if model.is_cloud() => Status::Preparing {
                label: "Checking the API key".into(),
                progress: None,
            },
            Some(ModelPreparationStage::Verifying) => Status::Preparing {
                label: tr("Verifying model").into(),
                progress: None,
            },
            Some(ModelPreparationStage::Loading) => {
                let elapsed = ui
                    .preparing
                    .map_or(0, |(_, since)| since.elapsed().as_secs());
                Status::Preparing {
                    label: tr_fill("Loading model · {}s", &elapsed.to_string()),
                    progress: None,
                }
            }
            _ => {
                let fraction = model.download_bytes().map_or(0.0, |bytes| {
                    (transcription.downloaded_bytes as f32 / bytes as f32).clamp(0.0, 1.0)
                });
                Status::Preparing {
                    label: tr_fill("Downloading {}%", &format!("{:.0}", fraction * 100.0)),
                    progress: matches!(model.runtime, ModelRuntime::Gguf(_)).then_some(fraction),
                }
            }
        }
    } else if active {
        Status::Active
    } else {
        Status::Available { installed }
    };
    let deletable = installed
        && !preparing
        && matches!(model.runtime, ModelRuntime::Gguf(_))
        && ui.settings.transcription.model != model.id;
    let confirming = ui.picker.deletion == Some(model.id);
    let metadata = model
        .streams
        .then(|| tr("Transcribes while you speak on a GPU"))
        .into_iter()
        .chain([
            tr(model.coverage),
            tr(model.quality_context),
            tr(model.timestamps),
        ])
        .chain((installed && !active && !model.is_cloud()).then(|| tr("Installed")))
        .collect::<Vec<_>>()
        .join(" · ");
    // Without a key there's nothing to choose yet, and a press on the card
    // would also land on the key field inside it.
    let needs_key = model.is_cloud()
        && ui.picker.key_source.unwrap_or(KeySource::Missing) == KeySource::Missing;
    let mut actions = row().key("actions").gap(8.0).center_y();
    match &status {
        Status::Available { .. } if needs_key => {}
        Status::Active => {
            actions = actions.child(
                row()
                    .key("active")
                    .gap(6.0)
                    .center_y()
                    .child(super::controls::status_dot(p.accent()))
                    .child(
                        text(tr("Active"))
                            .key("label")
                            .font(p.k.ty.ui_medium)
                            .color(p.text()),
                    ),
            );
        }
        Status::Available { installed } => {
            if deletable {
                let label = if confirming {
                    tr_fill("Delete {}?", &model.size_label())
                } else {
                    tr("Delete").into()
                };
                let delete_button = button(
                    p,
                    "delete-model",
                    &label,
                    if confirming {
                        ButtonKind::Danger
                    } else {
                        ButtonKind::Secondary
                    },
                    move |ui: &mut Ui, _| delete(ui, model),
                );
                actions = actions.child(if confirming {
                    delete_button.default_button()
                } else {
                    delete_button
                });
            }
            actions = actions.child(button(
                p,
                "choose-model",
                if *installed {
                    tr("Use")
                } else {
                    tr("Download")
                },
                ButtonKind::Secondary,
                move |ui: &mut Ui, _| choose(ui, model),
            ));
        }
        Status::Preparing { .. } => {
            actions = actions.child(button(
                p,
                "cancel-model",
                tr("Cancel"),
                ButtonKind::Secondary,
                |ui: &mut Ui, _| cancel(ui),
            ));
        }
    }
    let mut title = row()
        .key("title")
        .gap(8.0)
        .center_y()
        .grow(1.0)
        .shrink()
        .child(
            text(model.name)
                .key("name")
                .font(p.k.ty.body_medium)
                .color(p.text()),
        );
    if let Some(recommendation) = recommendation {
        title = title.child(Badge::new(tr(recommendation.label())).build(p.k));
    }
    if model.streams {
        title = title.child(Badge::new(tr("Streams")).build(p.k));
    }
    if model.is_cloud() {
        title = title.child(Badge::new("Cloud").build(p.k));
    }
    let metric = |key: &str, label: &str, value: String| {
        row()
            .key(key)
            .gap(6.0)
            .child(text(label).key("label").font(p.k.ty.small).color(p.muted()))
            .child(
                text(value)
                    .key("value")
                    .font(p.k.ty.small_medium)
                    .color(p.text()),
            )
    };
    let mut card = col()
        .key(format!("model{index}"))
        .px(16.0)
        .py(12.0)
        .gap(8.0)
        .radius(p.k.radius.lg)
        .fill(if active { c.layer_02 } else { c.bg_base })
        .border(
            p.hair,
            if active {
                c.border_focus
            } else {
                c.border_weak
            },
        )
        .child(
            row()
                .key("top")
                .gap(12.0)
                .center_y()
                .child(title)
                .child(actions),
        )
        .child(
            text(metadata)
                .key("metadata")
                .font(p.k.ty.small)
                .color(p.muted())
                .wrap(),
        )
        .child(
            row()
                .key("metrics")
                .gap(20.0)
                .child(metric("speed", tr("Speed"), tr(model.realtime).into()))
                .child(metric("size", tr("Size"), model.size_label()))
                .child(metric(
                    "accuracy",
                    tr("Accuracy"),
                    model.accuracy_label().unwrap_or_else(|| {
                        if model.is_cloud() {
                            "Not published".into()
                        } else {
                            "System".into()
                        }
                    }),
                )),
        );
    if !model.supports_language(language) {
        card = card.child(
            text(tr_named(
                "Doesn't support {language}; switches the language to {fallback}",
                &[
                    ("language", language_name(language)),
                    ("fallback", language_name(model.default_language())),
                ],
            ))
            .key("language-switch")
            .font(p.k.ty.small)
            .color(p.warning())
            .wrap(),
        );
    }
    if model.id == TranscriptionModelId::ParakeetV3 {
        card = card.child(
            text(
                "Detects language from audio, not the selected language. \
                 For language-guided transcription, try Whisper.",
            )
            .key("note")
            .font(p.k.ty.small)
            .color(p.muted())
            .wrap(),
        );
    }
    if model.is_cloud() {
        card = card
            .child(
                text(
                    "Your dictation audio is sent to ElevenLabs, under their retention \
                     terms. Meetings, Annotate, and other apps stay on this Mac.",
                )
                .key("cloud-warning")
                .font(p.k.ty.small)
                .color(p.warning())
                .wrap(),
            )
            .child(key_row(ui, p));
        if let Some(error) = &ui.picker.key_error {
            card = card.child(
                text(error.as_str())
                    .key("key-error")
                    .font(p.k.ty.small)
                    .color(p.danger())
                    .wrap(),
            );
        }
    }
    // One bar from download through verifying and loading: it fills, then
    // sweeps while the stage has no measure.
    if let Status::Preparing { label, progress } = status {
        let bar = Progress::new(progress).label(label);
        let bar = if progress.is_some() {
            bar.show_value()
        } else {
            bar
        };
        card = card.child(bar.build(p.k).key("progress"));
    }
    if !active && !preparing && !needs_key {
        card = card
            .role("button")
            .hover(c.hover)
            .press(c.pressed)
            .on_press(move |ui: &mut Ui| choose(ui, model));
    }
    card
}

/// The picker open with English and Parakeet saved; `downloading` shows a
/// model mid-download.
pub(super) fn fixture(ui: &mut Ui, downloading: Option<TranscriptionModelId>) {
    ui.picker.open = true;
    if let (Some(model), Some(view)) = (downloading, ui.view.as_mut()) {
        let bytes = transcription_models::definition(model)
            .download_bytes()
            .unwrap_or(0);
        view.transcription.preparing = Some(model);
        view.transcription.downloaded_bytes = bytes * 37 / 100;
        view.transcription.preparation_stage = Some(ModelPreparationStage::Downloading);
        ui.preparing = Some((model, std::time::Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cloud_model() -> &'static ModelDefinition {
        transcription_models::definition(TranscriptionModelId::ElevenLabsScribeV2)
    }

    #[test]
    fn only_the_cloud_model_counts_as_cloud() {
        assert!(cloud_model().is_cloud());
        assert!(
            transcription_models::MODELS
                .iter()
                .filter(|model| model.is_cloud())
                .count()
                == 1
        );
    }

    #[test]
    fn selected_transcription_model_is_not_active_until_installed() {
        let selection = TranscriptionSelection::default();
        let model = transcription_models::definition(selection.model);

        assert!(!transcription_selection_is_active(&selection, model, false));
        assert!(transcription_selection_is_active(&selection, model, true));
    }
}

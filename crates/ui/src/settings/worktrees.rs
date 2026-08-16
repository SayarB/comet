//! Settings → Worktrees: device-local root selection and read-only inventory.

use gpui::{
    AnyElement, Context, Entity, PathPromptOptions, Render, SharedString, Subscription, Task,
    Window, div, prelude::*, px,
};
use zeron_proto::WorktreeSettings;
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons;
use crate::popover::{self, Loadable};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

fn draft_root(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn effective_location_text(settings: &WorktreeSettings) -> String {
    if settings.environment_override {
        format!(
            "Effective location: {} (overridden by ZERON_WORKTREES_DIR)",
            settings.effective_root
        )
    } else {
        format!("Effective location: {}", settings.effective_root)
    }
}

pub struct WorktreesPage {
    state: Entity<AppState>,
    input: Entity<ComposerInput>,
    settings: Loadable<WorktreeSettings>,
    error: Option<SharedString>,
    saving: bool,
    task: Option<Task<()>>,
    picker_task: Option<Task<()>>,
    _input_events: Subscription,
}

impl WorktreesPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| ComposerInput::new("Leave blank to use the default", cx));
        let input_events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.save(cx);
            }
        });
        let mut page = Self {
            state,
            input,
            settings: Loadable::Idle,
            error: None,
            saving: false,
            task: None,
            picker_task: None,
            _input_events: input_events,
        };
        page.load(cx);
        page
    }

    fn apply_settings(&mut self, settings: WorktreeSettings, cx: &mut Context<Self>) {
        let value = settings.custom_root.clone().unwrap_or_default();
        self.input.update(cx, |input, cx| input.set_text(value, cx));
        self.settings = Loadable::Ready(settings);
        self.error = None;
        self.saving = false;
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.settings = Loadable::Error("Engine not connected".into());
            cx.notify();
            return;
        };
        self.settings = Loadable::Loading;
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::GET_WORKTREE_SETTINGS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => match serde_json::from_value::<WorktreeSettings>(value) {
                        Ok(settings) => page.apply_settings(settings, cx),
                        Err(err) => page.settings = Loadable::Error(err.to_string()),
                    },
                    Err(err) => page.settings = Loadable::Error(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let custom_root = draft_root(self.input.read(cx).text());
        self.saving = true;
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SET_WORKTREE_ROOT,
                    serde_json::json!({ "customRoot": custom_root }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.saving = false;
                match result {
                    Ok(value) => match serde_json::from_value::<WorktreeSettings>(value) {
                        Ok(settings) => page.apply_settings(settings, cx),
                        Err(err) => page.error = Some(err.to_string().into()),
                    },
                    Err(err) => page.error = Some(format!("Could not save: {err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.save(cx);
    }

    fn choose_directory(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose worktree location".into()),
        });
        self.picker_task = Some(cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |page, cx| {
                    page.input.update(cx, |input, cx| {
                        input.set_text(path.to_string_lossy().to_string(), cx)
                    });
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn render_ready(
        &mut self,
        settings: WorktreeSettings,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let input = self.input.clone();
        let saving = self.saving;
        let location = widgets::section_card(theme)
            .child(
                div()
                    .px(px(20.0))
                    .py(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(widgets::field_label(theme, "Worktree location"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(popover::dialog_field(input.into_any_element())),
                            )
                            .child(
                                popover::btn_ghost(theme, "Choose…", "worktrees-choose")
                                    .id("worktrees-choose")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.choose_directory(cx)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(12.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(
                                        "New worktrees use this location. Existing worktrees are not moved.",
                                    )),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .gap(px(8.0))
                                    .child(
                                        popover::btn_ghost(
                                            theme,
                                            "Reset",
                                            "worktrees-reset",
                                        )
                                        .id("worktrees-reset")
                                        .on_click(cx.listener(|this, _, _, cx| this.reset(cx))),
                                    )
                                    .child(
                                        popover::btn_primary(
                                            theme,
                                            if saving { "Saving…" } else { "Save" },
                                        )
                                        .id("worktrees-save")
                                        .opacity(if saving { 0.6 } else { 1.0 })
                                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .pt(px(2.0))
                            .text_size(px(11.5))
                            .text_color(if settings.environment_override {
                                theme.warning
                            } else {
                                theme.text_muted.opacity(0.7)
                            })
                            .child(SharedString::from(effective_location_text(&settings))),
                    ),
            );

        let rows: Vec<AnyElement> = settings
            .worktrees
            .iter()
            .enumerate()
            .map(|(index, worktree)| {
                widgets::card_row(theme, index == 0)
                    .child(widgets::row_tile(theme, icons::GIT_BRANCH))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(
                                theme,
                                format!("{} / {}", worktree.repo_name, worktree.name),
                            ))
                            .child(
                                div()
                                    .mt(px(3.0))
                                    .truncate()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted.opacity(0.65))
                                    .child(SharedString::from(worktree.path.clone())),
                            ),
                    )
                    .into_any_element()
            })
            .collect();
        let inventory = widgets::section_card(theme).when(rows.is_empty(), |card| {
            card.child(
                div()
                    .px(px(20.0))
                    .py(px(32.0))
                    .text_center()
                    .text_size(px(13.0))
                    .text_color(theme.text_muted.opacity(0.65))
                    .child(SharedString::from("No worktrees in this location")),
            )
        });
        let inventory = if rows.is_empty() {
            inventory
        } else {
            inventory.children(rows)
        };

        widgets::page_column()
            .child(widgets::page_header(
                theme,
                "Worktrees",
                Some(settings.worktrees.len()),
            ))
            .child(widgets::page_subtitle(
                theme,
                "Choose where this device creates isolated working copies.",
            ))
            .when_some(self.error.clone(), |page, error| {
                page.child(
                    widgets::error_strip(theme, error)
                        .id("worktrees-error")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.error = None;
                            cx.notify();
                        })),
                )
            })
            .child(location)
            .child(
                div()
                    .mt(px(28.0))
                    .child(widgets::field_label(theme, "Current worktrees")),
            )
            .child(inventory)
            .into_any_element()
    }
}

impl Render for WorktreesPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let content = match self.settings.clone() {
            Loadable::Idle | Loadable::Loading => widgets::page_column()
                .child(widgets::page_header(&theme, "Worktrees", None))
                .child(widgets::page_subtitle(
                    &theme,
                    "Choose where this device creates isolated working copies.",
                ))
                .child(popover::skeleton_rows(
                    "worktrees-loading",
                    &theme,
                    4,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(error) => widgets::page_column()
                .child(widgets::page_header(&theme, "Worktrees", None))
                .child(
                    widgets::error_strip(&theme, error)
                        .id("worktrees-load-error")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                )
                .into_any_element(),
            Loadable::Ready(settings) => self.render_ready(settings, &theme, cx),
        };
        div()
            .id("worktrees-page")
            .size_full()
            .overflow_y_scroll()
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(environment_override: bool) -> WorktreeSettings {
        WorktreeSettings {
            custom_root: Some("/saved".into()),
            effective_root: "/effective".into(),
            environment_override,
            worktrees: Vec::new(),
        }
    }

    #[test]
    fn blank_draft_resets_and_non_blank_is_trimmed() {
        assert_eq!(draft_root("  "), None);
        assert_eq!(
            draft_root("  /tmp/worktrees  "),
            Some("/tmp/worktrees".into())
        );
    }

    #[test]
    fn effective_location_names_environment_override() {
        assert_eq!(
            effective_location_text(&settings(false)),
            "Effective location: /effective"
        );
        assert_eq!(
            effective_location_text(&settings(true)),
            "Effective location: /effective (overridden by ZERON_WORKTREES_DIR)"
        );
    }
}

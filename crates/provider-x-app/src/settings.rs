//! Native settings view. The control plane owns operations and persisted preferences.
mod providers;
use crate::{
    runtime::AppServices,
    settings_state::{ModelDraft, OperationKind, SettingsSnapshot, SettingsState},
    ui_preferences::ThemePreference,
};
use gpui_kit::base::{
    CheckboxState,
    input::{InputEvent, InputState},
};
use gpui_kit::{
    AnyElement, App, AppContext, ClickEvent, Div, Entity, Focusable, FontWeight, Point,
    ScrollHandle, SharedString, Subscription, Task, Window, WindowAppearance, div, prelude::*, px,
    rems,
};
use gpui_omarchy::{self as ui, ActiveTheme, ButtonVariant, ChoiceItem, ChoiceState};
use provider_x_core::{MetadataSource, ProtocolId};
use provider_x_providers::CredentialKind;
use std::collections::BTreeMap;
use std::time::Duration;

fn text(key: &str) -> String {
    rust_i18n::t!(format!("settings.{key}")).to_string()
}
fn uses_dark_theme(preference: ThemePreference, appearance: WindowAppearance) -> bool {
    match preference {
        ThemePreference::System => matches!(
            appearance,
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ),
        ThemePreference::Dark => true,
        ThemePreference::Light => false,
    }
}

fn column() -> Div {
    div().flex().flex_col().gap_4().min_w_0()
}
fn row() -> Div {
    div().flex().items_center().gap_3().min_w_0()
}
fn muted(value: impl Into<SharedString>, cx: &App) -> Div {
    div().text_color(cx.omarchy().secondary).child(value.into())
}
fn heading(title: &str, description: &str, cx: &App) -> Div {
    column()
        .gap_2()
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::BOLD)
                .child(text(title)),
        )
        .child(muted(text(description), cx))
}
fn setting(title: &str, description: &str, control: impl IntoElement, cx: &App) -> Div {
    row()
        .py_2()
        .child(
            column()
                .gap_1()
                .flex_1()
                .child(text(title))
                .when(!description.is_empty(), |d| {
                    d.child(muted(text(description), cx))
                }),
        )
        .child(control)
}

fn toggled<F: Fn(&mut Settings, bool, &mut Window, &mut Context<Settings>) + 'static>(
    cx: &Context<Settings>,
    callback: F,
) -> impl Fn(bool, &ClickEvent, &mut Window, &mut App) + use<F> {
    let view = cx.entity().downgrade();
    move |value, _, window, cx| {
        let _ = view.update(cx, |this, cx| callback(this, value, window, cx));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Providers,
    Integration,
    General,
}

pub(crate) struct Settings {
    host: SettingsState,
    services: AppServices,
    data: SettingsSnapshot,
    page: Page,
    fields: BTreeMap<&'static str, Entity<InputState>>,
    choices: BTreeMap<&'static str, Entity<ChoiceState>>,
    choice_values: BTreeMap<&'static str, String>,
    subscriptions: Vec<Subscription>,
    input_subscriptions: Vec<Subscription>,
    search: Entity<InputState>,
    model_search: Entity<InputState>,
    model_editor: Option<String>,
    model_page: usize,
    advanced: bool,
    scroll: ScrollHandle,
    _refresh: Task<()>,
}

impl Settings {
    fn notification(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let color = if self.data.failed {
            cx.omarchy().danger
        } else {
            cx.omarchy().foreground
        };
        let message = if self.data.busy {
            text("working")
        } else {
            self.data.message.clone()
        };
        let mut content = row()
            .items_start()
            .child(
                ui::icon(if self.data.failed {
                    ui::IconName::TriangleAlert
                } else {
                    ui::IconName::Info
                })
                .size(px(16.))
                .flex_shrink_0()
                .text_color(color),
            )
            .child(
                div()
                    .id("notification-message")
                    .flex_1()
                    .min_w_0()
                    .max_h(px(160.))
                    .overflow_y_scroll()
                    .text_color(color)
                    .child(message),
            );
        if self.data.busy {
            if self.data.cancellable && self.data.operation_kind != OperationKind::Login {
                content = content.child(self.action_button("cancel", "cancel", "cancel", cx));
            }
        } else {
            content = content.child(
                ui::button("dismiss-notification", "", ButtonVariant::Secondary, cx)
                    .size(px(20.))
                    .p_0()
                    .flex_shrink_0()
                    .accessibility_label(text("dismiss_notification"))
                    .child(ui::icon(ui::IconName::Close).size(px(14.)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.host.dismiss_message();
                        this.refresh(window, cx);
                    })),
            );
        }
        // Out of document flow: feedback must never resize or move the form.
        div()
            .id("settings-notification")
            .absolute()
            .top_3()
            .right_6()
            .w(px(400.))
            .occlude()
            .child(ui::toast("settings-toast", cx).child(content))
    }

    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let host = cx.global::<SettingsState>().clone();
        let services = cx.global::<AppServices>().clone();
        let data = host
            .snapshot(&services)
            .expect("settings state initialized");
        Self::apply_theme(data.theme, window, cx);
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(text("search")));
        let model_search =
            cx.new(|cx| InputState::new(window, cx).placeholder(text("model_search")));
        cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        cx.subscribe(&model_search, |this, _, _: &InputEvent, cx| {
            this.model_page = 0;
            cx.notify();
        })
        .detach();
        cx.observe_window_appearance(window, |this, window, cx| {
            Self::apply_theme(this.data.theme, window, cx);
            cx.notify();
        })
        .detach();
        let refresh = cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                if view.update_in(cx, Settings::refresh).is_err() {
                    break;
                }
            }
        });
        let mut view = Self {
            host,
            services,
            data,
            page: Page::Providers,
            fields: BTreeMap::new(),
            choices: BTreeMap::new(),
            choice_values: BTreeMap::new(),
            subscriptions: Vec::new(),
            input_subscriptions: Vec::new(),
            search,
            model_search,
            model_editor: None,
            model_page: 0,
            advanced: false,
            scroll: ScrollHandle::new(),
            _refresh: refresh,
        };
        view.restore_fields(window, cx);
        view.restore_choices(window, cx);
        view
    }

    fn apply_theme(preference: ThemePreference, window: &Window, cx: &mut App) {
        let dark = uses_dark_theme(preference, window.appearance());
        // Explicit application stops Omarchy's file watcher; macOS appearance is our source.
        if dark {
            ui::Theme::tokyo_night()
        } else {
            ui::Theme::flexoki_light()
        }
        .apply(cx);
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(next) = self.host.snapshot(&self.services) else {
            return;
        };
        if next.revision == self.data.revision
            && next.effective_locale == self.data.effective_locale
        {
            return;
        }
        let theme_changed = next.theme != self.data.theme;
        let locale_changed = next.effective_locale != self.data.effective_locale;
        let busy_changed = next.busy != self.data.busy;
        let selection_changed = next.theme != self.data.theme || next.locale != self.data.locale;
        let draft_changed = next.draft.id != self.data.draft.id
            || next.draft.connection != self.data.draft.connection;
        self.data = next;
        if theme_changed {
            Self::apply_theme(self.data.theme, window, cx);
        }
        if draft_changed {
            self.model_editor = None;
            self.restore_fields(window, cx);
        }
        if locale_changed {
            self.search
                .update(cx, |s, cx| s.set_placeholder(text("search"), window, cx));
            self.model_search.update(cx, |s, cx| {
                s.set_placeholder(text("model_search"), window, cx);
            });
        }
        if busy_changed {
            for field in self.fields.values() {
                field.update(cx, |f, cx| f.set_disabled(self.data.busy, cx));
            }
        }
        if locale_changed || selection_changed || draft_changed || busy_changed {
            self.restore_choices(window, cx);
        }
        cx.notify();
    }

    fn perform(&mut self, action: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = self.host.action(action, &self.services, cx) {
            self.host.set_message(error.to_string());
        }
        self.refresh(window, cx);
    }
    fn edit(&self) {
        self.host.edit(self.data.draft.clone());
    }
    fn choose(
        &mut self,
        family: &str,
        connection: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.host.select(family, connection) {
            self.host.set_message(error.to_string());
        }
        self.refresh(window, cx);
        self.model_editor = None;
        self.restore_fields(window, cx);
        self.restore_choices(window, cx);
        self.scroll.set_offset(Point::default());
        cx.notify();
    }
    fn field_state(
        &mut self,
        key: &'static str,
        value: String,
        placeholder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(value)
                .placeholder(placeholder)
                .masked(key == "key")
        });
        self.input_subscriptions.push(cx.subscribe(
            &input,
            move |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) && !this.data.busy {
                    let value = input.read(cx).value().to_string();
                    this.input_changed(key, value);
                    cx.notify();
                }
            },
        ));
        self.fields.insert(key, input);
    }
    fn restore_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input_subscriptions.clear();
        self.fields.clear();
        let d = self.data.draft.clone();
        for (key, value) in [
            ("name", d.name),
            ("endpoint", d.endpoint),
            ("model_endpoint", d.model_endpoint),
            ("websocket_endpoint", d.websocket_endpoint),
        ] {
            self.field_state(key, value, String::new(), window, cx);
        }
        self.field_state(
            "key",
            String::new(),
            text(if self.data.has_credential {
                "credential_saved"
            } else {
                "api_key"
            }),
            window,
            cx,
        );
        self.field_state(
            "model",
            String::new(),
            text("model_placeholder"),
            window,
            cx,
        );
        if let Some(model) = self
            .model_editor
            .as_ref()
            .and_then(|id| self.data.draft.models.iter().find(|m| &m.id == id))
            .cloned()
        {
            self.field_state("model_name", model.name, String::new(), window, cx);
            self.field_state(
                "context_window",
                model
                    .context_window
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                String::new(),
                window,
                cx,
            );
        }
    }
    fn input_changed(&mut self, key: &str, value: String) {
        match key {
            "name" => self.data.draft.name = value,
            "endpoint" => self.data.draft.endpoint = value,
            "model_endpoint" => self.data.draft.model_endpoint = value,
            "websocket_endpoint" => self.data.draft.websocket_endpoint = value,
            "key" => {
                self.host.secret(value);
                return;
            }
            "model_name" | "context_window" => {
                if let Some(model) = self.current_model_mut() {
                    if key == "model_name" {
                        model.name = value;
                    } else {
                        model.context_window = value.parse::<u64>().ok().filter(|v| *v > 0);
                    }
                    model.metadata_sources.insert(
                        if key == "model_name" {
                            "display_name"
                        } else {
                            key
                        }
                        .into(),
                        MetadataSource::UserConfirmed,
                    );
                }
            }
            _ => return,
        }
        self.edit();
    }
    fn current_model_mut(&mut self) -> Option<&mut ModelDraft> {
        let id = self.model_editor.as_ref()?;
        self.data.draft.models.iter_mut().find(|m| &m.id == id)
    }
    fn add_choice(
        &mut self,
        key: &'static str,
        label: &str,
        items: Vec<(String, String)>,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = items.iter().position(|(id, _)| id == value);
        let state = cx.new(|cx| {
            let state = ChoiceState::new(
                items
                    .into_iter()
                    .map(|(id, label)| ChoiceItem::new(id, label))
                    .collect(),
                window,
                cx,
            )
            .label(text(label))
            .disabled(self.data.busy);
            if let Some(index) = index {
                state.default_selected(index)
            } else {
                state
            }
        });
        self.choice_values.insert(key, value.to_owned());
        self.subscriptions.push(
            cx.observe_in(&state, window, move |this, state, window, cx| {
                let value = state.read(cx).selected().map(|s| s.value.to_string());
                if let Some(value) = value
                    && this.choice_values.get(key) != Some(&value)
                    && !this.data.busy
                {
                    this.choice_values.insert(key, value.clone());
                    this.choice_changed(key, &value, window, cx);
                }
                cx.notify();
            }),
        );
        self.choices.insert(key, state);
    }
    fn restore_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.choices.iter().find_map(|(key, state)| {
            state
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
                .then_some(*key)
        });
        self.subscriptions.clear();
        self.choices.clear();
        self.choice_values.clear();
        self.add_choice(
            "theme",
            "appearance",
            vec![
                ("system".into(), text("follow_system")),
                ("light".into(), text("light")),
                ("dark".into(), text("dark")),
            ],
            match self.data.theme {
                ThemePreference::System => "system",
                ThemePreference::Dark => "dark",
                ThemePreference::Light => "light",
            },
            window,
            cx,
        );
        self.add_choice(
            "locale",
            "language",
            vec![
                ("system".into(), text("automatic")),
                ("en".into(), "English".into()),
                ("zh-CN".into(), "简体中文".into()),
            ],
            self.data.locale.code(),
            window,
            cx,
        );
        self.add_choice(
            "protocol",
            "protocol",
            vec![
                ("responses".into(), "OpenAI Responses".into()),
                ("chat".into(), "Chat Completions".into()),
                ("messages".into(), "Anthropic Messages".into()),
            ],
            match self.data.draft.protocol {
                ProtocolId::OpenaiResponses => "responses",
                ProtocolId::OpenaiChatCompletions => "chat",
                ProtocolId::AnthropicMessages => "messages",
            },
            window,
            cx,
        );
        if let Some(model) = self.current_model_mut().cloned() {
            for (key, value) in [
                ("parallel_tools", model.parallel_tools),
                ("search_tool", model.search_tool),
            ] {
                self.add_choice(
                    key,
                    key,
                    vec![
                        ("auto".into(), text("automatic")),
                        ("yes".into(), text("enabled")),
                        ("no".into(), text("disabled")),
                    ],
                    match value {
                        None => "auto",
                        Some(true) => "yes",
                        Some(false) => "no",
                    },
                    window,
                    cx,
                );
            }
        }
        if let Some(state) = focused.and_then(|key| self.choices.get(key)) {
            state.read(cx).focus_handle(cx).focus(window, cx);
        }
    }
    fn choice_changed(
        &mut self,
        key: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match key {
            "theme" => self.perform(&format!("theme-{value}"), window, cx),
            "locale" => self.perform(&format!("locale-{value}"), window, cx),
            "protocol" => {
                self.data.draft.protocol = match value {
                    "chat" => ProtocolId::OpenaiChatCompletions,
                    "messages" => ProtocolId::AnthropicMessages,
                    _ => ProtocolId::OpenaiResponses,
                };
                self.edit();
            }
            "parallel_tools" | "search_tool" => {
                if let Some(model) = self.current_model_mut() {
                    let value = match value {
                        "yes" => Some(true),
                        "no" => Some(false),
                        _ => None,
                    };
                    if key == "parallel_tools" {
                        model.parallel_tools = value;
                    } else {
                        model.search_tool = value;
                    }
                    model.metadata_sources.insert(
                        if key == "parallel_tools" {
                            "supports_parallel_tool_calls"
                        } else {
                            "supports_search_tool"
                        }
                        .into(),
                        MetadataSource::UserConfirmed,
                    );
                }
                self.edit();
            }
            _ => {}
        }
        cx.notify();
    }
    fn picker(&self, key: &'static str, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .w(px(260.))
            .flex_shrink_0()
            .child(ui::select(key, &self.choices[key], window, cx))
            .into_any_element()
    }
    fn field(
        &self,
        key: &'static str,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        column()
            .gap_1()
            .child(text(label))
            .child(ui::input(key, &self.fields[key], window, cx))
    }
    fn action_button(
        &self,
        id: &'static str,
        label: &str,
        action: &'static str,
        cx: &mut Context<Self>,
    ) -> ui::Button {
        ui::button(id, text(label), ButtonVariant::Outline, cx)
            .disabled((self.data.busy || self.data.settings_pending) && action != "cancel")
            .on_click(cx.listener(move |this, _, window, cx| this.perform(action, window, cx)))
    }
    fn action_switch(
        &self,
        id: &'static str,
        label: &str,
        checked: bool,
        actions: (&'static str, &'static str),
        cx: &mut Context<Self>,
    ) -> gpui_kit::base::Switch {
        ui::switch(id, "", checked, cx)
            .accessibility_label(text(label))
            .disabled(self.data.busy)
            .on_change(toggled(cx, move |this, value, window, cx| {
                this.perform(if value { actions.0 } else { actions.1 }, window, cx);
            }))
    }
    fn general(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let appearance = self.picker("theme", window, cx);
        let locale = self.picker("locale", window, cx);
        let dock = self.action_switch(
            "dock",
            "dock_icon",
            self.data.dock_visible,
            ("dock-show", "dock-hide"),
            cx,
        );
        let startup = self.action_switch(
            "startup",
            "launch_at_login",
            self.data.startup != crate::platform::macos::LaunchAtLoginStatus::Disabled,
            ("startup-enable", "startup-disable"),
            cx,
        );
        column()
            .child(heading("general", "general_description", cx))
            .child(
                ui::panel(text("display_group"), cx)
                    .child(setting("appearance", "appearance_hint", appearance, cx))
                    .child(setting("language", "language_hint", locale, cx)),
            )
            .child(
                ui::panel(text("behavior_group"), cx)
                    .child(setting("dock_icon", "dock_hint", dock, cx))
                    .child(setting(
                        "launch_at_login",
                        if self.data.startup
                            == crate::platform::macos::LaunchAtLoginStatus::RequiresApproval
                        {
                            "login_approval"
                        } else {
                            "login_hint"
                        },
                        startup,
                        cx,
                    )),
            )
            .child(muted(text("general_hint"), cx))
    }
    fn integration(&self, cx: &mut Context<Self>) -> Div {
        let control = ui::switch("integration", "", self.data.integration == "active", cx)
            .accessibility_label(text("codex_connection"))
            .disabled(self.data.busy || !matches!(self.data.integration, "active" | "inactive"))
            .on_change(toggled(cx, |this, value, window, cx| {
                this.perform(
                    if value {
                        "integration-enable"
                    } else {
                        "integration-disable"
                    },
                    window,
                    cx,
                );
            }));
        column()
            .child(heading("integration", "integration_description", cx))
            .child(ui::panel("Codex", cx).child(setting(
                "codex_connection",
                &format!("integration_{}", self.data.integration),
                control,
                cx,
            )))
            .when(self.data.integration == "changed", |d| {
                d.child(self.action_button(
                    "restore",
                    "disable_integration",
                    "integration-disable",
                    cx,
                ))
            })
            .child(muted(text("integration_hint"), cx))
    }
}

impl Render for Settings {
    #[allow(clippy::too_many_lines)] // Window composition and action footer.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.omarchy().clone();
        let mut nav = column()
            .w(px(206.))
            .h_full()
            .flex_shrink_0()
            .pt(px(54.))
            .px_4()
            .pb_5()
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .child("ProviderX"),
            )
            .child(muted(text("settings"), cx).mb_4());
        for (page, key) in [
            (Page::Providers, "providers"),
            (Page::Integration, "integration"),
            (Page::General, "general"),
        ] {
            nav = nav.child(
                ui::button(key, text(key), ButtonVariant::Secondary, cx)
                    .selected(self.page == page)
                    .h(px(40.))
                    .w_full()
                    .justify_start()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.page = page;
                        this.scroll.set_offset(Point::default());
                        if page == Page::Integration {
                            this.perform("integration-status", window, cx);
                        }
                        cx.notify();
                    })),
            );
        }
        let body = match self.page {
            Page::Providers => self.providers(window, cx),
            Page::Integration => self.integration(cx),
            Page::General => self.general(window, cx),
        };
        let mut main = column()
            .gap_0()
            .h_full()
            .flex_1()
            .child(div().h(px(40.)).flex_shrink_0())
            .child(
                div()
                    .id("settings-scroll")
                    .track_scroll(&self.scroll)
                    .overflow_y_scroll()
                    .flex_1()
                    .min_h_0()
                    .px_8()
                    .pt_3()
                    .pb_7()
                    .child(body),
            );
        let editing = self.page == Page::Providers && !self.data.draft.preset.is_empty();
        if editing {
            let mut footer = column()
                .gap_2()
                .px_8()
                .py_3()
                .border_t_1()
                .border_color(theme.border);
            let claude = provider_x_providers::presets()
                .iter()
                .find(|p| p.id == self.data.draft.preset)
                .and_then(|p| {
                    p.connections
                        .iter()
                        .find(|c| c.id == self.data.draft.connection)
                })
                .is_some_and(|c| c.credentials == CredentialKind::ClaudeCode);
            let mut actions = row().child(muted(text("save_hint"), cx).flex_1());
            if !claude {
                actions = actions.child(self.action_button("test", "test", "test", cx));
            }
            footer = footer.child(actions.child(self.action_button("save", "save", "save", cx)));
            main = main.child(footer);
        }
        ui::focus_scope("settings")
            .relative()
            .size_full()
            .font_family(theme.font.clone())
            .text_size(rems(0.8125))
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                row()
                    .gap_0()
                    .items_start()
                    .size_full()
                    .child(nav)
                    .child(main),
            )
            .when(self.data.busy || !self.data.message.is_empty(), |root| {
                root.child(self.notification(cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_theme_resolves_macos_light_and_vibrant_dark() {
        for appearance in [WindowAppearance::Light, WindowAppearance::VibrantLight] {
            assert!(!uses_dark_theme(ThemePreference::System, appearance));
        }
        for appearance in [WindowAppearance::Dark, WindowAppearance::VibrantDark] {
            assert!(uses_dark_theme(ThemePreference::System, appearance));
        }
    }

    #[test]
    fn explicit_theme_ignores_macos_appearance_changes() {
        for appearance in [WindowAppearance::Light, WindowAppearance::Dark] {
            assert!(!uses_dark_theme(ThemePreference::Light, appearance));
            assert!(uses_dark_theme(ThemePreference::Dark, appearance));
        }
    }
}

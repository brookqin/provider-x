use gpui_kit::{
    InteractiveElement, IntoElement, StatefulInteractiveElement, prelude::FluentBuilder,
};

use super::{
    ActiveTheme, ButtonVariant, CheckboxState, Context, CredentialKind, Div, FontWeight,
    MetadataSource, ModelDraft, OperationKind, ParentElement, Point, ProtocolId, Settings,
    SharedString, Styled, Window, column, div, heading, muted, px, row, setting, text, toggled, ui,
};

impl Settings {
    fn provider_dialog_width(window: &Window) -> f32 {
        (f32::from(window.viewport_size().width) - 64.).min(800.)
    }

    fn open_provider_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.provider_return_focus = window.focused(cx);
        self.provider_dialog_open = true;
        self.provider_commit_pending = false;
        self.provider_removal = None;
        self.provider_scroll.set_offset(Point::default());
        self.model_editor = None;
        self.model_page = 0;
        self.advanced = false;
        self.restore_fields(window, cx);
        self.restore_choices(window, cx);
        self.provider_dialog_focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn close_provider_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.data.busy || self.data.settings_pending {
            return;
        }
        self.provider_dialog_open = false;
        self.provider_commit_pending = false;
        self.provider_removal = None;
        self.model_editor = None;
        self.host.clear_draft();
        self.refresh(window, cx);
        self.restore_fields(window, cx);
        if let Some(focus) = self.provider_return_focus.take() {
            focus.focus(window, cx);
        }
        cx.notify();
    }

    fn cancel_provider_removal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.provider_removal = None;
        self.provider_remove_button_focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn provider_removal_dialog(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let name = self
            .data
            .providers
            .iter()
            .find(|provider| Some(&provider.id) == self.provider_removal.as_ref())
            .map_or(self.data.draft.name.as_str(), |provider| {
                provider.name.as_str()
            });
        let title = rust_i18n::t!("settings.remove_confirmation", name = name).to_string();
        let popup = ui::dialog_popup(cx)
            .child(ui::dialog_title(title, cx))
            .child(ui::dialog_description(text("remove_confirmation_hint"), cx))
            .child(
                row()
                    .justify_end()
                    .child(
                        ui::dialog_button(
                            "cancel-provider-removal",
                            text("cancel"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.cancel_provider_removal(window, cx);
                        })),
                    )
                    .child(
                        ui::dialog_button(
                            "confirm-provider-removal",
                            text("confirm_remove"),
                            ButtonVariant::Danger,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            let confirmed = this.provider_removal.take();
                            this.provider_dialog_focus.focus(window, cx);
                            if confirmed.as_deref() == Some(this.data.draft.id.as_str()) {
                                this.perform("remove", window, cx);
                            }
                            cx.notify();
                        })),
                    ),
            );
        let cancel =
            cx.listener(|this, _: &bool, window, cx| this.cancel_provider_removal(window, cx));
        ui::alert_dialog(&self.provider_removal_focus, cx)
            .on_ok(|_, _, _| false)
            .popup(div().id("provider-removal-surface").occlude().child(popup))
            .request_close(move |confirmed, window, cx| cancel(&confirmed, window, cx))
    }

    #[allow(clippy::too_many_lines)] // Modal header, scrollable form and fixed actions.
    pub(super) fn provider_dialog(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let editing = !self.data.draft.preset.is_empty();
        let family = provider_x_providers::presets()
            .into_iter()
            .find(|p| p.id == self.data.draft.preset);
        let title = family.as_ref().map_or_else(
            || text("add_provider"),
            |family| {
                format!(
                    "{} · {}",
                    text(if self.data.draft.id.is_empty() {
                        "new_connection"
                    } else {
                        "edit_connection"
                    }),
                    family.name
                )
            },
        );
        let blocked = self.data.busy || self.data.settings_pending;
        let header = row()
            .child(div().flex_1().child(ui::dialog_title(title, cx)))
            .when(editing && self.data.draft.id.is_empty(), |header| {
                header.child(
                    ui::button(
                        "choose-provider",
                        text("choose_provider"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .disabled(blocked)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.host.clear_draft();
                        this.refresh(window, cx);
                        this.model_editor = None;
                        this.provider_scroll.set_offset(Point::default());
                        this.provider_dialog_focus.focus(window, cx);
                    })),
                )
            })
            .child(
                ui::button("close-provider-dialog", "", ButtonVariant::Secondary, cx)
                    .size(px(28.))
                    .p_0()
                    .accessibility_label(text("close_dialog"))
                    .disabled(blocked)
                    .child(ui::icon(ui::IconName::Close).size(px(16.)))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.close_provider_dialog(window, cx)),
                    ),
            );
        let body = if editing {
            self.provider_editor(window, cx)
        } else {
            self.provider_presets(window, cx)
        };
        let mut popup = ui::dialog_popup(cx)
            .w(px(Self::provider_dialog_width(window)))
            .h(px(
                (f32::from(window.viewport_size().height) - 96.).min(680.)
            ))
            .p_0()
            .gap_0()
            .child(header.px_6().py_4().flex_shrink_0())
            .child(
                div()
                    .id("provider-dialog-scroll")
                    .track_scroll(&self.provider_scroll)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .pb_6()
                    .child(body),
            );
        if editing {
            let claude = family
                .as_ref()
                .and_then(|p| {
                    p.connections
                        .iter()
                        .find(|c| c.id == self.data.draft.connection)
                })
                .is_some_and(|c| c.credentials == CredentialKind::ClaudeCode);
            let mut actions = row();
            if !self.data.draft.id.is_empty() {
                actions = actions.child(
                    ui::button("remove", text("remove"), ButtonVariant::Danger, cx)
                        .track_focus(&self.provider_remove_button_focus)
                        .disabled(blocked)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.provider_removal = Some(this.data.draft.id.clone());
                            this.provider_removal_focus.focus(window, cx);
                            cx.notify();
                        })),
                );
            }
            actions = actions.child(div().flex_1());
            if !claude {
                actions = actions.child(self.action_button("test", "test", "test", cx));
            }
            popup = popup.child(
                actions
                    .child(self.action_button("save", "save", "save", cx))
                    .px_6()
                    .py_3()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(cx.omarchy().border),
            );
        }
        let close =
            cx.listener(|this, _: &bool, window, cx| this.close_provider_dialog(window, cx));
        ui::dialog(&self.provider_dialog_focus, cx)
            .close_on_backdrop_press(false)
            .on_ok(|_, _, _| false)
            .popup(div().id("provider-dialog-surface").occlude().child(popup))
            .request_close(move |confirmed, window, cx| close(&confirmed, window, cx))
    }

    #[allow(clippy::too_many_lines)] // Declarative provider directory layout.
    pub(super) fn providers(&self, cx: &mut Context<Self>) -> Div {
        let mut content = column().child(
            row()
                .child(heading("providers", "configured_intro", cx).flex_1())
                .child(
                    ui::button(
                        "add-provider",
                        text("add_provider"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .disabled(self.data.busy || self.data.settings_pending)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.host.clear_draft();
                        this.refresh(window, cx);
                        this.search.update(cx, |s, cx| s.set_value("", window, cx));
                        this.open_provider_dialog(window, cx);
                    })),
                ),
        );
        let theme = cx.omarchy();
        let mut configured = column().gap_0();
        for (index, provider) in self.data.providers.iter().enumerate() {
            let id = provider.id.clone();
            let status = text(if provider.enabled {
                "enabled"
            } else {
                "disabled"
            });
            let connection = if provider.auth_label == provider.connection_label {
                text(provider.auth_label)
            } else {
                format!(
                    "{} · {}",
                    text(provider.auth_label),
                    text(provider.connection_label)
                )
            };
            let models = if provider.model_preview.is_empty() {
                text("no_enabled_models")
            } else {
                provider.model_preview.clone()
            };
            let model_count = format!(
                "{} / {}",
                provider.enabled_model_count, provider.model_count
            );
            let status_color = if provider.enabled {
                theme.accent
            } else {
                theme.secondary.opacity(0.65)
            };
            if index > 0 {
                configured = configured.child(ui::separator(cx));
            }
            configured = configured.child(
                ui::button(
                    SharedString::from(format!("open-{id}")),
                    "",
                    ButtonVariant::Secondary,
                    cx,
                )
                .accessibility_label(format!(
                    "{} · {id} · {connection} · {} {model_count} · {status}",
                    provider.name,
                    text("enabled_models")
                ))
                .w_full()
                .min_h(px(64.))
                .px_2()
                .py_2()
                .gap_4()
                .hover(|style| style.bg(theme.hover_fill()))
                .disabled(self.data.busy || self.data.settings_pending)
                .child(self.provider_icons.render(&provider.preset, cx))
                .child(
                    column()
                        .gap_1()
                        .flex_1()
                        .overflow_hidden()
                        .child(
                            row()
                                .gap_2()
                                .child(
                                    div()
                                        .min_w_0()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .truncate()
                                        .child(provider.name.clone()),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .px_1p5()
                                        .py_0p5()
                                        .text_size(px(11.))
                                        .line_height(px(14.))
                                        .bg(theme.normal_fill())
                                        .text_color(theme.secondary)
                                        .child(connection),
                                ),
                        )
                        .child(
                            muted(format!("{id} · {models}"), cx)
                                .text_xs()
                                .text_color(theme.foreground.opacity(0.65))
                                .truncate(),
                        ),
                )
                .child(
                    column()
                        .gap_1()
                        .w(px(96.))
                        .flex_shrink_0()
                        .items_end()
                        .child(div().text_sm().child(model_count))
                        .child(
                            muted(text("enabled_models"), cx)
                                .text_size(px(11.))
                                .whitespace_nowrap(),
                        ),
                )
                .child(
                    row()
                        .gap_2()
                        .w(px(68.))
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(status_color)
                        .child(div().size(px(6.)).rounded_full().bg(status_color))
                        .child(status),
                )
                .child(
                    ui::icon(ui::IconName::ChevronRight)
                        .size(px(16.))
                        .text_color(theme.secondary)
                        .flex_shrink_0(),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Err(error) = this.host.open(&id, &this.services) {
                        this.host.set_message(error.to_string());
                        this.refresh(window, cx);
                        return;
                    }
                    this.refresh(window, cx);
                    this.open_provider_dialog(window, cx);
                })),
            );
        }
        content = content.child(configured);
        if self.data.providers.is_empty() {
            content = content.child(
                column()
                    .gap_2()
                    .py_12()
                    .items_center()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(text("no_connections")),
                    )
                    .child(muted(text("add_provider_hint"), cx)),
            );
        }
        content
    }

    fn provider_presets(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let mut content = column().child(muted(text("intro"), cx));
        content = content.child(
            row()
                .child(div().flex_1().child(text("add_connection")))
                .child(
                    div()
                        .w(px(260.))
                        .child(ui::input("search", &self.search, window, cx)),
                ),
        );
        let query = self.search.read(cx).value().to_lowercase();
        let mut grid = row().flex_wrap();
        for family in provider_x_providers::presets().into_iter().filter(|p| {
            format!("{} {}", p.id, p.name)
                .to_lowercase()
                .contains(&query)
        }) {
            let connection = family.connections[0].id;
            let description = if family.connections.iter().any(|c| c.mode == "subscription") {
                "api_and_subscription"
            } else {
                "api_connection"
            };
            grid = grid.child(
                ui::button(family.id, "", ButtonVariant::Outline, cx)
                    .accessibility_label(family.name)
                    .disabled(self.data.busy)
                    .w(px(
                        ((Self::provider_dialog_width(window) - 76.) / 3.).floor()
                    ))
                    .min_h(px(82.))
                    .justify_start()
                    .child(self.provider_icons.render(family.id, cx))
                    .child(
                        column()
                            .gap_2()
                            .child(div().font_weight(FontWeight::BOLD).child(
                                if family.id == "custom" {
                                    text("custom")
                                } else {
                                    family.name.into()
                                },
                            ))
                            .child(muted(text(description), cx)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.choose(family.id, connection, window, cx);
                    })),
            );
        }
        content.child(grid)
    }
    #[allow(clippy::too_many_lines)] // Declarative connection form.
    fn provider_editor(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let d = &self.data.draft;
        let Some(family) = provider_x_providers::presets()
            .into_iter()
            .find(|f| f.id == d.preset)
        else {
            return column();
        };
        let Some(connection) = family.connections.iter().find(|c| c.id == d.connection) else {
            return column();
        };
        let mut content = column();
        let mut account =
            ui::panel(text("connection_group"), cx).child(self.field("name", "name", window, cx));
        if family.connections.len() > 1 {
            let target = cx.entity().downgrade();
            let connections: Vec<_> = family.connections.iter().map(|c| c.id).collect();
            let group = ui::button_group(
                "connection",
                family
                    .connections
                    .iter()
                    .map(|c| ui::ChoiceItem::new(c.id, text(c.label)).disabled(self.data.busy))
                    .collect(),
                connections.iter().position(|id| *id == d.connection),
                move |index, window, cx| {
                    if let Some(connection) = connections.get(index) {
                        let _ = target.update(cx, |this, cx| {
                            if !this.data.busy {
                                this.choose(family.id, connection, window, cx);
                            }
                        });
                    }
                },
                window,
                cx,
            )
            .aria_label(text("connection_type"));
            account = account.child(setting(
                "connection_type",
                "",
                div().w(px(260.)).child(group),
                cx,
            ));
        }
        if connection.mode == "subscription" {
            let button = self.action_button(
                "login",
                if connection.credentials == CredentialKind::ClaudeCode {
                    "check_cli"
                } else if self.data.has_credential {
                    "signed_in"
                } else {
                    "sign_in"
                },
                "login",
                cx,
            );
            let mut actions = row().child(button);
            let login_pending = self.data.operation_kind == OperationKind::Login;
            if login_pending && self.data.busy && self.data.cancellable {
                actions = actions.child(self.action_button("cancel-login", "cancel", "cancel", cx));
            }
            account = account.child(setting(
                "account",
                if connection.credentials == CredentialKind::ClaudeCode {
                    "claude_dependency"
                } else {
                    "subscription_hint"
                },
                actions,
                cx,
            ));
        } else if connection.credentials == CredentialKind::ApiKey {
            account = account.child(self.field("key", "api_key", window, cx));
        }
        if !d.id.is_empty() {
            let enabled = self
                .data
                .providers
                .iter()
                .any(|p| p.id == d.id && p.enabled);
            let switch = self.action_switch(
                "enabled",
                "enable_provider",
                enabled,
                ("provider-enable", "provider-disable"),
                cx,
            );
            account = account.child(setting(
                "enable_provider",
                "provider_enable_hint",
                switch,
                cx,
            ));
        }
        content = content
            .child(account)
            .child(self.models(connection.credentials, window, cx));
        if connection.mode != "subscription" {
            let advanced = ui::button("advanced", text("advanced"), ButtonVariant::Secondary, cx)
                .selected(self.advanced)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.advanced = !this.advanced;
                    cx.notify();
                }));
            let mut panel = column().child(advanced);
            if self.advanced {
                let protocol = self.picker("protocol", window, cx);
                panel = panel
                    .child(self.field("endpoint", "endpoint", window, cx))
                    .child(setting("protocol", "", protocol, cx))
                    .child(self.field("model_endpoint", "model_endpoint", window, cx));
                if d.protocol == ProtocolId::OpenaiResponses {
                    let switch = ui::switch("websocket", "", d.websocket, cx)
                        .accessibility_label(text("websocket"))
                        .disabled(self.data.busy)
                        .on_change(toggled(cx, |this, value, _, cx| {
                            this.data.draft.websocket = value;
                            this.edit();
                            cx.notify();
                        }));
                    panel = panel.child(setting("websocket", "websocket_hint", switch, cx));
                    if d.websocket {
                        panel = panel.child(self.field(
                            "websocket_endpoint",
                            "websocket_endpoint",
                            window,
                            cx,
                        ));
                    }
                }
            }
            content = content.child(panel);
        }
        content
    }
    #[allow(clippy::too_many_lines)] // Model list and inline editor share the draft.
    fn models(
        &self,
        credentials: CredentialKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut panel = ui::panel(text("models"), cx);
        let mut tools = row().child(muted(text("models_hint"), cx).flex_1());
        if credentials != CredentialKind::ClaudeCode {
            tools = tools.child(self.action_button("discover", "discover", "discover", cx));
        }
        let add = ui::button("add-model", text("add_model"), ButtonVariant::Outline, cx)
            .disabled(self.data.busy)
            .on_click(cx.listener(|this, _, window, cx| {
                let id = this.fields["model"].read(cx).value().trim().to_owned();
                if !id.is_empty() && !this.data.draft.models.iter().any(|m| m.id == id) {
                    this.data.draft.models.push(ModelDraft {
                        id: id.clone(),
                        name: id,
                        enabled: true,
                        ..ModelDraft::default()
                    });
                    this.edit();
                    this.fields["model"].update(cx, |s, cx| s.set_value("", window, cx));
                    cx.notify();
                }
            }));
        panel = panel.child(tools).child(
            row()
                .child(
                    div()
                        .flex_1()
                        .child(ui::input("model", &self.fields["model"], window, cx)),
                )
                .child(add),
        );
        if self.data.draft.models.len() > 10 {
            panel = panel.child(ui::input("model-search", &self.model_search, window, cx));
        }
        let query = self.model_search.read(cx).value().to_lowercase();
        let models: Vec<_> = self
            .data
            .draft
            .models
            .iter()
            .filter(|m| {
                format!("{} {}", m.id, m.name)
                    .to_lowercase()
                    .contains(&query)
            })
            .collect();
        let pages = models.len().div_ceil(30).max(1);
        let page = self.model_page.min(pages - 1);
        if pages > 1 {
            panel = panel.child(
                row()
                    .child(
                        ui::button(
                            "previous",
                            text("previous_page"),
                            ButtonVariant::Outline,
                            cx,
                        )
                        .disabled(page == 0)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model_page = this.model_page.saturating_sub(1);
                            cx.notify();
                        })),
                    )
                    .child(format!("{} / {pages}", page + 1))
                    .child(
                        ui::button("next", text("next_page"), ButtonVariant::Outline, cx)
                            .disabled(page + 1 >= pages)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model_page += 1;
                                cx.notify();
                            })),
                    ),
            );
        }
        for model in models.into_iter().skip(page * 30).take(30) {
            let id = model.id.clone();
            let edit_id = id.clone();
            let remove_id = id.clone();
            let switch = ui::switch(
                SharedString::from(format!("model-{id}")),
                "",
                model.enabled,
                cx,
            )
            .accessibility_label(model.name.clone())
            .disabled(self.data.busy)
            .on_change(toggled(cx, move |this, value, _, cx| {
                if let Some(m) = this.data.draft.models.iter_mut().find(|m| m.id == id) {
                    m.enabled = value;
                }
                this.edit();
                cx.notify();
            }));
            let edit = ui::button(
                SharedString::from(format!("edit-{edit_id}")),
                text("model_settings"),
                ButtonVariant::Outline,
                cx,
            )
            .disabled(self.data.busy)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.model_editor = Some(edit_id.clone());
                this.restore_fields(window, cx);
                this.restore_choices(window, cx);
                cx.notify();
            }));
            let remove = ui::button(
                SharedString::from(format!("remove-{remove_id}")),
                text("remove_model"),
                ButtonVariant::Secondary,
                cx,
            )
            .disabled(self.data.busy)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.data.draft.models.retain(|m| m.id != remove_id);
                this.model_editor = None;
                this.edit();
                cx.notify();
            }));
            panel = panel.child(ui::separator(cx)).child(
                row()
                    .child(
                        column()
                            .gap_1()
                            .flex_1()
                            .child(model.name.clone())
                            .child(muted(model.id.clone(), cx)),
                    )
                    .child(switch)
                    .child(edit)
                    .child(remove),
            );
            if self.model_editor.as_ref() == Some(&model.id) {
                let parallel = self.picker("parallel_tools", window, cx);
                let search = self.picker("search_tool", window, cx);
                let choices = self.reasoning_menu(model, cx);
                panel = panel.child(
                    column()
                        .child(self.field("model_name", "model_name", window, cx))
                        .child(self.field("context_window", "model_context_window", window, cx))
                        .child(setting("model_reasoning_levels", "", choices, cx))
                        .child(setting("parallel_tools", "", parallel, cx))
                        .child(setting("search_tool", "", search, cx))
                        .child(
                            ui::button("done", text("done"), ButtonVariant::Outline, cx).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.model_editor = None;
                                    cx.notify();
                                }),
                            ),
                        ),
                );
            }
        }
        panel
    }
    fn reasoning_menu(
        &self,
        model: &ModelDraft,
        cx: &mut Context<Self>,
    ) -> gpui_kit::base::Popover {
        let label = if model.reasoning_levels.is_empty() {
            text("automatic")
        } else {
            model.reasoning_levels.join(", ")
        };
        let trigger = ui::button("reasoning-trigger", label, ButtonVariant::Outline, cx)
            .disabled(self.data.busy)
            .accessibility_label(text("model_reasoning_levels"));
        let owner = cx.entity().downgrade();
        let id = model.id.clone();
        ui::popover("reasoning-levels", trigger, move |_, _, cx| {
            let Some(owner) = owner.upgrade() else {
                return column();
            };
            let Some(model) = owner
                .read(cx)
                .data
                .draft
                .models
                .iter()
                .find(|m| m.id == id)
                .cloned()
            else {
                return column();
            };
            let mut values = vec![
                "none".into(),
                "minimal".into(),
                "low".into(),
                "medium".into(),
                "high".into(),
                "xhigh".into(),
            ];
            for value in &model.reasoning_levels {
                if !values.contains(value) {
                    values.push(value.clone());
                }
            }
            let mut menu = column().gap_1();
            for value in values {
                let checked = model.reasoning_levels.contains(&value);
                let target = owner.clone();
                let id = id.clone();
                menu = menu.child(
                    ui::checkbox(
                        SharedString::from(format!("reasoning-{value}")),
                        value.clone(),
                        if checked {
                            CheckboxState::Checked
                        } else {
                            CheckboxState::Unchecked
                        },
                        cx,
                    )
                    .on_change(move |state, _, _, cx| {
                        target.update(cx, |this, cx| {
                            if this.data.busy {
                                return;
                            }
                            if let Some(model) =
                                this.data.draft.models.iter_mut().find(|m| m.id == id)
                            {
                                model.reasoning_levels.retain(|v| v != &value);
                                if state == CheckboxState::Checked {
                                    model.reasoning_levels.push(value.clone());
                                }
                                model.metadata_sources.insert(
                                    "supported_reasoning_levels".into(),
                                    MetadataSource::UserConfirmed,
                                );
                            }
                            this.edit();
                            cx.notify();
                        });
                    }),
                );
            }
            menu
        })
    }
}

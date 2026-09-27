use gpui_kit::StatefulInteractiveElement;

use super::{
    ButtonVariant, CheckboxState, Context, CredentialKind, Div, FontWeight, MetadataSource,
    ModelDraft, OperationKind, ParentElement, Point, ProtocolId, Settings, SharedString, Styled,
    Window, column, div, heading, muted, px, row, setting, text, toggled, ui,
};

impl Settings {
    #[allow(clippy::too_many_lines)] // Declarative provider directory layout.
    pub(super) fn providers(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        if !self.data.draft.preset.is_empty() {
            return self.provider_editor(window, cx);
        }
        let mut content = column().child(heading("providers", "intro", cx));
        if !self.data.providers.is_empty() {
            let mut configured = ui::panel(text("your_connections"), cx);
            for provider in &self.data.providers {
                let id = provider.id.clone();
                configured = configured.child(
                    row()
                        .child(
                            column()
                                .gap_1()
                                .flex_1()
                                .child(provider.name.clone())
                                .child(muted(
                                    format!(
                                        "{} · {}",
                                        id,
                                        text(if provider.enabled {
                                            "enabled"
                                        } else {
                                            "disabled"
                                        })
                                    ),
                                    cx,
                                )),
                        )
                        .child(
                            ui::button(
                                SharedString::from(format!("open-{id}")),
                                text("manage"),
                                ButtonVariant::Outline,
                                cx,
                            )
                            .disabled(self.data.busy)
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if let Err(error) = this.host.open(&id, &this.services) {
                                        this.host.set_message(error.to_string());
                                    }
                                    this.refresh(window, cx);
                                    this.model_editor = None;
                                    this.restore_fields(window, cx);
                                    this.restore_choices(window, cx);
                                },
                            )),
                        ),
                );
            }
            content = content.child(configured);
        }
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
                        ((f32::from(window.viewport_size().width) - 300.) / 3.).floor()
                    ))
                    .min_h(px(82.))
                    .justify_start()
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
        let back = ui::button("back", text("back"), ButtonVariant::Outline, cx)
            .disabled(self.data.busy)
            .on_click(cx.listener(|this, _, _, cx| {
                this.data.draft.preset.clear();
                this.edit();
                this.model_editor = None;
                this.scroll.set_offset(Point::default());
                cx.notify();
            }));
        let mut content =
            column()
                .child(row().child(back).child(muted(
                    text(if d.id.is_empty() {
                        "new_connection"
                    } else {
                        "edit_connection"
                    }),
                    cx,
                )))
                .child(div().text_xl().font_weight(FontWeight::BOLD).child(
                    if family.id == "custom" {
                        text("custom")
                    } else {
                        family.name.to_owned()
                    },
                ));
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
        if !d.id.is_empty() {
            content = content.child(self.action_button("remove", "remove", "remove", cx));
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

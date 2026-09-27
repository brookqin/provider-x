import { View, div } from "gpui-kit";
import { InputState, h_flex, v_flex, set_theme } from "gpui-base";
import { snapshot, select, edit, secret, action, text, open } from "providerx";
import {
  AppShell, Button, Title, MutedText, Label, FormField, TextField, Tabs,
  Alert, AccordionSection, applyOmarchyRoles, applyOmarchyStyle, omarchyTheme,
} from "./vendor/omarchy-ui/src/index.js";

const t = key => text(`shell.${key}`);
const COLORS = `mode = "dark"
background = "#1a1b26"
foreground = "#c0caf5"
accent = "#7aa2f7"
red = "#f7768e"
green = "#9ece6a"
yellow = "#e0af68"
blue = "#7aa2f7"
magenta = "#bb9af7"
cyan = "#7dcfff"`;

const LIGHT = `mode = "light"
background = "#fffcf0"
foreground = "#100f0f"
accent = "#205ea6"
red = "#af3029"
green = "#66800b"
yellow = "#ad8301"
blue = "#205ea6"
magenta = "#a02f6f"
cyan = "#24837b"`;

export default class Settings extends View {
  init(_props, cx) {
    this.tokens = applyOmarchyStyle("[font]\nbase-size = 13\n[spacing]\nscale = 1\nscale-with-font = true", {
      fontFamily: "Menlo", platform: "darwin",
    });
    this.data = JSON.parse(snapshot());
    this.applyTheme(cx);
    this.page = "providers";
    this.advanced = false;
    this.modelEditor = null;
    this.modelFields = {};
    this.modelPage = 0;
    this.restoreSearchInputs();
    this.restoreInputs();
    // Theme updates need the live host scope supplied by a Shell-owned task.
    cx.timer.every(400, context => this.refresh(context));
  }

  applyTheme(cx) {
    const colors = this.data.theme === "light" ? LIGHT : COLORS;
    applyOmarchyRoles(colors);
    set_theme(omarchyTheme(colors, cx.theme(), this.tokens));
  }

  refresh(cx) {
    const next = JSON.parse(snapshot());
    if (next.revision === this.data.revision && next.router_ready === this.data.router_ready) return;
    const themeChanged = next.theme !== this.data.theme;
    const localeChanged = next.locale !== this.data.locale;
    this.data = next;
    if (localeChanged) { this.restoreInputs(); this.restoreSearchInputs(); }
    if (themeChanged) this.applyTheme(cx);
    cx.notify();
  }

  restoreSearchInputs() {
    const search = this.search?.value() || "";
    const modelSearch = this.modelSearch?.value() || "";
    this.search?.release();
    this.modelSearch?.release();
    this.search = InputState.new({ placeholder: t("search"), value: search });
    this.search.on("change", (_event, context) => context.notify());
    this.modelSearch = InputState.new({ placeholder: t("model_search"), value: modelSearch });
    this.modelSearch.on("change", (_event, context) => { this.modelPage = 0; context.notify(); });
  }

  restoreInputs() {
    this.modelEditor = null;
    for (const state of Object.values(this.modelFields || {})) state.release();
    this.modelFields = {};
    for (const state of Object.values(this.fields || {})) state.release();
    this.fields = {};
    for (const key of ["name", "endpoint", "model_endpoint", "websocket_endpoint"]) {
      const state = InputState.new({ value: this.data.draft[key] || "" });
      state.on("change", () => {
        this.data.draft[key] = state.value();
        edit(JSON.stringify(this.data.draft));
      });
      this.fields[key] = state;
    }
    this.fields.key = InputState.new({ placeholder: this.data.has_credential ? t("credential_saved") : t("api_key") });
    this.fields.key.set_masked(true);
    this.fields.key.on("change", () => secret(this.fields.key.value()));
    this.fields.model = InputState.new({ placeholder: t("model_placeholder") });
  }

  editModel(model, cx) {
    for (const state of Object.values(this.modelFields)) state.release();
    this.modelFields = {};
    this.modelEditor = model.id;
    for (const field of ["name", "context_window", "reasoning_levels"]) {
      const value = field === "reasoning_levels" ? (model[field] || []).join(", ") : String(model[field] || "");
      const input = InputState.new({ value });
      input.on("change", () => {
        const current = this.data.draft.models.find(item => item.id === model.id);
        const value = input.value().trim();
        current[field] = field === "context_window" ? (/^[1-9][0-9]*$/.test(value) && Number.isSafeInteger(Number(value)) ? Number(value) : null)
          : field === "reasoning_levels" ? value.split(",").map(item => item.trim()).filter(Boolean) : value;
        current.metadata_sources ||= {};
        current.metadata_sources[field === "name" ? "display_name" : field === "reasoning_levels" ? "supported_reasoning_levels" : field] = "user_confirmed";
        edit(JSON.stringify(this.data.draft));
      });
      this.modelFields[field] = input;
    }
    cx.notify();
  }

  choose(family, connection, cx) {
    if (this.data.busy) return;
    this.modelPage = 0;
    select(family.id, connection.id);
    this.data = JSON.parse(snapshot());
    this.restoreInputs();
    cx.notify();
  }

  button(id, label, callback, cx, primary = false) {
    return new Button(id).label(label).accent(primary).disabled(this.data.busy)
      .onClick((_event, context) => callback(context)).build(cx);
  }

  perform(name, cx) {
    action(name);
    this.refresh(cx);
  }

  render(cx) {
    const colors = cx.theme().colors;
    const nav = v_flex().w(196).h_full().p(20).gap(10).bg(colors.surface)
      .child(new Title("ProviderX").build(cx))
      .child(new MutedText(t("tagline")).build(cx))
      .child(div().h(24));
    for (const page of ["providers", "integration", "general"]) {
      nav.child(new Button(`nav-${page}`).label(t(page)).selected(this.page === page)
        .onClick((_event, context) => { this.page = page; if (page === "integration") this.perform("integration-status", context); context.notify(); }).build(cx));
    }
    nav.child(div().flex_1()).child(new MutedText(t(this.data.router_ready ? "router_running" : "router_failed")).build(cx));
    const content = v_flex().id("settings-content").flex_1().min_h(0).min_w(0).p(30).gap(20).overflow_y_scroll();
    if (this.page === "providers") this.providers(content, cx);
    if (this.page === "integration") {
      content.child(new Title(t("integration")).build(cx))
        .child(new MutedText(t("integration_hint")).build(cx))
        .child(new Alert("integration-status").message(t(`integration_${this.data.integration || "unknown"}`)).tone(this.data.integration === "changed" ? "danger" : "neutral").build(cx))
        .child(this.button("integration-enable", t("enable_integration"), context => this.perform("integration-enable", context), cx))
        .child(this.button("integration-disable", t("disable_integration"), context => this.perform("integration-disable", context), cx));
    }
    if (this.page === "general") {
      content.child(new Title(t("general")).build(cx))
        .child(new MutedText(t("general_hint")).build(cx))
        .child(new Tabs("language").items([{value:"en",label:"English"},{value:"zh-CN",label:"简体中文"}]).value(this.data.locale)
          .onChange((value, context) => this.perform(`locale-${value}`, context)).build(cx))
        .child(new Label(t("appearance")).build(cx))
        .child(new Tabs("appearance").items([{value:"dark",label:t("dark")},{value:"light",label:t("light")}]).value(this.data.theme || "dark")
          .onChange((value, context) => this.perform(`theme-${value}`, context)).build(cx))
        .child(new Label(t("launch_at_login")).build(cx))
        .child(this.button("startup", t(this.data.startup === "Disabled" ? "enable" : "disable"), context => this.perform(this.data.startup === "Disabled" ? "startup-enable" : "startup-disable", context), cx));
    }
    const pane = v_flex().flex_1().min_w(0).h_full().child(content);
    if ((this.page === "providers" && this.data.draft.preset) || this.data.message || this.data.busy) {
      const footer = v_flex().p(20).gap(10).bg(colors.surface);
      if (this.page === "providers" && this.data.draft.preset) {
        const actions = h_flex().gap(12).child(this.button("save-provider", t("save"), context => this.perform("save", context), cx, true));
        const family = this.data.presets.find(item => item.id === this.data.draft.preset);
        const connection = family.connections.find(item => item.id === this.data.draft.connection);
        if (connection.credentials !== "claude_code") actions.child(this.button("test-provider", t("test"), context => this.perform("test", context), cx));
        footer.child(actions);
      }
      if (this.data.message) footer.child(new Alert("operation-message").message(this.data.message).tone(this.data.failed ? "danger" : "success").build(cx));
      if (this.data.busy) {
        const progress = h_flex().gap(16).child(new MutedText(t("working")).build(cx));
        if (this.data.cancellable) progress.child(new Button("cancel").label(t("cancel")).onClick((_event, context) => this.perform("cancel", context)).build(cx));
        footer.child(progress);
      }
      pane.child(footer);
    }
    return new AppShell().content(h_flex().size_full().items_start().child(nav).child(pane)).build(cx);
  }

  providers(content, cx) {
    const draft = this.data.draft;
    content.child(new Title(t("providers")).build(cx))
      .child(new MutedText(t("intro")).build(cx));
    if (!draft.preset) {
      for (const provider of this.data.providers) {
        content.child(this.button(`open-${provider.id}`, `${provider.name} · ${t(provider.enabled ? "enabled" : "disabled")}`, context => {
          open(provider.id); this.data = JSON.parse(snapshot()); this.restoreInputs(); context.notify();
        }, cx));
      }
      content.child(new TextField().state(this.search).build(cx));
      const query = this.search.value().trim().toLowerCase();
      const grid = h_flex().flex_wrap().gap(10);
      for (const family of this.data.presets.filter(family => `${family.name} ${family.id}`.toLowerCase().includes(query))) {
        grid.child(new Button(`preset-${family.id}`).label(family.id === "custom" ? t("custom") : family.name)
          .onClick((_event, context) => this.choose(family, family.connections[0], context))
          .build(cx).w(180).h(56));
      }
      content.child(grid);
      return;
    }
    const family = this.data.presets.find(family => family.id === draft.preset);
    const connection = family.connections.find(connection => connection.id === draft.connection);
    content.child(h_flex().gap(12)
      .child(this.button("back", t("back"), context => {
        this.data.draft = { ...draft, preset: "" };
        edit(JSON.stringify(this.data.draft)); context.notify();
      }, cx))
      .child(new Title(family.id === "custom" ? t("custom") : family.name).build(cx)));
    content.child(new FormField("provider-name").label(t("name")).control(new TextField().state(this.fields.name).build(cx).disabled(this.data.busy)).build(cx));
    if (family.connections.length > 1) {
      content.child(new Tabs("connection-mode").segmented().items(family.connections.map(item => ({ value: item.id, label: t(item.label) })))
        .value(connection.id).onChange((value, context) => this.choose(family, family.connections.find(item => item.id === value), context)).build(cx));
    }
    if (connection.mode === "subscription") {
      content.child(new MutedText(t(connection.credentials === "claude_code" ? "claude_dependency" : "subscription_hint")).build(cx))
        .child(this.button("sign-in", t(connection.credentials === "claude_code" ? "check_cli" : this.data.has_credential ? "signed_in" : "sign_in"), context => this.perform("login", context), cx));
    } else if (connection.credentials === "api_key") {
      content.child(new FormField("credential").label(t("api_key")).control(new TextField().state(this.fields.key).build(cx).disabled(this.data.busy)).build(cx));
    }
    const modelHeader = h_flex().justify_between().child(new Label(t("models")).strong().build(cx));
    if (connection.credentials !== "claude_code") modelHeader.child(this.button("discover", t("discover"), context => this.perform("discover", context), cx));
    content.child(modelHeader);
    const addModel = context => {
      const id = this.fields.model.value().trim();
      if (id && !draft.models.some(model => model.id === id)) {
        draft.models.push({ id, name: id, enabled: true, context_window: null, reasoning_levels: [], parallel_tools: null, search_tool: null });
        edit(JSON.stringify(draft)); this.fields.model.set_value(""); context.notify();
      }
    };
    content.child(h_flex().gap(8).child(new TextField().state(this.fields.model).build(cx).disabled(this.data.busy).flex_1())
      .child(this.button("add-model", t("add_model"), addModel, cx)));
    if (!draft.models.length) content.child(new MutedText(t("models_hint")).build(cx));
    if (draft.models.length > 10) content.child(new TextField().state(this.modelSearch).build(cx));
    const query = this.modelSearch.value().trim().toLowerCase();
    const filtered = draft.models.filter(model => `${model.id} ${model.name}`.toLowerCase().includes(query));
    this.modelPage = Math.min(this.modelPage, Math.max(0, Math.ceil(filtered.length / 30) - 1));
    if (filtered.length > 30) content.child(h_flex().gap(12)
      .child(this.button("models-prev", t("previous_page"), context => { this.modelPage = Math.max(0, this.modelPage - 1); context.notify(); }, cx))
      .child(new Label(`${this.modelPage + 1} / ${Math.ceil(filtered.length / 30)}`).build(cx))
      .child(this.button("models-next", t("next_page"), context => { this.modelPage = Math.min(Math.ceil(filtered.length / 30) - 1, this.modelPage + 1); context.notify(); }, cx)));
    for (const model of filtered.slice(this.modelPage * 30, (this.modelPage + 1) * 30)) {
      content.child(h_flex().gap(12).child(new Label(model.name).build(cx)).child(div().flex_1())
        .child(this.button(`model-${model.id}`, t(model.enabled ? "enabled" : "disabled"), context => {
          model.enabled = !model.enabled; edit(JSON.stringify(draft)); context.notify();
        }, cx))
        .child(this.button(`edit-${model.id}`, t("model_settings"), context => this.editModel(model, context), cx))
        .child(this.button(`remove-${model.id}`, t("remove_model"), context => { draft.models = draft.models.filter(item => item.id !== model.id); edit(JSON.stringify(draft)); this.modelEditor = null; context.notify(); }, cx)));
      if (this.modelEditor === model.id) {
        const editor = v_flex().p(12).gap(10);
        for (const key of ["name", "context_window", "reasoning_levels"]) {
          editor.child(new FormField(`model-${key}`).label(t(`model_${key}`)).control(new TextField().state(this.modelFields[key]).build(cx).disabled(this.data.busy)).build(cx));
        }
        for (const key of ["parallel_tools", "search_tool"]) {
          editor.child(this.button(`model-${key}`, `${t(key)} · ${t(model[key] === true ? "enabled" : model[key] === false ? "disabled" : "automatic")}`, context => {
            model[key] = model[key] == null ? true : model[key] ? false : null; model.metadata_sources ||= {}; model.metadata_sources[key === "parallel_tools" ? "supports_parallel_tool_calls" : "supports_search_tool"] = "user_confirmed"; edit(JSON.stringify(draft)); context.notify();
          }, cx));
        }
        content.child(editor);
      }
    }
    const advanced = v_flex().gap(12)
      .child(new FormField("endpoint").label(t("endpoint")).control(new TextField().state(this.fields.endpoint).build(cx).disabled(this.data.busy)).build(cx))
      .child(new Tabs("protocol").items([
        {value: "openai_responses", label: "Responses"},
        {value: "openai_chat_completions", label: "Chat Completions"},
        {value: "anthropic_messages", label: "Messages"},
      ]).value(draft.protocol).onChange((value, context) => { if (this.data.busy) return; draft.protocol = value; edit(JSON.stringify(draft)); context.notify(); }).build(cx));
    advanced.child(new FormField("model-endpoint").label(t("model_endpoint")).control(new TextField().state(this.fields.model_endpoint).build(cx).disabled(this.data.busy)).build(cx));
    if (draft.protocol === "openai_responses") {
      advanced.child(this.button("native-websocket", `${t("websocket")} · ${t(draft.websocket ? "enabled" : "disabled")}`, context => { draft.websocket = !draft.websocket; edit(JSON.stringify(draft)); context.notify(); }, cx));
      if (draft.websocket) advanced.child(new FormField("websocket-endpoint").label(t("websocket_endpoint")).control(new TextField().state(this.fields.websocket_endpoint).build(cx).disabled(this.data.busy)).build(cx));
    }
    if (connection.mode !== "subscription") content.child(new AccordionSection("advanced").title(t("advanced")).open(this.advanced).body(advanced)
      .onToggle((value, context) => { this.advanced = value; context.notify(); }).build(cx));
    if (draft.id) {
      content.child(h_flex().gap(12)
        .child(this.button("toggle-provider", t(this.data.providers.find(p => p.id === draft.id)?.enabled ? "disable_provider" : "enable_provider"), context => this.perform("toggle", context), cx))
        .child(this.button("remove-provider", t("remove"), context => this.perform("remove", context), cx)));
    }

  }
}

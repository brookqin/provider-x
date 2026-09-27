import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { SourceTextModule, SyntheticModule } from "node:vm";

// Exercise the shipping controller with both possible host completion timings.
// Native rendering and the Shell task scope are additionally checked in the app.
async function fixture() {
  let state = { revision: 0, theme: "dark", locale: "en", router_ready: true, busy: false, draft: {} };
  let immediate = false;
  const themes = [];
  const tasks = [];
  const input = (options = {}) => ({ on() {}, set_masked() {}, release() {},
    value: () => options.value || "", set_value(value) { options.value = value; },
  });
  const modules = {
    "gpui-kit": { View: class {}, div() {} },
    "gpui-base": {
      InputState: { new: input }, h_flex() {}, v_flex() {},
      set_theme(theme) { themes.push(theme); },
    },
    providerx: {
      snapshot: () => JSON.stringify(state), select() {}, edit() {}, secret() {}, open() {},
      text: key => key,
      action(name) {
        state = { ...state, revision: state.revision + 1, busy: !immediate };
        if (immediate) state.theme = name.slice(6);
      },
    },
    "./vendor/omarchy-ui/src/index.js": {
      ...Object.fromEntries(["AppShell", "Button", "Title", "MutedText", "Label", "FormField", "TextField", "Tabs", "Alert", "AccordionSection"].map(key => [key, class {}])),
      applyOmarchyRoles() {}, applyOmarchyStyle() { return {}; },
      omarchyTheme(palette) { return palette.includes('mode = "light"') ? "light" : "dark"; },
    },
  };
  const source = await readFile(new URL("../main.js", import.meta.url), "utf8");
  const module = new SourceTextModule(source);
  await module.link(specifier => {
    const exports = modules[specifier];
    assert.ok(exports, `unexpected dependency: ${specifier}`);
    return new SyntheticModule(Object.keys(exports), function () {
      for (const [key, value] of Object.entries(exports)) this.setExport(key, value);
    });
  });
  await module.evaluate();
  const cx = {
    theme: () => ({}), notify() {},
    timer: { every(_interval, callback) { tasks.push(callback); } },
  };
  const view = new module.namespace.default();
  view.init(undefined, cx);
  return {
    view, cx, themes, tasks,
    complete(patch) { state = { ...state, ...patch, busy: false, revision: state.revision + 1 }; },
    immediate() { immediate = true; },
    async tick() {
      assert.equal(tasks.length, 1, "one refresh timer remains alive");
      tasks[0](cx);
      await Promise.resolve();
      await Promise.resolve();
    },
  };
}

test("theme completion clears busy and later updates keep refreshing", async () => {
  const f = await fixture();
  assert.equal(f.tasks.length, 1, "refresh loop must be owned by Shell");
  f.view.perform("theme-light", f.cx);
  assert.equal(f.view.data.busy, true);
  f.complete({ theme: "light", message: "saved" });
  await f.tick();
  assert.equal(f.view.data.busy, false);
  assert.equal(f.view.data.message, "saved");
  assert.deepEqual(f.themes, ["dark", "light"]);
  f.view.perform("theme-dark", f.cx);
  f.complete({ theme: "dark" });
  await f.tick();
  assert.equal(f.view.data.busy, false);
  assert.deepEqual(f.themes, ["dark", "light", "dark"]);
  f.complete({ locale: "zh-CN" });
  await f.tick();
  assert.equal(f.view.data.locale, "zh-CN");
});

test("completion before the immediate snapshot still applies the theme", async () => {
  const f = await fixture();
  f.immediate();
  f.view.perform("theme-light", f.cx);
  assert.equal(f.view.data.busy, false);
  assert.deepEqual(f.themes, ["dark", "light"]);
  await f.tick();
  assert.deepEqual(f.themes, ["dark", "light"], "unchanged state does not reapply theme");
});

test("failed preference save releases controls and keeps the existing theme", async () => {
  const f = await fixture();
  f.view.perform("theme-light", f.cx);
  f.complete({ failed: true, message: "could not save" });
  await f.tick();
  assert.equal(f.view.data.busy, false);
  assert.equal(f.view.data.failed, true);
  assert.deepEqual(f.themes, ["dark"]);
});

test("changing language preserves search text and does not break later theme updates", async () => {
  const f = await fixture();
  f.view.search.set_value("openai");
  f.view.modelSearch.set_value("model-query");
  f.complete({ locale: "zh-CN" });
  await f.tick();
  assert.equal(f.view.search.value(), "openai");
  assert.equal(f.view.modelSearch.value(), "model-query");
  f.view.perform("theme-light", f.cx);
  f.complete({ theme: "light" });
  await f.tick();
  assert.equal(f.view.data.busy, false);
  assert.deepEqual(f.themes, ["dark", "light"]);
});

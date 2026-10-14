// Minimal W3C WebDriver client (fetch only, no dependencies) for driving the Tauri app through
// tauri-driver, which proxies to WebKitWebDriver on Linux.

const ELEMENT = "element-6066-11e4-a52e-4f735466cecf";

export class WebDriverError extends Error {
  constructor(status, value) {
    super(`${value?.error ?? status}: ${value?.message ?? ""}`.trim());
    this.status = status;
    this.value = value;
  }
}

async function call(base, method, path, body) {
  const res = await fetch(`${base}${path}`, {
    method,
    headers: { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  const json = text ? JSON.parse(text) : {};
  if (!res.ok) throw new WebDriverError(res.status, json.value);
  return json.value;
}

/** One WebDriver session. */
export class Session {
  constructor(base, id) {
    this.base = base;
    this.id = id;
  }

  static async create(base, capabilities) {
    const value = await call(base, "POST", "/session", {
      capabilities: { alwaysMatch: capabilities },
    });
    return new Session(base, value.sessionId);
  }

  cmd(method, path, body) {
    return call(this.base, method, `/session/${this.id}${path}`, body);
  }

  async delete() {
    await call(this.base, "DELETE", `/session/${this.id}`);
  }

  /** Runs `script` (a function body) in the page with `args`; returns its JSON result. */
  exec(script, ...args) {
    return this.cmd("POST", "/execute/sync", { script, args });
  }

  /** Like `exec`, but the body receives a `done` callback as its last argument. */
  execAsync(script, ...args) {
    return this.cmd("POST", "/execute/async", { script, args });
  }

  async find(using, value) {
    const v = await this.cmd("POST", "/element", { using, value });
    return v[ELEMENT];
  }

  async findAll(using, value) {
    const v = await this.cmd("POST", "/elements", { using, value });
    return v.map((e) => e[ELEMENT]);
  }

  click(el) {
    return this.cmd("POST", `/element/${el}/click`, {});
  }

  type(el, text) {
    return this.cmd("POST", `/element/${el}/value`, { text });
  }

  clear(el) {
    return this.cmd("POST", `/element/${el}/clear`, {});
  }

  text(el) {
    return this.cmd("GET", `/element/${el}/text`);
  }

  /** Keyboard input to the focused element (W3C key actions, e.g. "" = Control). */
  async keys(chords) {
    const actions = [];
    for (const k of chords) actions.push({ type: "keyDown", value: k });
    for (const k of [...chords].reverse()) actions.push({ type: "keyUp", value: k });
    await this.cmd("POST", "/actions", {
      actions: [{ type: "key", id: "keyboard", actions }],
    });
    await this.cmd("DELETE", "/actions");
  }

  screenshot() {
    return this.cmd("GET", "/screenshot");
  }

  source() {
    return this.cmd("GET", "/source");
  }
}

export const Keys = {
  Control: "",
  Meta: "",
  Shift: "",
  Enter: "",
  Escape: "",
  Tab: "",
};

import { listen } from "@tauri-apps/api/event";
import type { IpcAdapter } from "./adapter";
import { commands } from "./bindings";

export const tauriAdapter: IpcAdapter = {
  kind: "tauri",
  commands,
  async listen(name, handler) {
    // Payloads are validated/typed by the consumers (event bus) per event name.
    return listen<unknown>(name, (e) => handler(e.payload));
  },
};

// Electron main process for the komo desktop shell.
//
// Since the shared renderer (@komo/app) now runs the same HttpKomoClient as the
// web build, main no longer proxies HTTP: its only job is gateway *discovery*.
// It reads ~/.komo/gateway.json and hands the renderer {base, key} over a
// single IPC channel (`komo:gateway`), re-read on every call so a gateway
// restart (new port/key) is picked up. All /api and /v1 traffic then goes
// straight from the renderer to the loopback gateway.

import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { app, BrowserWindow, dialog, ipcMain, shell } from "electron";

interface Gateway {
  base: string;
  key: string;
}

/** Resolve ~/.komo, honoring KOMO_HOME. */
function komoHome(): string {
  const env = process.env.KOMO_HOME;
  if (env && env.length > 0) return env;
  return path.join(os.homedir(), ".komo");
}

/** Read the gateway rendezvous file, or null if absent/unparseable. */
function readGateway(): Gateway | null {
  try {
    const raw = fs.readFileSync(path.join(komoHome(), "gateway.json"), "utf8");
    const info = JSON.parse(raw);
    const host = info.bind === "0.0.0.0" ? "127.0.0.1" : info.bind;
    return { base: `http://${host}:${info.port}`, key: String(info.key) };
  } catch {
    return null;
  }
}

function registerIpc(): void {
  // The renderer's GatewayResolver: hand it the current endpoint, or null when
  // no gateway is running. Health-probing and all requests happen renderer-side.
  ipcMain.handle("komo:gateway", () => readGateway());
  // The native directory dialog behind the workspace picker's "choose another
  // folder". Only main can open it; the renderer gets back a name + path and
  // sends the path to the gateway as an opaque `folder:` workspace id.
  ipcMain.handle("komo:choose-workspace", async () => {
    const result = await dialog.showOpenDialog({
      title: "选择 workspace 文件夹",
      properties: ["openDirectory", "createDirectory"],
    });
    const selected = result.filePaths[0];
    if (result.canceled || !selected) return null;
    return { name: path.basename(selected) || selected, path: selected };
  });
}

function createWindow(): void {
  const win = new BrowserWindow({
    width: 1100,
    height: 780,
    minWidth: 720,
    minHeight: 520,
    title: "komo",
    backgroundColor: "#07070d",
    webPreferences: {
      preload: path.join(__dirname, "../preload/index.cjs"),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
    },
  });

  // Open external links in the OS browser, never in-app.
  win.webContents.setWindowOpenHandler(({ url }) => {
    void shell.openExternal(url);
    return { action: "deny" };
  });

  // electron-vite injects the renderer dev URL; production loads the bundle.
  const devUrl = process.env.ELECTRON_RENDERER_URL;
  if (devUrl) {
    void win.loadURL(devUrl);
  } else {
    void win.loadFile(path.join(__dirname, "../renderer/index.html"));
  }
}

app.whenReady().then(() => {
  registerIpc();
  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});

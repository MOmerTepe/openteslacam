/**
 * OpenTeslaCam — Electron main process
 */
'use strict';

const { app, BrowserWindow, shell, Menu } = require('electron');
const path = require('path');

// Keep a global reference of the window object to prevent garbage collection
let mainWindow = null;

function createWindow() {
    mainWindow = new BrowserWindow({
        width: 1440,
        height: 900,
        minWidth: 900,
        minHeight: 600,
        title: 'OpenTeslaCam',
        backgroundColor: '#0a0a0a',
        icon: path.join(__dirname, 'assets', 'icon.png'),
        webPreferences: {
            nodeIntegration: false,
            contextIsolation: true,
            sandbox: false,
            // Allow file:// to access local resources (needed for dashcam-master/)
            webSecurity: true,
        },
        show: false, // wait for ready-to-show to avoid white flash
    });

    // Load the app (OTC_URL allows loading from a dev HTTP server for testing)
    if (process.env.OTC_URL) {
        mainWindow.loadURL(process.env.OTC_URL);
    } else {
        mainWindow.loadFile('index.html');
    }

    // Show once the page has rendered
    mainWindow.once('ready-to-show', () => {
        mainWindow.show();
    });

    // Open external links (e.g. OSM attribution) in the system browser
    mainWindow.webContents.setWindowOpenHandler(({ url }) => {
        shell.openExternal(url);
        return { action: 'deny' };
    });

    mainWindow.on('closed', () => {
        mainWindow = null;
    });
}

app.whenReady().then(() => {
    // No application menu (File/View/Help) — keep the window chrome clean
    Menu.setApplicationMenu(null);
    createWindow();

    // macOS: re-create window when dock icon is clicked and no windows are open
    app.on('activate', () => {
        if (BrowserWindow.getAllWindows().length === 0) createWindow();
    });
});

app.on('window-all-closed', () => {
    if (process.platform !== 'darwin') app.quit();
});

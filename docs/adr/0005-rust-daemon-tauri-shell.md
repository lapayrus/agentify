# Rust daemon and Tauri shell

We chose one Rust workspace: a long-lived daemon plus a Tauri desktop shell that shares the same language and types, with the UI rendered in the OS webview. The alternatives were a Go daemon, which is faster to write and has strong process libraries, and Electron, which is heavier at runtime. Rust avoids garbage-collection pauses in a process that supervises VMs and streams logs, keeps the installer and resident memory small, and Tauri uses WebView2/WKWebView instead of bundling a browser.

Consequences: backend control shells out to `wsl.exe`, `container`, and `limactl`; those integrations are the main Rust-side risk.

//! 应用实际使用的 Tauri 运行时。
//!
//! Linux 上 WebKitGTK 的性能与内存表现撑不起这个前端，因此 Linux 改走 CEF
//! （Chromium，见 `src-tauri/vendor/tauri-runtime-cef`）；其余平台继续用 wry
//! 包装的系统 WebView。
//!
//! Tauri 只把 `AppHandle` / `WebviewWindow` 这些泛型的默认参数设成 `Wry`，
//! 而 Linux 构建里根本不启用 wry，所以凡是要落到具体运行时的地方（全局保存的
//! `AppHandle`、`#[tauri::command]` 的参数等）都必须用这里的别名，**不要再写
//! 不带泛型参数的 `tauri::AppHandle`**——它在 Linux 上编译不过。能写成
//! `R: Runtime` 泛型的代码（插件等）照旧写泛型即可。

#[cfg(target_os = "linux")]
pub type AppRuntime = tauri_runtime_cef::CefRuntime<tauri::EventLoopMessage>;
#[cfg(not(target_os = "linux"))]
pub type AppRuntime = tauri::Wry;

pub type App = tauri::App<AppRuntime>;
pub type AppHandle = tauri::AppHandle<AppRuntime>;
pub type Builder = tauri::Builder<AppRuntime>;
pub type Webview = tauri::Webview<AppRuntime>;
pub type WebviewWindow = tauri::WebviewWindow<AppRuntime>;
pub type Window = tauri::Window<AppRuntime>;

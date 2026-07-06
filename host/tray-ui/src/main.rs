//! NebulaDisplay tray companion.
//!
//! Windows-only UI shim around the `nebulad` service: a notification-area
//! icon with quick actions (open control panel, copy viewer URL, rotate PIN,
//! start/stop service process, quit). All state lives in nebulad; the tray
//! talks to its loopback panel API over plain HTTP/1.1.

#[cfg(windows)]
mod tray {
    use std::io::{Read, Write};
    use std::mem::size_of;
    use std::net::TcpStream;
    use std::process::{Child, Command};
    use std::time::Duration;

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
        NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
        DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostQuitMessage, RegisterClassW,
        SetForegroundWindow, TrackPopupMenu, TranslateMessage, HMENU, IDI_APPLICATION,
        MF_SEPARATOR, MF_STRING, MSG, SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_RETURNCMD, WM_APP,
        WM_COMMAND, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
    };

    const WM_TRAYICON: u32 = WM_APP + 1;
    const CMD_OPEN_PANEL: usize = 1;
    const CMD_COPY_URL: usize = 2;
    const CMD_NEW_PIN: usize = 3;
    const CMD_START_STOP: usize = 4;
    const CMD_QUIT: usize = 5;

    struct State {
        panel_port: u16,
        service: Option<Child>,
    }

    static mut STATE: Option<State> = None;

    /// Minimal HTTP/1.1 request to the loopback panel API. Deliberately tiny:
    /// no TLS (loopback only), no keep-alive, 2 s timeout.
    fn http(method: &str, port: u16, path: &str) -> Option<String> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .ok()?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        )
        .ok()?;
        let mut buf = String::new();
        stream.read_to_string(&mut buf).ok()?;
        let body = buf.split_once("\r\n\r\n").map(|(_, b)| b.to_string())?;
        buf.starts_with("HTTP/1.1 200").then_some(body)
    }

    fn service_running(panel_port: u16) -> bool {
        http("GET", panel_port, "/api/status").is_some()
    }

    /// Extract `"key":"value"` / `"key":[...]` fragments without a JSON dep.
    fn json_str<'a>(json: &'a str, key: &str) -> Option<&'a str> {
        let pat = format!("\"{key}\":\"");
        let start = json.find(&pat)? + pat.len();
        let end = json[start..].find('"')? + start;
        Some(&json[start..end])
    }

    fn viewer_url(panel_port: u16) -> Option<String> {
        let body = http("GET", panel_port, "/api/status")?;
        // First entry of viewer_urls.
        let pat = "\"viewer_urls\":[\"";
        let start = body.find(pat)? + pat.len();
        let end = body[start..].find('"')? + start;
        Some(body[start..end].to_string())
    }

    fn copy_to_clipboard(text: &str) {
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
        };
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            if OpenClipboard(None).is_ok() {
                let _ = EmptyClipboard();
                if let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
                    let ptr = GlobalLock(hmem) as *mut u16;
                    if !ptr.is_null() {
                        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                        let _ = GlobalUnlock(hmem);
                        // CF_UNICODETEXT = 13
                        let _ =
                            SetClipboardData(13, Some(windows::Win32::Foundation::HANDLE(hmem.0)));
                    }
                }
                let _ = CloseClipboard();
            }
        }
    }

    fn spawn_service() -> Option<Child> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        Command::new(dir.join("nebulad.exe")).spawn().ok()
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match msg {
            WM_TRAYICON if (l.0 as u32 == WM_RBUTTONUP) || (l.0 as u32 == WM_LBUTTONUP) => {
                show_menu(hwnd);
                LRESULT(0)
            }
            WM_COMMAND => {
                handle_command(w.0 & 0xFFFF);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }

    unsafe fn show_menu(hwnd: HWND) {
        #[allow(static_mut_refs)]
        let state = STATE.as_ref().expect("state initialized");
        let running = service_running(state.panel_port);
        let menu: HMENU = CreatePopupMenu().expect("menu");
        let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_PANEL, w!("Open control panel"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_COPY_URL, w!("Copy viewer URL"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_NEW_PIN, w!("Generate new PIN"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            CMD_START_STOP,
            if running {
                w!("Stop host service")
            } else {
                w!("Start host service")
            },
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("Quit tray"));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_BOTTOMALIGN | TPM_RETURNCMD,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        if cmd.as_bool() {
            handle_command(cmd.0 as usize);
        }
    }

    fn handle_command(cmd: usize) {
        #[allow(static_mut_refs)]
        let state = unsafe { STATE.as_mut().expect("state initialized") };
        match cmd {
            CMD_OPEN_PANEL => {
                let url = format!("http://127.0.0.1:{}/panel.html\0", state.panel_port);
                let wide: Vec<u16> = url.encode_utf16().collect();
                unsafe {
                    ShellExecuteW(
                        None,
                        w!("open"),
                        PCWSTR(wide.as_ptr()),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        SW_SHOWNORMAL,
                    );
                }
            }
            CMD_COPY_URL => {
                if let Some(url) = viewer_url(state.panel_port) {
                    copy_to_clipboard(&url);
                }
            }
            CMD_NEW_PIN => {
                let _ = http("POST", state.panel_port, "/api/pin/rotate");
            }
            CMD_START_STOP => {
                if let Some(child) = &mut state.service {
                    let _ = child.kill();
                    state.service = None;
                } else if service_running(state.panel_port) {
                    // Running but not our child (e.g. a Windows service): we
                    // can't stop it safely; just do nothing.
                } else {
                    state.service = spawn_service();
                }
            }
            CMD_QUIT => unsafe { PostQuitMessage(0) },
            _ => {}
        }
    }

    pub fn run() -> anyhow::Result<()> {
        let panel_port = std::env::args()
            .skip_while(|a| a != "--panel-port")
            .nth(1)
            .and_then(|p| p.parse().ok())
            .unwrap_or(ndsp_protocol::DEFAULT_PANEL_PORT);

        unsafe {
            STATE = Some(State {
                panel_port,
                service: if service_running(panel_port) {
                    None
                } else {
                    spawn_service()
                },
            });

            let hinstance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: w!("NebulaDisplayTray"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                Default::default(),
                w!("NebulaDisplayTray"),
                w!("NebulaDisplay"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinstance.into()),
                None,
            )?;

            let mut nid = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
                uCallbackMessage: WM_TRAYICON,
                hIcon: LoadIconW(None, IDI_APPLICATION)?,
                ..Default::default()
            };
            let tip: Vec<u16> = "NebulaDisplay host".encode_utf16().collect();
            nid.szTip[..tip.len()].copy_from_slice(&tip);
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);

            #[allow(static_mut_refs)]
            if let Some(state) = STATE.as_mut() {
                if let Some(child) = &mut state.service {
                    let _ = child.kill();
                }
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    tray::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "nebula-tray is Windows-only. On this platform run `nebulad` directly \
         and open http://127.0.0.1:{}/panel.html",
        ndsp_protocol::DEFAULT_PANEL_PORT
    );
    std::process::exit(1);
}

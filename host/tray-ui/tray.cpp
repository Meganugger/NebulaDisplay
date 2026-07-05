// NebulaDisplay tray companion (Win32, zero dependencies).
//
// Sits in the notification area next to the clock and gives quick access to
// the host: open control panel, start/stop the NebulaDisplayHost service,
// and quit. Build on Windows with build.bat (any VS Developer prompt):
//
//     cl /O2 /W4 /EHsc tray.cpp /link shell32.lib advapi32.lib user32.lib
//
// The heavy lifting all lives in the nebula-host service; this is a thin
// original UI shim, so users never need the command line.

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <shellapi.h>
#include <string>

namespace {

constexpr UINT WMAPP_NOTIFY = WM_APP + 1;
constexpr UINT ID_OPEN_PANEL = 1001;
constexpr UINT ID_OPEN_VIEWER_URL = 1002;
constexpr UINT ID_SERVICE_START = 1003;
constexpr UINT ID_SERVICE_STOP = 1004;
constexpr UINT ID_QUIT = 1005;

constexpr wchar_t kServiceName[] = L"NebulaDisplayHost";
constexpr wchar_t kPanelUrl[] = L"https://localhost:38470/";
constexpr wchar_t kWindowClass[] = L"NebulaDisplayTrayWnd";

NOTIFYICONDATAW g_nid = {};

bool QueryServiceRunning() {
    SC_HANDLE scm = OpenSCManagerW(nullptr, nullptr, SC_MANAGER_CONNECT);
    if (!scm) return false;
    SC_HANDLE svc = OpenServiceW(scm, kServiceName, SERVICE_QUERY_STATUS);
    bool running = false;
    if (svc) {
        SERVICE_STATUS status = {};
        if (QueryServiceStatus(svc, &status)) {
            running = status.dwCurrentState == SERVICE_RUNNING;
        }
        CloseServiceHandle(svc);
    }
    CloseServiceHandle(scm);
    return running;
}

void ControlService(bool start) {
    SC_HANDLE scm = OpenSCManagerW(nullptr, nullptr, SC_MANAGER_CONNECT);
    if (!scm) return;
    DWORD access = start ? SERVICE_START : SERVICE_STOP;
    SC_HANDLE svc = OpenServiceW(scm, kServiceName, access);
    if (svc) {
        if (start) {
            StartServiceW(svc, 0, nullptr);
        } else {
            SERVICE_STATUS status = {};
            ControlService(svc, SERVICE_CONTROL_STOP, &status);
        }
        CloseServiceHandle(svc);
    } else if (GetLastError() == ERROR_ACCESS_DENIED) {
        MessageBoxW(nullptr,
                    L"Starting/stopping the service needs administrator rights.\n"
                    L"Right-click the tray app and run it as administrator, or use "
                    L"'services.msc'.",
                    L"NebulaDisplay", MB_ICONINFORMATION);
    }
    CloseServiceHandle(scm);
}

void ShowMenu(HWND hwnd) {
    POINT pt;
    GetCursorPos(&pt);
    HMENU menu = CreatePopupMenu();
    const bool running = QueryServiceRunning();

    AppendMenuW(menu, MF_STRING, ID_OPEN_PANEL, L"Open control panel");
    AppendMenuW(menu, MF_SEPARATOR, 0, nullptr);
    AppendMenuW(menu, MF_STRING | (running ? MF_GRAYED : 0), ID_SERVICE_START,
                L"Start host service");
    AppendMenuW(menu, MF_STRING | (running ? 0 : MF_GRAYED), ID_SERVICE_STOP,
                L"Stop host service");
    AppendMenuW(menu, MF_SEPARATOR, 0, nullptr);
    AppendMenuW(menu, MF_STRING, ID_QUIT, L"Quit tray");

    SetForegroundWindow(hwnd);
    TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, nullptr);
    DestroyMenu(menu);
}

LRESULT CALLBACK WndProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam) {
    switch (msg) {
        case WMAPP_NOTIFY:
            if (LOWORD(lParam) == WM_RBUTTONUP || LOWORD(lParam) == WM_CONTEXTMENU) {
                ShowMenu(hwnd);
            } else if (LOWORD(lParam) == WM_LBUTTONDBLCLK) {
                ShellExecuteW(nullptr, L"open", kPanelUrl, nullptr, nullptr, SW_SHOWNORMAL);
            }
            return 0;
        case WM_COMMAND:
            switch (LOWORD(wParam)) {
                case ID_OPEN_PANEL:
                    ShellExecuteW(nullptr, L"open", kPanelUrl, nullptr, nullptr, SW_SHOWNORMAL);
                    break;
                case ID_SERVICE_START:
                    ControlService(true);
                    break;
                case ID_SERVICE_STOP:
                    ControlService(false);
                    break;
                case ID_QUIT:
                    DestroyWindow(hwnd);
                    break;
            }
            return 0;
        case WM_DESTROY:
            Shell_NotifyIconW(NIM_DELETE, &g_nid);
            PostQuitMessage(0);
            return 0;
        default:
            return DefWindowProcW(hwnd, msg, wParam, lParam);
    }
}

}  // namespace

int WINAPI wWinMain(HINSTANCE hInstance, HINSTANCE, PWSTR, int) {
    // Single instance.
    CreateMutexW(nullptr, TRUE, L"NebulaDisplayTraySingleton");
    if (GetLastError() == ERROR_ALREADY_EXISTS) return 0;

    WNDCLASSW wc = {};
    wc.lpfnWndProc = WndProc;
    wc.hInstance = hInstance;
    wc.lpszClassName = kWindowClass;
    RegisterClassW(&wc);

    HWND hwnd = CreateWindowW(kWindowClass, L"NebulaDisplay", 0, 0, 0, 0, 0, HWND_MESSAGE,
                              nullptr, hInstance, nullptr);

    g_nid.cbSize = sizeof(g_nid);
    g_nid.hWnd = hwnd;
    g_nid.uID = 1;
    g_nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    g_nid.uCallbackMessage = WMAPP_NOTIFY;
    g_nid.hIcon = LoadIconW(nullptr, IDI_APPLICATION);
    wcscpy_s(g_nid.szTip, L"NebulaDisplay host");
    Shell_NotifyIconW(NIM_ADD, &g_nid);

    MSG msg;
    while (GetMessageW(&msg, nullptr, 0, 0)) {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    return 0;
}

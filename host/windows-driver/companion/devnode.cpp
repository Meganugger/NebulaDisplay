// nebula-devnode — creates/removes the root-enumerated software device that
// the NebulaDisplay IddCx driver binds to (Root\NebulaDisplayIdd).
//
// Windows has no built-in way to instantiate a root-enumerated device for a
// UMDF driver besides devgen/devcon (WDK tools) — production installs ship
// this tiny companion using the public SwDeviceCreate API instead.
//
// Build: cl /std:c++17 /EHsc devnode.cpp /link swdevice.lib onecoreuap.lib
// Usage: nebula-devnode.exe create | remove | status

#include <windows.h>
#include <swdevice.h>

#include <cstdio>
#include <string>

static const wchar_t* kInstanceId = L"NebulaDisplayIdd0";
static const wchar_t* kHardwareId = L"Root\\NebulaDisplayIdd";
static const wchar_t* kDeviceDescription = L"NebulaDisplay Virtual Display";
// Marker registry key remembering that the devnode was created (SwDevice
// lifetime SWDeviceLifetimeParentPresent persists across reboots).
static const wchar_t* kMarkerKey = L"SOFTWARE\\NebulaDisplay\\DevNode";

static HANDLE g_created = nullptr;

static VOID WINAPI CreationCallback(HSWDEVICE, HRESULT result, PVOID, PCWSTR instanceId)
{
    if (SUCCEEDED(result)) {
        wprintf(L"device created: %s\n", instanceId ? instanceId : L"(unknown)");
    } else {
        wprintf(L"device creation failed: 0x%08X\n", static_cast<unsigned>(result));
    }
    SetEvent(g_created);
}

static int CreateDevice()
{
    g_created = CreateEventW(nullptr, TRUE, FALSE, nullptr);

    // Hardware IDs are REG_MULTI_SZ-style double-NUL terminated lists.
    wchar_t hardwareIds[128] = {};
    wcscpy_s(hardwareIds, kHardwareId);

    SW_DEVICE_CREATE_INFO info = {};
    info.cbSize = sizeof(info);
    info.pszInstanceId = kInstanceId;
    info.pszzHardwareIds = hardwareIds;
    info.pszDeviceDescription = kDeviceDescription;
    info.CapabilityFlags = SWDeviceCapabilitiesRemovable | SWDeviceCapabilitiesDriverRequired;

    HSWDEVICE device = nullptr;
    HRESULT hr = SwDeviceCreate(L"NebulaDisplay", L"HTREE\\ROOT\\0", &info, 0, nullptr,
                                CreationCallback, nullptr, &device);
    if (FAILED(hr)) {
        fwprintf(stderr, L"SwDeviceCreate failed: 0x%08X\n", static_cast<unsigned>(hr));
        return 1;
    }
    WaitForSingleObject(g_created, 15000);

    // Keep the devnode alive after this process exits so nebulad doesn't need
    // to babysit it; removal happens explicitly via `remove`.
    hr = SwDeviceSetLifetime(device, SWDeviceLifetimeParentPresent);
    if (FAILED(hr)) {
        fwprintf(stderr, L"SwDeviceSetLifetime failed: 0x%08X\n", static_cast<unsigned>(hr));
        SwDeviceClose(device);
        return 1;
    }
    HKEY key = nullptr;
    if (RegCreateKeyExW(HKEY_LOCAL_MACHINE, kMarkerKey, 0, nullptr, 0, KEY_WRITE, nullptr,
                        &key, nullptr) == ERROR_SUCCESS) {
        RegCloseKey(key);
    }
    SwDeviceClose(device);
    wprintf(L"ok: virtual display device is present (persistent)\n");
    return 0;
}

static int RemoveDevice()
{
    // Re-creating the software device with ParentNotPresent lifetime and then
    // closing it removes the persistent devnode (public SwDevice semantics).
    g_created = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    wchar_t hardwareIds[128] = {};
    wcscpy_s(hardwareIds, kHardwareId);
    SW_DEVICE_CREATE_INFO info = {};
    info.cbSize = sizeof(info);
    info.pszInstanceId = kInstanceId;
    info.pszzHardwareIds = hardwareIds;
    info.pszDeviceDescription = kDeviceDescription;
    info.CapabilityFlags = SWDeviceCapabilitiesRemovable | SWDeviceCapabilitiesDriverRequired;

    HSWDEVICE device = nullptr;
    HRESULT hr = SwDeviceCreate(L"NebulaDisplay", L"HTREE\\ROOT\\0", &info, 0, nullptr,
                                CreationCallback, nullptr, &device);
    if (FAILED(hr)) {
        fwprintf(stderr, L"SwDeviceCreate(remove) failed: 0x%08X\n", static_cast<unsigned>(hr));
        return 1;
    }
    WaitForSingleObject(g_created, 15000);
    SwDeviceSetLifetime(device, SWDeviceLifetimeParentNotPresent);
    SwDeviceClose(device);
    RegDeleteKeyW(HKEY_LOCAL_MACHINE, kMarkerKey);
    wprintf(L"ok: virtual display device removed\n");
    return 0;
}

static int Status()
{
    HKEY key = nullptr;
    const bool marked =
        RegOpenKeyExW(HKEY_LOCAL_MACHINE, kMarkerKey, 0, KEY_READ, &key) == ERROR_SUCCESS;
    if (key) RegCloseKey(key);
    wprintf(L"devnode marker: %s\n", marked ? L"present" : L"absent");
    return marked ? 0 : 2;
}

int wmain(int argc, wchar_t** argv)
{
    if (argc == 2 && _wcsicmp(argv[1], L"create") == 0) return CreateDevice();
    if (argc == 2 && _wcsicmp(argv[1], L"remove") == 0) return RemoveDevice();
    if (argc == 2 && _wcsicmp(argv[1], L"status") == 0) return Status();
    fwprintf(stderr, L"usage: nebula-devnode.exe create | remove | status\n");
    return 64;
}

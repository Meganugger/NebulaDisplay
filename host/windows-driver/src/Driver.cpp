// NebulaDisplay Indirect Display Driver — implementation.
// See Driver.h for the design overview, version matrix and clean-room
// statement.

#include "Driver.h"

#include <avrt.h>
#include <dxgi1_2.h>

using Microsoft::WRL::ComPtr;

// ===========================================================================
// EDID synthesis
// ===========================================================================
// VESA E-EDID 1.4 base block for "NBD NebulaDisplay". The EDID data format is
// a public VESA standard; every byte below is derived from that spec.
std::array<BYTE, 128> BuildNdspEdid(UINT32 serial)
{
    std::array<BYTE, 128> e{};
    // Header.
    const BYTE header[8] = {0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00};
    memcpy(e.data(), header, 8);
    // Manufacturer PnP ID "NBD" → 5-bit letters (A=1): N=14, B=2, D=4.
    const UINT16 mfg = (14 << 10) | (2 << 5) | 4;
    e[8] = static_cast<BYTE>(mfg >> 8);
    e[9] = static_cast<BYTE>(mfg & 0xFF);
    // Product code 0x0001; serial distinguishes each virtual monitor so the
    // OS persists independent layout/scale settings per connector.
    e[10] = 0x01; e[11] = 0x00;
    e[12] = static_cast<BYTE>(serial & 0xFF);
    e[13] = static_cast<BYTE>((serial >> 8) & 0xFF);
    e[14] = static_cast<BYTE>((serial >> 16) & 0xFF);
    e[15] = static_cast<BYTE>((serial >> 24) & 0xFF);
    // Week 1, year 2026 (offset from 1990).
    e[16] = 1; e[17] = 2026 - 1990;
    // EDID 1.4.
    e[18] = 1; e[19] = 4;
    // Digital input, 8-bit color, DisplayPort-style.
    e[20] = 0xB5;
    // Screen size unknown (projector-style virtual display) — the OS derives
    // DPI scaling from resolution instead, which is what we want.
    e[21] = 0; e[22] = 0;
    // Gamma 2.2 → (220 - 100) = 120.
    e[23] = 120;
    // Features: preferred timing includes native info, RGB 4:4:4.
    e[24] = 0x06;
    // Chromaticity: sRGB primaries (values from the sRGB spec).
    const BYTE chroma[10] = {0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54};
    memcpy(&e[25], chroma, 10);
    // Established timings: none (we rely on the detailed descriptor + the
    // mode table reported through Parse/QueryTargetModes).
    e[35] = 0x00; e[36] = 0x00; e[37] = 0x00;
    // Standard timings: unused (0x0101).
    for (int i = 38; i < 54; i += 2) { e[i] = 0x01; e[i + 1] = 0x01; }

    // Detailed timing descriptor #1: 1920x1080 @ 60 Hz (CVT-RB style,
    // pixel clock 138.5 MHz → stored in 10 kHz units = 13850).
    BYTE* d = &e[54];
    const UINT16 pclk = 13850;
    d[0] = pclk & 0xFF; d[1] = pclk >> 8;
    // H active 1920, H blank 160 (RB).
    d[2] = 1920 & 0xFF; d[3] = 160 & 0xFF; d[4] = ((1920 >> 8) << 4) | (160 >> 8);
    // V active 1080, V blank 31.
    d[5] = 1080 & 0xFF; d[6] = 31; d[7] = ((1080 >> 8) << 4) | 0;
    // H sync offset 48, width 32; V sync offset 3, width 5.
    d[8] = 48; d[9] = 32; d[10] = (3 << 4) | 5; d[11] = 0;
    // Physical size unknown.
    d[12] = 0; d[13] = 0; d[14] = 0;
    d[15] = 0; d[16] = 0;
    // Digital separate sync, positive polarity.
    d[17] = 0x1E;

    // Descriptor #2: display name "NebulaDsply".
    BYTE* n = &e[72];
    n[0] = 0; n[1] = 0; n[2] = 0; n[3] = 0xFC; n[4] = 0;
    const char name[] = "NebulaDsply\n ";   // 13 chars, LF-terminated + pad
    memcpy(&n[5], name, 13);

    // Descriptor #3: dummy.
    e[90 + 3] = 0x10;
    // Descriptor #4: dummy.
    e[108 + 3] = 0x10;

    // No extension blocks.
    e[126] = 0;
    // Checksum: sum of all 128 bytes ≡ 0 (mod 256).
    BYTE sum = 0;
    for (int i = 0; i < 127; ++i) sum += e[i];
    e[127] = static_cast<BYTE>(256 - sum);
    return e;
}

// ===========================================================================
// Registry configuration
// ===========================================================================
// Values under the device's hardware key (set by the INF / installer /
// companion CLI; device restart applies changes → monitor hotplug):
//   MonitorCount  REG_DWORD     number of virtual monitors (1..kMaxMonitors)
//   ExtraModes    REG_MULTI_SZ  additional modes, one per string: "WxH@Hz"
static void LoadConfiguration(WDFDEVICE device, DeviceContext* ctx)
{
    ctx->monitorCount = 1;
    ctx->modes.assign(kDefaultModes.begin(), kDefaultModes.end());

    WDFKEY key = nullptr;
    NTSTATUS status = WdfDeviceOpenRegistryKey(
        device, PLUGPLAY_REGKEY_DEVICE, KEY_READ, WDF_NO_OBJECT_ATTRIBUTES, &key);
    if (!NT_SUCCESS(status)) return;

    // MonitorCount
    {
        DECLARE_CONST_UNICODE_STRING(name, L"MonitorCount");
        ULONG value = 0;
        if (NT_SUCCESS(WdfRegistryQueryULong(key, &name, &value)) && value >= 1) {
            ctx->monitorCount = min(value, kMaxMonitors);
        }
    }

    // ExtraModes ("1234x567@89" strings)
    {
        DECLARE_CONST_UNICODE_STRING(name, L"ExtraModes");
        WDFCOLLECTION strings = nullptr;
        if (NT_SUCCESS(WdfCollectionCreate(WDF_NO_OBJECT_ATTRIBUTES, &strings)) &&
            NT_SUCCESS(WdfRegistryQueryMultiString(key, &name, WDF_NO_OBJECT_ATTRIBUTES,
                                                   strings))) {
            const ULONG count = WdfCollectionGetCount(strings);
            for (ULONG i = 0; i < count; ++i) {
                WDFSTRING s = (WDFSTRING)WdfCollectionGetItem(strings, i);
                UNICODE_STRING us;
                WdfStringGetUnicodeString(s, &us);
                // Parse "WxH@Hz" (bounded copy → wcstoul).
                wchar_t buf[64] = {};
                const USHORT chars = min<USHORT>(us.Length / sizeof(WCHAR), 63);
                wmemcpy(buf, us.Buffer, chars);
                wchar_t* end = nullptr;
                const ULONG w = wcstoul(buf, &end, 10);
                if (!end || *end != L'x') continue;
                const ULONG h = wcstoul(end + 1, &end, 10);
                if (!end || *end != L'@') continue;
                const ULONG hz = wcstoul(end + 1, &end, 10);
                // Sanity + ring-capacity bounds.
                if (w >= 640 && h >= 480 && w <= ndsp::NDSP_MAX_WIDTH &&
                    h <= ndsp::NDSP_MAX_HEIGHT && hz >= 24 && hz <= 240) {
                    ctx->modes.push_back({w, h, hz});
                }
            }
        }
        if (strings) WdfObjectDelete(strings);
    }

    WdfRegistryClose(key);
}

// ===========================================================================
// FrameRing
// ===========================================================================
FrameRing::~FrameRing()
{
    if (m_header) { m_header->connected = 0; UnmapViewOfFile(m_header); }
    if (m_mapping) CloseHandle(m_mapping);
    if (m_frameEvent) CloseHandle(m_frameEvent);
}

HRESULT FrameRing::Initialize(UINT32 monitorIndex)
{
    const UINT64 total = ndsp::ring_total_bytes();
    wchar_t ringName[64];
    wchar_t eventName[64];
    ndsp::ring_name(monitorIndex, ringName, 64);
    ndsp::frame_event_name(monitorIndex, eventName, 64);

    m_mapping = CreateFileMappingW(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE,
                                   static_cast<DWORD>(total >> 32),
                                   static_cast<DWORD>(total & 0xFFFFFFFF),
                                   ringName);
    if (!m_mapping) return HRESULT_FROM_WIN32(GetLastError());

    m_base = static_cast<BYTE*>(MapViewOfFile(m_mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0));
    if (!m_base) return HRESULT_FROM_WIN32(GetLastError());
    m_header = reinterpret_cast<ndsp::RingHeader*>(m_base);

    m_frameEvent = CreateEventW(nullptr, /*manual*/ FALSE, FALSE, eventName);
    if (!m_frameEvent) return HRESULT_FROM_WIN32(GetLastError());

    m_header->magic = ndsp::NDSP_RING_MAGIC;
    m_header->version = ndsp::NDSP_RING_VERSION;
    m_header->slots = ndsp::NDSP_RING_SLOTS;
    m_header->slot_stride = static_cast<UINT32>(ndsp::ring_payload_bytes());
    m_header->latest_slot = 0xFFFFFFFF;
    m_header->connected = 0;
    m_header->monitor_index = monitorIndex;
    return S_OK;
}

void FrameRing::SetMode(UINT32 width, UINT32 height, UINT32 refreshHz)
{
    if (!m_header) return;
    m_header->width = width;
    m_header->height = height;
    m_header->refresh_hz = refreshHz;
}

void FrameRing::SetConnected(bool connected)
{
    if (m_header) m_header->connected = connected ? 1 : 0;
}

void FrameRing::PublishFrame(const void* data, UINT32 width, UINT32 height,
                             UINT32 srcPitch, UINT64 timestampQpc)
{
    if (!m_header || width > ndsp::NDSP_MAX_WIDTH || height > ndsp::NDSP_MAX_HEIGHT) return;
    const UINT32 latest = m_header->latest_slot;
    const UINT32 slot = (latest == 0xFFFFFFFF) ? 0 : (latest + 1) % ndsp::NDSP_RING_SLOTS;

    ndsp::SlotHeader& sh = m_header->slot_headers[slot];
    // Seqlock write: odd = in progress.
    const UINT32 seqStart = sh.seq + 1;
    sh.seq = seqStart;                       // odd
    MemoryBarrier();

    BYTE* dst = m_base + sizeof(ndsp::RingHeader) + static_cast<UINT64>(slot) * m_header->slot_stride;
    const UINT32 rowBytes = width * 4;
    const BYTE* src = static_cast<const BYTE*>(data);
    for (UINT32 y = 0; y < height; ++y) {
        memcpy(dst + static_cast<UINT64>(y) * rowBytes, src + static_cast<UINT64>(y) * srcPitch, rowBytes);
    }
    sh.width = width;
    sh.height = height;
    sh.pitch_bytes = rowBytes;
    sh.timestamp_qpc = timestampQpc;
    sh.frame_number = ++m_frameNumber;

    MemoryBarrier();
    sh.seq = seqStart + 1;                   // even = complete
    m_header->latest_slot = slot;
    SetEvent(m_frameEvent);
}

// ===========================================================================
// SwapChainProcessor
// ===========================================================================
SwapChainProcessor::SwapChainProcessor(IDDCX_SWAPCHAIN swapChain, HANDLE newFrameEvent,
                                       LUID renderAdapter, FrameRing* ring)
    : m_swapChain(swapChain), m_newFrameEvent(newFrameEvent), m_renderAdapter(renderAdapter),
      m_ring(ring)
{
    m_stopEvent = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    m_thread = CreateThread(nullptr, 0, ThreadProc, this, 0, nullptr);
}

SwapChainProcessor::~SwapChainProcessor()
{
    if (m_stopEvent) SetEvent(m_stopEvent);
    if (m_thread) {
        WaitForSingleObject(m_thread, 5000);
        CloseHandle(m_thread);
    }
    if (m_stopEvent) CloseHandle(m_stopEvent);
}

DWORD CALLBACK SwapChainProcessor::ThreadProc(LPVOID arg)
{
    static_cast<SwapChainProcessor*>(arg)->Run();
    return 0;
}

// Create the D3D11 device on the exact adapter the OS renders this monitor
// on (IDARG_IN_SETSWAPCHAIN::RenderAdapterLuid). Creating it on adapter 0
// would break multi-GPU systems (hybrid laptops, eGPUs, multi-card rigs):
// the swap-chain surfaces live on the render adapter and cannot be opened
// from a different device.
HRESULT SwapChainProcessor::CreateDeviceOnRenderAdapter()
{
    ComPtr<IDXGIFactory4> factory;
    HRESULT hr = CreateDXGIFactory2(0, IID_PPV_ARGS(&factory));
    if (FAILED(hr)) return hr;

    ComPtr<IDXGIAdapter1> adapter;
    hr = factory->EnumAdapterByLuid(m_renderAdapter, IID_PPV_ARGS(&adapter));
    if (FAILED(hr)) return hr;

    D3D_FEATURE_LEVEL fl;
    hr = D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                           D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                           D3D11_SDK_VERSION, &m_device, &fl, &m_context);
    if (FAILED(hr)) return hr;
    return m_device.As(&m_dxgiDevice);
}

void SwapChainProcessor::Run()
{
    // Give the processing thread multimedia scheduling so composition isn't
    // starved under load (public API; falls back silently if unavailable).
    DWORD mmcssTask = 0;
    HANDLE mmcss = AvSetMmThreadCharacteristicsW(L"Distribution", &mmcssTask);

    HRESULT hr = CreateDeviceOnRenderAdapter();
    if (SUCCEEDED(hr)) {
        IDARG_IN_SWAPCHAINSETDEVICE setDevice = {};
        setDevice.pDevice = m_dxgiDevice.Get();
        hr = IddCxSwapChainSetDevice(m_swapChain, &setDevice);
    }
    if (SUCCEEDED(hr)) {
        ProcessFrames();
    }

    if (mmcss) AvRevertMmThreadCharacteristics(mmcss);
}

HRESULT SwapChainProcessor::ProcessFrames()
{
    const HANDLE waits[2] = {m_stopEvent, m_newFrameEvent};
    for (;;) {
        IDARG_OUT_RELEASEANDACQUIREBUFFER buffer = {};
        HRESULT hr = IddCxSwapChainReleaseAndAcquireBuffer(m_swapChain, &buffer);

        if (hr == E_PENDING) {
            // No frame yet: wait for either stop or the new-frame event.
            const DWORD w = WaitForMultipleObjects(2, waits, FALSE, 16);
            if (w == WAIT_OBJECT_0) return S_OK;   // stop requested
            continue;
        }
        if (FAILED(hr)) return hr;   // swap chain abandoned (mode switch etc.)

        // Got a frame: the surface carries a reference we must release.
        ComPtr<IDXGIResource> acquired;
        acquired.Attach(buffer.MetaData.pSurface);

        // Copy it through a CPU staging texture into the ring.
        ComPtr<ID3D11Texture2D> tex;
        if (acquired && SUCCEEDED(acquired.As(&tex))) {
            D3D11_TEXTURE2D_DESC desc = {};
            tex->GetDesc(&desc);
            if (!m_staging || m_stagingW != desc.Width || m_stagingH != desc.Height) {
                D3D11_TEXTURE2D_DESC sd = desc;
                sd.Usage = D3D11_USAGE_STAGING;
                sd.BindFlags = 0;
                sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
                sd.MiscFlags = 0;
                sd.MipLevels = 1;
                sd.ArraySize = 1;
                sd.SampleDesc = {1, 0};
                m_staging.Reset();
                if (SUCCEEDED(m_device->CreateTexture2D(&sd, nullptr, &m_staging))) {
                    m_stagingW = desc.Width;
                    m_stagingH = desc.Height;
                }
            }
            if (m_staging) {
                m_context->CopyResource(m_staging.Get(), tex.Get());
                D3D11_MAPPED_SUBRESOURCE mapped = {};
                if (SUCCEEDED(m_context->Map(m_staging.Get(), 0, D3D11_MAP_READ, 0, &mapped))) {
                    LARGE_INTEGER qpc;
                    QueryPerformanceCounter(&qpc);
                    m_ring->PublishFrame(mapped.pData, desc.Width, desc.Height,
                                         mapped.RowPitch, static_cast<UINT64>(qpc.QuadPart));
                    m_context->Unmap(m_staging.Get(), 0);
                }
            }
        }
        acquired.Reset();   // release the frame reference

#if NDSP_IDDCX_HAS_FINISHED_PROCESSING_FRAME
        // IddCx 1.4+ (Win10 2004+): hint that initial processing finished so
        // the OS can start preparing the next frame while we go back to
        // ReleaseAndAcquireBuffer. Pre-1.4 headers/hosts have no equivalent —
        // ReleaseAndAcquireBuffer alone paces the pipeline there.
        hr = IddCxSwapChainFinishedProcessingFrame(m_swapChain);
        if (FAILED(hr)) return hr;
#endif

        if (WaitForSingleObject(m_stopEvent, 0) == WAIT_OBJECT_0) return S_OK;
    }
}

// ===========================================================================
// Mode helpers
// ===========================================================================
static void FillSignalInfo(DISPLAYCONFIG_VIDEO_SIGNAL_INFO& info, UINT32 w, UINT32 h, UINT32 hz)
{
    info.pixelRate = static_cast<UINT64>(w) * h * hz;   // approximation; blanking-less virtual link
    info.hSyncFreq.Numerator = static_cast<UINT32>(info.pixelRate);
    info.hSyncFreq.Denominator = w;
    info.vSyncFreq.Numerator = hz;
    info.vSyncFreq.Denominator = 1;
    info.activeSize.cx = static_cast<LONG>(w);
    info.activeSize.cy = static_cast<LONG>(h);
    info.totalSize = info.activeSize;
    info.AdditionalSignalInfo.vSyncFreqDivider = 1;
    info.AdditionalSignalInfo.videoStandard = 255; // vendor-specific
    info.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
}

static IDDCX_MONITOR_MODE MakeMonitorMode(UINT32 w, UINT32 h, UINT32 hz,
                                          IDDCX_MONITOR_MODE_ORIGIN origin)
{
    IDDCX_MONITOR_MODE mode = {};
    mode.Size = sizeof(mode);
    mode.Origin = origin;
    FillSignalInfo(mode.MonitorVideoSignalInfo, w, h, hz);
    return mode;
}

static IDDCX_TARGET_MODE MakeTargetMode(UINT32 w, UINT32 h, UINT32 hz)
{
    IDDCX_TARGET_MODE mode = {};
    mode.Size = sizeof(mode);
    FillSignalInfo(mode.TargetVideoSignalInfo.targetVideoSignalInfo, w, h, hz);
    return mode;
}

#if NDSP_IDDCX_HAS_HDR
static IDDCX_MONITOR_MODE2 MakeMonitorMode2(UINT32 w, UINT32 h, UINT32 hz,
                                            IDDCX_MONITOR_MODE_ORIGIN origin)
{
    IDDCX_MONITOR_MODE2 mode = {};
    mode.Size = sizeof(mode);
    mode.Origin = origin;
    FillSignalInfo(mode.MonitorVideoSignalInfo, w, h, hz);
    // The virtual link carries whatever the render pipeline produces; report
    // 8-bit (SDR) and 10-bit (HDR10/WCG-capable) per-component support.
    mode.BitsPerComponent.Value = IDDCX_BITS_PER_COMPONENT_8 | IDDCX_BITS_PER_COMPONENT_10;
    return mode;
}

static IDDCX_TARGET_MODE2 MakeTargetMode2(UINT32 w, UINT32 h, UINT32 hz)
{
    IDDCX_TARGET_MODE2 mode = {};
    mode.Size = sizeof(mode);
    FillSignalInfo(mode.TargetVideoSignalInfo.targetVideoSignalInfo, w, h, hz);
    mode.BitsPerComponent.Value = IDDCX_BITS_PER_COMPONENT_8 | IDDCX_BITS_PER_COMPONENT_10;
    // Uncompressed BGRA over shared memory: bandwidth is bounded by the copy,
    // not a wire — report the raw pixel rate so the OS never rules a mode out.
    mode.RequiredBandwidth = static_cast<UINT64>(w) * h * hz * 4 * 8;
    return mode;
}
#endif

// ===========================================================================
// WDF / IddCx plumbing
// ===========================================================================
extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT driverObject, PUNICODE_STRING registryPath)
{
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, NdspEvtDeviceAdd);
    return WdfDriverCreate(driverObject, registryPath, WDF_NO_OBJECT_ATTRIBUTES, &config,
                           WDF_NO_HANDLE);
}

NTSTATUS NdspEvtDeviceAdd(WDFDRIVER, PWDFDEVICE_INIT deviceInit)
{
    // Let IddCx hook the device callbacks it needs.
    IDD_CX_CLIENT_CONFIG config;
    IDD_CX_CLIENT_CONFIG_INIT(&config);
    config.EvtIddCxAdapterInitFinished = NdspAdapterInitFinished;
    config.EvtIddCxAdapterCommitModes = NdspAdapterCommitModes;
    config.EvtIddCxParseMonitorDescription = NdspParseMonitorDescription;
    config.EvtIddCxMonitorGetDefaultDescriptionModes = NdspMonitorGetDefaultModes;
    config.EvtIddCxMonitorQueryTargetModes = NdspMonitorQueryModes;
    config.EvtIddCxMonitorAssignSwapChain = NdspMonitorAssignSwapChain;
    config.EvtIddCxMonitorUnassignSwapChain = NdspMonitorUnassignSwapChain;
#if NDSP_IDDCX_HAS_HDR
    // IddCx 1.10+ hosts call the *2 variants (wire-format aware). Both sets
    // stay registered: pre-1.10 hosts ignore fields they don't know.
    config.EvtIddCxAdapterQueryTargetInfo = NdspAdapterQueryTargetInfo;
    config.EvtIddCxParseMonitorDescription2 = NdspParseMonitorDescription2;
    config.EvtIddCxMonitorQueryTargetModes2 = NdspMonitorQueryModes2;
    config.EvtIddCxAdapterCommitModes2 = NdspAdapterCommitModes2;
    config.EvtIddCxMonitorSetDefaultHdrMetadata = NdspMonitorSetDefaultHdrMetadata;
#endif
    NTSTATUS status = IddCxDeviceInitConfig(deviceInit, &config);
    if (!NT_SUCCESS(status)) return status;

    WDF_PNPPOWER_EVENT_CALLBACKS pnpCallbacks;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&pnpCallbacks);
    pnpCallbacks.EvtDeviceD0Entry = NdspEvtD0Entry;
    WdfDeviceInitSetPnpPowerEventCallbacks(deviceInit, &pnpCallbacks);

    WDF_OBJECT_ATTRIBUTES attr;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attr, DeviceContext);
    attr.EvtCleanupCallback = [](WDFOBJECT obj) {
        // Run C++ destructors for the placement-initialized context.
        DeviceContext* ctx = WdfObjectGet_DeviceContext(obj);
        ctx->~DeviceContext();
    };

    WDFDEVICE device = nullptr;
    status = WdfDeviceCreate(&deviceInit, &attr, &device);
    if (!NT_SUCCESS(status)) return status;

    DeviceContext* ctx = WdfObjectGet_DeviceContext(device);
    new (ctx) DeviceContext();
    ctx->wdfDevice = device;
    LoadConfiguration(device, ctx);

    return IddCxDeviceInitialize(device);
}

NTSTATUS NdspEvtD0Entry(WDFDEVICE device, WDF_POWER_DEVICE_STATE)
{
    DeviceContext* ctx = WdfObjectGet_DeviceContext(device);
    if (ctx->adapter) return STATUS_SUCCESS;   // already initialized (resume)

    // Describe and create the IddCx adapter.
    IDDCX_ADAPTER_CAPS caps = {};
    caps.Size = sizeof(caps);
    caps.MaxMonitorsSupported = ctx->monitorCount;
    caps.EndPointDiagnostics.Size = sizeof(caps.EndPointDiagnostics);
    caps.EndPointDiagnostics.GammaSupport = IDDCX_FEATURE_IMPLEMENTATION_NONE;
    caps.EndPointDiagnostics.TransmissionType = IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;

    static const WCHAR kFriendlyName[] = L"NebulaDisplay Virtual Adapter";
    caps.EndPointDiagnostics.pEndPointFriendlyName = kFriendlyName;
    static const WCHAR kManufacturer[] = L"NebulaDisplay";
    caps.EndPointDiagnostics.pEndPointManufacturerName = kManufacturer;
    static const WCHAR kModel[] = L"NDSP-VDD-1";
    caps.EndPointDiagnostics.pEndPointModelName = kModel;
    // Firmware/hardware versions are IDDCX_ENDPOINT_VERSION structs (NOT
    // strings — passing WCHAR* here was the old compile error).
    static IDDCX_ENDPOINT_VERSION kVersion = []() {
        IDDCX_ENDPOINT_VERSION v = {};
        v.Size = sizeof(v);
        v.MajorVer = 0;
        v.MinorVer = 3;
        v.Build = 0;
        return v;
    }();
    caps.EndPointDiagnostics.pFirmwareVersion = &kVersion;
    caps.EndPointDiagnostics.pHardwareVersion = &kVersion;

    WDF_OBJECT_ATTRIBUTES adapterAttr;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&adapterAttr, AdapterContextWrapper);

    IDARG_IN_ADAPTER_INIT adapterInit = {};
    adapterInit.WdfDevice = device;
    adapterInit.pCaps = &caps;
    adapterInit.ObjectAttributes = &adapterAttr;
    IDARG_OUT_ADAPTER_INIT adapterOut = {};
    NTSTATUS status = IddCxAdapterInitAsync(&adapterInit, &adapterOut);
    if (NT_SUCCESS(status)) {
        ctx->adapter = adapterOut.AdapterObject;
        WdfObjectGet_AdapterContextWrapper(adapterOut.AdapterObject)->ctx = ctx;
    }
    return status;
}

// Plug in one virtual monitor on `connector`.
static NTSTATUS PlugInMonitor(DeviceContext* ctx, UINT32 connector)
{
    MonitorState& ms = ctx->monitors[connector];
    ms.edid = BuildNdspEdid(connector + 1);

    ms.ring = std::make_unique<FrameRing>();
    if (FAILED(ms.ring->Initialize(connector))) {
        // Non-fatal: the monitor still works, frames just can't reach the
        // service (it will report "driver ring unavailable").
        ms.ring.reset();
    }

    IDDCX_MONITOR_INFO monitorInfo = {};
    monitorInfo.Size = sizeof(monitorInfo);
    monitorInfo.MonitorType = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL;
    monitorInfo.ConnectorIndex = connector;
    monitorInfo.MonitorDescription.Size = sizeof(monitorInfo.MonitorDescription);
    monitorInfo.MonitorDescription.Type = IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
    monitorInfo.MonitorDescription.DataSize = static_cast<UINT>(ms.edid.size());
    monitorInfo.MonitorDescription.pData = ms.edid.data();
    // Stable per-connector container id so Windows remembers layout/scale.
    // Base GUID generated for NebulaDisplay; last byte varies per connector.
    GUID containerId =
        {0x6fa5f3a2, 0x9c3b, 0x4f0a, {0x8e, 0x0d, 0x3b, 0x1a, 0x2c, 0x4d, 0x5e, 0x6f}};
    containerId.Data4[7] = static_cast<BYTE>(0x6f + connector);
    monitorInfo.MonitorContainerId = containerId;

    WDF_OBJECT_ATTRIBUTES monitorAttr;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&monitorAttr, MonitorContextWrapper);

    IDARG_IN_MONITORCREATE monitorCreate = {};
    monitorCreate.ObjectAttributes = &monitorAttr;
    monitorCreate.pMonitorInfo = &monitorInfo;
    IDARG_OUT_MONITORCREATE monitorOut = {};
    NTSTATUS status = IddCxMonitorCreate(ctx->adapter, &monitorCreate, &monitorOut);
    if (!NT_SUCCESS(status)) return status;
    ms.monitor = monitorOut.MonitorObject;
    auto* wrapper = WdfObjectGet_MonitorContextWrapper(ms.monitor);
    wrapper->ctx = ctx;
    wrapper->state = &ms;

    IDARG_OUT_MONITORARRIVAL arrivalOut = {};
    status = IddCxMonitorArrival(ms.monitor, &arrivalOut);
    if (NT_SUCCESS(status) && ms.ring) ms.ring->SetConnected(true);
    return status;
}

NTSTATUS NdspAdapterInitFinished(IDDCX_ADAPTER adapter, const IDARG_IN_ADAPTER_INIT_FINISHED* args)
{
    if (!NT_SUCCESS(args->AdapterInitStatus)) return args->AdapterInitStatus;

    DeviceContext* ctx = WdfObjectGet_AdapterContextWrapper(adapter)->ctx;
    for (UINT32 i = 0; i < ctx->monitorCount; ++i) {
        const NTSTATUS status = PlugInMonitor(ctx, i);
        if (!NT_SUCCESS(status)) return status;
    }
    return STATUS_SUCCESS;
}

NTSTATUS NdspParseMonitorDescription(const IDARG_IN_PARSEMONITORDESCRIPTION* in,
                                     IDARG_OUT_PARSEMONITORDESCRIPTION* out)
{
    // Report the modes we consider part of the (synthesized) EDID. The mode
    // table is device-global; grab it through the WDF driver context is not
    // possible here (no handle), so the default table is used — the *device*
    // mode table (with registry extras) flows through QueryTargetModes.
    out->MonitorModeBufferOutputCount = static_cast<UINT32>(kDefaultModes.size());
    if (in->MonitorModeBufferInputCount < kDefaultModes.size()) {
        // Caller probes for the required buffer size first.
        return (in->MonitorModeBufferInputCount == 0) ? STATUS_SUCCESS : STATUS_BUFFER_TOO_SMALL;
    }
    for (size_t i = 0; i < kDefaultModes.size(); ++i) {
        in->pMonitorModes[i] = MakeMonitorMode(
            kDefaultModes[i].width, kDefaultModes[i].height, kDefaultModes[i].vsync_hz,
            IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR);
    }
    out->PreferredMonitorModeIdx = kPreferredModeIndex;
    return STATUS_SUCCESS;
}

NTSTATUS NdspMonitorGetDefaultModes(IDDCX_MONITOR,
                                    const IDARG_IN_GETDEFAULTDESCRIPTIONMODES* in,
                                    IDARG_OUT_GETDEFAULTDESCRIPTIONMODES* out)
{
    // Only used when the description can't be parsed; mirror the same table.
    out->DefaultMonitorModeBufferOutputCount = static_cast<UINT32>(kDefaultModes.size());
    if (in->DefaultMonitorModeBufferInputCount < kDefaultModes.size()) {
        return (in->DefaultMonitorModeBufferInputCount == 0) ? STATUS_SUCCESS
                                                             : STATUS_BUFFER_TOO_SMALL;
    }
    for (size_t i = 0; i < kDefaultModes.size(); ++i) {
        in->pDefaultMonitorModes[i] = MakeMonitorMode(
            kDefaultModes[i].width, kDefaultModes[i].height, kDefaultModes[i].vsync_hz,
            IDDCX_MONITOR_MODE_ORIGIN_DRIVER);
    }
    out->PreferredMonitorModeIdx = kPreferredModeIndex;
    return STATUS_SUCCESS;
}

NTSTATUS NdspMonitorQueryModes(IDDCX_MONITOR monitor, const IDARG_IN_QUERYTARGETMODES* in,
                               IDARG_OUT_QUERYTARGETMODES* out)
{
    const DeviceContext* ctx = WdfObjectGet_MonitorContextWrapper(monitor)->ctx;
    const auto& modes = ctx->modes;
    out->TargetModeBufferOutputCount = static_cast<UINT32>(modes.size());
    if (in->TargetModeBufferInputCount < modes.size()) {
        return (in->TargetModeBufferInputCount == 0) ? STATUS_SUCCESS : STATUS_BUFFER_TOO_SMALL;
    }
    for (size_t i = 0; i < modes.size(); ++i) {
        in->pTargetModes[i] = MakeTargetMode(modes[i].width, modes[i].height, modes[i].vsync_hz);
    }
    return STATUS_SUCCESS;
}

// Shared by CommitModes and CommitModes2.
static void CommitPath(DeviceContext* ctx, IDDCX_MONITOR monitorObject, bool active,
                       const DISPLAYCONFIG_VIDEO_SIGNAL_INFO& sig)
{
    MonitorState* ms = ctx->FindMonitor(monitorObject);
    if (!ms || !active) return;
    ms->currentWidth = static_cast<UINT32>(sig.activeSize.cx);
    ms->currentHeight = static_cast<UINT32>(sig.activeSize.cy);
    ms->currentHz = sig.vSyncFreq.Denominator
                        ? sig.vSyncFreq.Numerator / sig.vSyncFreq.Denominator
                        : 60;
    if (ms->ring) ms->ring->SetMode(ms->currentWidth, ms->currentHeight, ms->currentHz);
}

NTSTATUS NdspAdapterCommitModes(IDDCX_ADAPTER adapter, const IDARG_IN_COMMITMODES* in)
{
    DeviceContext* ctx = WdfObjectGet_AdapterContextWrapper(adapter)->ctx;
    for (UINT32 i = 0; i < in->PathCount; ++i) {
        const IDDCX_PATH& path = in->pPaths[i];
        CommitPath(ctx, path.MonitorObject, (path.Flags & IDDCX_PATH_FLAGS_ACTIVE) != 0,
                   path.TargetVideoSignalInfo);
    }
    return STATUS_SUCCESS;
}

NTSTATUS NdspMonitorAssignSwapChain(IDDCX_MONITOR monitor, const IDARG_IN_SETSWAPCHAIN* in)
{
    auto* wrapper = WdfObjectGet_MonitorContextWrapper(monitor);
    MonitorState* ms = wrapper->state;
    ms->processor.reset();   // drop any previous swap chain first
    ms->processor = std::make_unique<SwapChainProcessor>(
        in->hSwapChain, in->hNextSurfaceAvailable, in->RenderAdapterLuid, ms->ring.get());
    return STATUS_SUCCESS;
}

NTSTATUS NdspMonitorUnassignSwapChain(IDDCX_MONITOR monitor)
{
    WdfObjectGet_MonitorContextWrapper(monitor)->state->processor.reset();
    return STATUS_SUCCESS;
}

#if NDSP_IDDCX_HAS_HDR
// ---------------------------------------------------------------------------
// IddCx 1.10+ (Windows 11 22H2+): wire-format aware variants + HDR plumbing.
// ---------------------------------------------------------------------------
NTSTATUS NdspAdapterQueryTargetInfo(IDDCX_ADAPTER, IDARG_IN_QUERYTARGET_INFO*,
                                    IDARG_OUT_QUERYTARGET_INFO* out)
{
    // The virtual link is lossless shared memory: wide (WCG) and high (HDR)
    // color spaces pass through untouched; no dithering is applied.
    out->TargetCaps = static_cast<IDDCX_TARGET_CAPS>(IDDCX_TARGET_CAPS_WIDE_COLOR_SPACE |
                                                     IDDCX_TARGET_CAPS_HIGH_COLOR_SPACE);
    out->DitheringSupport.Value = IDDCX_BITS_PER_COMPONENT_NONE;
    return STATUS_SUCCESS;
}

NTSTATUS NdspParseMonitorDescription2(const IDARG_IN_PARSEMONITORDESCRIPTION2* in,
                                      IDARG_OUT_PARSEMONITORDESCRIPTION* out)
{
    out->MonitorModeBufferOutputCount = static_cast<UINT32>(kDefaultModes.size());
    if (in->MonitorModeBufferInputCount < kDefaultModes.size()) {
        return (in->MonitorModeBufferInputCount == 0) ? STATUS_SUCCESS : STATUS_BUFFER_TOO_SMALL;
    }
    for (size_t i = 0; i < kDefaultModes.size(); ++i) {
        in->pMonitorModes[i] = MakeMonitorMode2(
            kDefaultModes[i].width, kDefaultModes[i].height, kDefaultModes[i].vsync_hz,
            IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR);
    }
    out->PreferredMonitorModeIdx = kPreferredModeIndex;
    return STATUS_SUCCESS;
}

NTSTATUS NdspMonitorQueryModes2(IDDCX_MONITOR monitor, const IDARG_IN_QUERYTARGETMODES2* in,
                                IDARG_OUT_QUERYTARGETMODES* out)
{
    const DeviceContext* ctx = WdfObjectGet_MonitorContextWrapper(monitor)->ctx;
    const auto& modes = ctx->modes;
    out->TargetModeBufferOutputCount = static_cast<UINT32>(modes.size());
    if (in->TargetModeBufferInputCount < modes.size()) {
        return (in->TargetModeBufferInputCount == 0) ? STATUS_SUCCESS : STATUS_BUFFER_TOO_SMALL;
    }
    for (size_t i = 0; i < modes.size(); ++i) {
        in->pTargetModes[i] = MakeTargetMode2(modes[i].width, modes[i].height, modes[i].vsync_hz);
    }
    return STATUS_SUCCESS;
}

NTSTATUS NdspAdapterCommitModes2(IDDCX_ADAPTER adapter, const IDARG_IN_COMMITMODES2* in)
{
    DeviceContext* ctx = WdfObjectGet_AdapterContextWrapper(adapter)->ctx;
    for (UINT32 i = 0; i < in->PathCount; ++i) {
        const IDDCX_PATH2& path = in->pPaths[i];
        CommitPath(ctx, path.MonitorObject, (path.Flags & IDDCX_PATH_FLAGS_ACTIVE) != 0,
                   path.TargetVideoSignalInfo);
    }
    return STATUS_SUCCESS;
}

NTSTATUS NdspMonitorSetDefaultHdrMetadata(IDDCX_MONITOR,
                                          const IDARG_IN_MONITOR_SET_DEFAULT_HDR_METADATA*)
{
    // The stream carries SDR-tonemapped content today; default HDR metadata
    // needs no persistence on a virtual link — accept and continue.
    return STATUS_SUCCESS;
}
#endif // NDSP_IDDCX_HAS_HDR

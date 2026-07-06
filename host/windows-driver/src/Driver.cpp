// NebulaDisplay Indirect Display Driver — implementation.
// See Driver.h for the design overview and clean-room statement.

#include "Driver.h"

#include <avrt.h>
#include <dxgi1_2.h>

using Microsoft::WRL::ComPtr;

// ===========================================================================
// EDID synthesis
// ===========================================================================
// VESA E-EDID 1.4 base block for "NBD NebulaDisplay". The EDID data format is
// a public VESA standard; every byte below is derived from that spec.
std::array<BYTE, 128> BuildNdspEdid()
{
    std::array<BYTE, 128> e{};
    // Header.
    const BYTE header[8] = {0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00};
    memcpy(e.data(), header, 8);
    // Manufacturer PnP ID "NBD" → 5-bit letters (A=1): N=14, B=2, D=4.
    const UINT16 mfg = (14 << 10) | (2 << 5) | 4;
    e[8] = static_cast<BYTE>(mfg >> 8);
    e[9] = static_cast<BYTE>(mfg & 0xFF);
    // Product code 0x0001, serial 1.
    e[10] = 0x01; e[11] = 0x00;
    e[12] = 0x01; e[13] = 0x00; e[14] = 0x00; e[15] = 0x00;
    // Week 1, year 2026 (offset from 1990).
    e[16] = 1; e[17] = 2026 - 1990;
    // EDID 1.4.
    e[18] = 1; e[19] = 4;
    // Digital input, 8-bit color, DisplayPort-style.
    e[20] = 0xB5;
    // Screen size unknown (projector-style virtual display).
    e[21] = 0; e[22] = 0;
    // Gamma 2.2 → (220 - 100) = 120.
    e[23] = 120;
    // Features: preferred timing includes native info, RGB 4:4:4.
    e[24] = 0x06;
    // Chromaticity: sRGB primaries (values from the sRGB spec).
    const BYTE chroma[10] = {0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54};
    memcpy(&e[25], chroma, 10);
    // Established timings: none (we rely on the detailed descriptor + DMTs
    // reported through QueryTargetModes).
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

    // Descriptor #2: display name "NebulaDisplay".
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
// FrameRing
// ===========================================================================
FrameRing::~FrameRing()
{
    if (m_header) { m_header->connected = 0; UnmapViewOfFile(m_header); }
    if (m_mapping) CloseHandle(m_mapping);
    if (m_frameEvent) CloseHandle(m_frameEvent);
}

HRESULT FrameRing::Initialize()
{
    const UINT64 total = ndsp::ring_total_bytes();
    m_mapping = CreateFileMappingW(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE,
                                   static_cast<DWORD>(total >> 32),
                                   static_cast<DWORD>(total & 0xFFFFFFFF),
                                   ndsp::NDSP_RING_NAME);
    if (!m_mapping) return HRESULT_FROM_WIN32(GetLastError());

    m_base = static_cast<BYTE*>(MapViewOfFile(m_mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0));
    if (!m_base) return HRESULT_FROM_WIN32(GetLastError());
    m_header = reinterpret_cast<ndsp::RingHeader*>(m_base);

    m_frameEvent = CreateEventW(nullptr, /*manual*/ FALSE, FALSE, ndsp::NDSP_FRAME_EVENT);
    if (!m_frameEvent) return HRESULT_FROM_WIN32(GetLastError());

    m_header->magic = ndsp::NDSP_RING_MAGIC;
    m_header->version = ndsp::NDSP_RING_VERSION;
    m_header->slots = ndsp::NDSP_RING_SLOTS;
    m_header->slot_stride = static_cast<UINT32>(ndsp::ring_payload_bytes());
    m_header->latest_slot = 0xFFFFFFFF;
    m_header->connected = 0;
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
                                       FrameRing* ring)
    : m_swapChain(swapChain), m_newFrameEvent(newFrameEvent), m_ring(ring)
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

void SwapChainProcessor::Run()
{
    // Give the processing thread multimedia scheduling so composition isn't
    // starved under load (public API; falls back silently if unavailable).
    DWORD mmcssTask = 0;
    HANDLE mmcss = AvSetMmThreadCharacteristicsW(L"Distribution", &mmcssTask);

    // Create the D3D device the swap chain will be realized on.
    D3D_FEATURE_LEVEL fl;
    HRESULT hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr,
                                   D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                                   D3D11_SDK_VERSION, &m_device, &fl, &m_context);
    if (FAILED(hr)) {
        // WARP fallback keeps the virtual display alive without a GPU.
        hr = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_WARP, nullptr,
                               D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                               D3D11_SDK_VERSION, &m_device, &fl, &m_context);
    }
    if (SUCCEEDED(hr)) hr = m_device.As(&m_dxgiDevice);
    if (SUCCEEDED(hr)) {
        IDARG_IN_SWAPCHAINSETDEVICE setDevice = {};
        setDevice.pDevice = m_dxgiDevice.Get();
        hr = IddCxSwapChainSetDevice(m_swapChain, &setDevice);
    }
    if (SUCCEEDED(hr)) {
        ProcessFrames();
    }

    // Always finish access so IddCx can tear the swap chain down.
    IddCxSwapChainFinishedProcessing(m_swapChain);
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
        if (FAILED(hr)) return hr;

        // Got a frame: copy it through a CPU staging texture into the ring.
        ComPtr<IDXGIResource> res = buffer.MetaData.pSurface;
        ComPtr<ID3D11Texture2D> tex;
        if (res && SUCCEEDED(res.As(&tex))) {
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

        // Report the frame as processed so DWM can reuse the buffer.
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

    return IddCxDeviceInitialize(device);
}

NTSTATUS NdspEvtD0Entry(WDFDEVICE device, WDF_POWER_DEVICE_STATE)
{
    DeviceContext* ctx = WdfObjectGet_DeviceContext(device);
    if (ctx->adapter) return STATUS_SUCCESS;   // already initialized (resume)

    // Describe and create the IddCx adapter.
    IDDCX_ADAPTER_CAPS caps = {};
    caps.Size = sizeof(caps);
    caps.MaxMonitorsSupported = 1;
    caps.EndPointDiagnostics.Size = sizeof(caps.EndPointDiagnostics);
    caps.EndPointDiagnostics.GammaSupport = IDDCX_FEATURE_IMPLEMENTATION_NONE;
    caps.EndPointDiagnostics.TransmissionType = IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;

    static const WCHAR kFriendlyName[] = L"NebulaDisplay Virtual Adapter";
    caps.EndPointDiagnostics.pEndPointFriendlyName = kFriendlyName;
    static const WCHAR kManufacturer[] = L"NebulaDisplay";
    caps.EndPointDiagnostics.pEndPointManufacturerName = kManufacturer;
    static const WCHAR kModel[] = L"NDSP-VDD-1";
    caps.EndPointDiagnostics.pEndPointModelName = kModel;
    static const WCHAR kFirmware[] = L"0.2.0";
    static const WCHAR kHardware[] = L"0.2.0";
    caps.EndPointDiagnostics.pFirmwareVersion = kFirmware;
    caps.EndPointDiagnostics.pHardwareVersion = kHardware;

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

NTSTATUS NdspAdapterInitFinished(IDDCX_ADAPTER adapter, const IDARG_IN_ADAPTER_INIT_FINISHED* args)
{
    if (!NT_SUCCESS(args->AdapterInitStatus)) return args->AdapterInitStatus;

    DeviceContext* ctx = WdfObjectGet_AdapterContextWrapper(adapter)->ctx;

    ctx->ring = std::make_unique<FrameRing>();
    if (FAILED(ctx->ring->Initialize())) {
        // Non-fatal: the monitor still works, frames just can't reach the
        // service (it will report "driver ring unavailable").
        ctx->ring.reset();
    }

    // Plug in the (single) virtual monitor.
    static std::array<BYTE, 128> edid = BuildNdspEdid();

    IDDCX_MONITOR_INFO monitorInfo = {};
    monitorInfo.Size = sizeof(monitorInfo);
    monitorInfo.MonitorType = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL;
    monitorInfo.ConnectorIndex = 0;
    monitorInfo.MonitorDescription.Size = sizeof(monitorInfo.MonitorDescription);
    monitorInfo.MonitorDescription.Type = IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
    monitorInfo.MonitorDescription.DataSize = static_cast<UINT>(edid.size());
    monitorInfo.MonitorDescription.pData = edid.data();
    // Stable container id so Windows remembers layout/scale for this monitor.
    // {6FA5F3A2-9C3B-4F0A-8E0D-3B1A2C4D5E6F} — generated for NebulaDisplay.
    static const GUID kContainerId =
        {0x6fa5f3a2, 0x9c3b, 0x4f0a, {0x8e, 0x0d, 0x3b, 0x1a, 0x2c, 0x4d, 0x5e, 0x6f}};
    monitorInfo.MonitorContainerId = kContainerId;

    WDF_OBJECT_ATTRIBUTES monitorAttr;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&monitorAttr, MonitorContextWrapper);

    IDARG_IN_MONITORCREATE monitorCreate = {};
    monitorCreate.ObjectAttributes = &monitorAttr;
    monitorCreate.pMonitorInfo = &monitorInfo;
    IDARG_OUT_MONITORCREATE monitorOut = {};
    NTSTATUS status = IddCxMonitorCreate(adapter, &monitorCreate, &monitorOut);
    if (!NT_SUCCESS(status)) return status;
    ctx->monitor = monitorOut.MonitorObject;
    WdfObjectGet_MonitorContextWrapper(ctx->monitor)->ctx = ctx;

    IDARG_OUT_MONITORARRIVAL arrivalOut = {};
    status = IddCxMonitorArrival(ctx->monitor, &arrivalOut);
    if (NT_SUCCESS(status) && ctx->ring) ctx->ring->SetConnected(true);
    return status;
}

NTSTATUS NdspParseMonitorDescription(const IDARG_IN_PARSEMONITORDESCRIPTION* in,
                                     IDARG_OUT_PARSEMONITORDESCRIPTION* out)
{
    // Report the modes we consider part of the (synthesized) EDID.
    out->MonitorModeBufferOutputCount = static_cast<UINT32>(kModes.size());
    if (in->MonitorModeBufferInputCount < kModes.size()) {
        // Caller probes for the required buffer size first.
        return (in->MonitorModeBufferInputCount == 0) ? STATUS_SUCCESS : STATUS_BUFFER_TOO_SMALL;
    }
    for (size_t i = 0; i < kModes.size(); ++i) {
        in->pMonitorModes[i] = MakeMonitorMode(
            kModes[i].width, kModes[i].height, kModes[i].vsync_hz,
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
    out->DefaultMonitorModeBufferOutputCount = static_cast<UINT32>(kModes.size());
    if (in->DefaultMonitorModeBufferInputCount < kModes.size()) {
        return (in->DefaultMonitorModeBufferInputCount == 0) ? STATUS_SUCCESS
                                                             : STATUS_BUFFER_TOO_SMALL;
    }
    for (size_t i = 0; i < kModes.size(); ++i) {
        in->pDefaultMonitorModes[i] = MakeMonitorMode(
            kModes[i].width, kModes[i].height, kModes[i].vsync_hz,
            IDDCX_MONITOR_MODE_ORIGIN_DRIVER);
    }
    out->PreferredMonitorModeIdx = kPreferredModeIndex;
    return STATUS_SUCCESS;
}

NTSTATUS NdspMonitorQueryModes(IDDCX_MONITOR, const IDARG_IN_QUERYTARGETMODES* in,
                               IDARG_OUT_QUERYTARGETMODES* out)
{
    out->TargetModeBufferOutputCount = static_cast<UINT32>(kModes.size());
    if (in->TargetModeBufferInputCount < kModes.size()) {
        return (in->TargetModeBufferInputCount == 0) ? STATUS_SUCCESS : STATUS_BUFFER_TOO_SMALL;
    }
    for (size_t i = 0; i < kModes.size(); ++i) {
        in->pTargetModes[i] = MakeTargetMode(kModes[i].width, kModes[i].height, kModes[i].vsync_hz);
    }
    return STATUS_SUCCESS;
}

NTSTATUS NdspAdapterCommitModes(IDDCX_ADAPTER adapter, const IDARG_IN_COMMITMODES* in)
{
    DeviceContext* ctx = WdfObjectGet_AdapterContextWrapper(adapter)->ctx;
    for (UINT32 i = 0; i < in->PathCount; ++i) {
        const IDDCX_PATH& path = in->pPaths[i];
        if (path.MonitorObject == ctx->monitor &&
            (path.Flags & IDDCX_PATH_FLAGS_ACTIVE)) {
            const auto& sig = path.TargetVideoSignalInfo;
            ctx->currentWidth = static_cast<UINT32>(sig.activeSize.cx);
            ctx->currentHeight = static_cast<UINT32>(sig.activeSize.cy);
            ctx->currentHz = sig.vSyncFreq.Denominator
                                 ? sig.vSyncFreq.Numerator / sig.vSyncFreq.Denominator
                                 : 60;
            if (ctx->ring) ctx->ring->SetMode(ctx->currentWidth, ctx->currentHeight, ctx->currentHz);
        }
    }
    return STATUS_SUCCESS;
}

NTSTATUS NdspMonitorAssignSwapChain(IDDCX_MONITOR monitor, const IDARG_IN_SETSWAPCHAIN* in)
{
    DeviceContext* ctx = WdfObjectGet_MonitorContextWrapper(monitor)->ctx;
    ctx->processor.reset();   // drop any previous swap chain first
    ctx->processor = std::make_unique<SwapChainProcessor>(
        in->hSwapChain, in->hNextSurfaceAvailable, ctx->ring.get());
    return STATUS_SUCCESS;
}

NTSTATUS NdspMonitorUnassignSwapChain(IDDCX_MONITOR monitor)
{
    DeviceContext* ctx = WdfObjectGet_MonitorContextWrapper(monitor)->ctx;
    ctx->processor.reset();
    return STATUS_SUCCESS;
}

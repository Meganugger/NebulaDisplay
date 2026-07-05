// NebulaDisplay IddCx driver implementation. See Driver.h for the overview.

#include <wrl.h>
#include "Driver.h"

using Microsoft::WRL::ComPtr;
using namespace nebula;

// ---------------------------------------------------------------------------
// Supported virtual monitor modes.
//
// The default monitor advertises a broad set of common resolutions and
// refresh rates; Windows picks the user's choice in Display Settings and
// commits it through EvtAdapterCommitModes.
// ---------------------------------------------------------------------------

struct ModeEntry { UINT32 w, h, hz; };

static constexpr ModeEntry kModes[] = {
    {1280,  720, 60},
    {1366,  768, 60},
    {1600,  900, 60},
    {1920, 1080, 30}, {1920, 1080, 60}, {1920, 1080, 90}, {1920, 1080, 120},
    {2560, 1440, 30}, {2560, 1440, 60}, {2560, 1440, 90},
    {3840, 2160, 30}, {3840, 2160, 60},
    // Portrait-friendly tablet modes.
    {1200, 1920, 60},
    {1668, 2388, 60},
};

static constexpr ModeEntry kPreferredMode = {1920, 1080, 60};

// Fill an IddCx target mode from a mode entry (standard CVT-RB style timing
// summary; IddCx only needs totals/sync summaries to be self-consistent).
static void FillSignalInfo(DISPLAYCONFIG_VIDEO_SIGNAL_INFO& info, UINT32 w, UINT32 h, UINT32 hz) {
    info = {};
    info.totalSize.cx = w;
    info.totalSize.cy = h;
    info.activeSize.cx = w;
    info.activeSize.cy = h;
    info.vSyncFreq.Numerator = hz;
    info.vSyncFreq.Denominator = 1;
    info.hSyncFreq.Numerator = hz * h;
    info.hSyncFreq.Denominator = 1;
    info.pixelRate = static_cast<UINT64>(w) * h * hz;
    info.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
}

static IDDCX_TARGET_MODE MakeTargetMode(const ModeEntry& m) {
    IDDCX_TARGET_MODE mode = {};
    mode.Size = sizeof(mode);
    FillSignalInfo(mode.TargetVideoSignalInfo.targetVideoSignalInfo, m.w, m.h, m.hz);
    return mode;
}

static IDDCX_MONITOR_MODE MakeMonitorMode(const ModeEntry& m,
                                          IDDCX_MONITOR_MODE_ORIGIN origin) {
    IDDCX_MONITOR_MODE mode = {};
    mode.Size = sizeof(mode);
    mode.Origin = origin;
    FillSignalInfo(mode.MonitorVideoSignalInfo, m.w, m.h, m.hz);
    return mode;
}

// ---------------------------------------------------------------------------
// DriverEntry / device bring-up
// ---------------------------------------------------------------------------

extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT driverObject, PUNICODE_STRING registryPath) {
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, NebulaDeviceAdd);

    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT(&attributes);

    return WdfDriverCreate(driverObject, registryPath, &attributes, &config, WDF_NO_HANDLE);
}

NTSTATUS NebulaDeviceAdd(WDFDRIVER, PWDFDEVICE_INIT deviceInit) {
    // Register IddCx device callbacks before creating the WDF device.
    IDD_CX_CLIENT_CONFIG iddConfig;
    IDD_CX_CLIENT_CONFIG_INIT(&iddConfig);
    iddConfig.EvtIddCxAdapterInitFinished = NebulaAdapterInitFinished;
    iddConfig.EvtIddCxAdapterCommitModes = NebulaAdapterCommitModes;
    iddConfig.EvtIddCxParseMonitorDescription = NebulaParseMonitorDescription;
    iddConfig.EvtIddCxMonitorGetDefaultDescriptionModes = NebulaMonitorGetDefaultModes;
    iddConfig.EvtIddCxMonitorQueryTargetModes = NebulaMonitorQueryModes;
    iddConfig.EvtIddCxMonitorAssignSwapChain = NebulaMonitorAssignSwapChain;
    iddConfig.EvtIddCxMonitorUnassignSwapChain = NebulaMonitorUnassignSwapChain;

    NTSTATUS status = IddCxDeviceInitConfig(deviceInit, &iddConfig);
    if (!NT_SUCCESS(status)) return status;

    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, DeviceContextWrapper);

    WDF_PNPPOWER_EVENT_CALLBACKS pnpCallbacks;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&pnpCallbacks);
    pnpCallbacks.EvtDeviceD0Entry = NebulaDeviceD0Entry;
    WdfDeviceInitSetPnpPowerEventCallbacks(deviceInit, &pnpCallbacks);

    WDFDEVICE device = nullptr;
    status = WdfDeviceCreate(&deviceInit, &attributes, &device);
    if (!NT_SUCCESS(status)) return status;

    status = IddCxDeviceInitialize(device);
    if (!NT_SUCCESS(status)) return status;

    auto* wrapper = WdfObjectGet_DeviceContextWrapper(device);
    wrapper->context = new AdapterContext();
    wrapper->context->wdfDevice = device;
    return STATUS_SUCCESS;
}

NTSTATUS NebulaDeviceD0Entry(WDFDEVICE device, WDF_POWER_DEVICE_STATE) {
    auto* wrapper = WdfObjectGet_DeviceContextWrapper(device);
    AdapterContext* ctx = wrapper->context;

    // Describe the virtual adapter to IddCx.
    IDDCX_ADAPTER_CAPS caps = {};
    caps.Size = sizeof(caps);
    caps.MaxMonitorsSupported = 4;
    caps.EndPointDiagnostics.Size = sizeof(caps.EndPointDiagnostics);
    caps.EndPointDiagnostics.GammaSupport = IDDCX_FEATURE_IMPLEMENTATION_NONE;
    caps.EndPointDiagnostics.TransmissionType = IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;

    static const WCHAR kFriendlyName[] = L"NebulaDisplay Virtual Adapter";
    static const WCHAR kManufacturer[] = L"NebulaDisplay Project";
    static const WCHAR kModel[] = L"Nebula Virtual Display";
    caps.EndPointDiagnostics.pEndPointFriendlyName = kFriendlyName;
    caps.EndPointDiagnostics.pEndPointManufacturerName = kManufacturer;
    caps.EndPointDiagnostics.pEndPointModelName = kModel;

    IDDCX_ENDPOINT_VERSION version = {};
    version.Size = sizeof(version);
    version.MajorVer = 0;
    version.MinorVer = 1;
    version.SKU = 0;
    caps.EndPointDiagnostics.pFirmwareVersion = &version;
    caps.EndPointDiagnostics.pHardwareVersion = &version;

    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, DeviceContextWrapper);

    IDARG_IN_ADAPTER_INIT adapterInit = {};
    adapterInit.WdfDevice = device;
    adapterInit.pCaps = &caps;
    adapterInit.ObjectAttributes = &attributes;

    IDARG_OUT_ADAPTER_INIT adapterOut = {};
    NTSTATUS status = IddCxAdapterInitAsync(&adapterInit, &adapterOut);
    if (NT_SUCCESS(status)) {
        ctx->adapter = adapterOut.AdapterObject;
        auto* adapterWrapper = WdfObjectGet_DeviceContextWrapper(adapterOut.AdapterObject);
        adapterWrapper->context = ctx;
    }
    return status;
}

NTSTATUS NebulaAdapterInitFinished(IDDCX_ADAPTER adapter,
                                   const IDARG_IN_ADAPTER_INIT_FINISHED* args) {
    auto* wrapper = WdfObjectGet_DeviceContextWrapper(adapter);
    if (!NT_SUCCESS(args->AdapterInitStatus)) return STATUS_SUCCESS;
    // Bring up virtual monitor 0. Additional monitors are hot-plugged on
    // demand (host service raises the count through the device interface in
    // a future revision; one always-on connector is v1 behavior, mirroring
    // how a docking station's port behaves when nothing is attached).
    wrapper->context->FinishInit(0);
    return STATUS_SUCCESS;
}

void nebula::AdapterContext::FinishInit(UINT connectorIndex) {
    auto monitorCtx = std::make_unique<MonitorContext>();
    monitorCtx->index = connectorIndex;

    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, MonitorContextWrapper);

    IDDCX_MONITOR_INFO monitorInfo = {};
    monitorInfo.Size = sizeof(monitorInfo);
    monitorInfo.MonitorType = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI;
    monitorInfo.ConnectorIndex = connectorIndex;
    // No hardware EDID: describe the monitor through default modes instead.
    monitorInfo.MonitorDescription.Size = sizeof(monitorInfo.MonitorDescription);
    monitorInfo.MonitorDescription.Type = IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
    monitorInfo.MonitorDescription.DataSize = 0;
    monitorInfo.MonitorDescription.pData = nullptr;

    // Stable container id so Windows remembers per-monitor settings.
    // {8f4f4dd0-9d24-4e2e-8e6f-8a5b1e6e0c01} — generated for this project.
    static const GUID kContainerId = {
        0x8f4f4dd0, 0x9d24, 0x4e2e, {0x8e, 0x6f, 0x8a, 0x5b, 0x1e, 0x6e, 0x0c, 0x01}};
    monitorInfo.MonitorContainerId = kContainerId;

    IDARG_IN_MONITORCREATE monitorCreate = {};
    monitorCreate.ObjectAttributes = &attributes;
    monitorCreate.pMonitorInfo = &monitorInfo;

    IDARG_OUT_MONITORCREATE monitorOut = {};
    NTSTATUS status = IddCxMonitorCreate(adapter, &monitorCreate, &monitorOut);
    if (!NT_SUCCESS(status)) return;

    monitorCtx->monitor = monitorOut.MonitorObject;
    monitorCtx->writer = std::make_shared<SharedFrameWriter>();
    monitorCtx->writer->Create(connectorIndex);
    monitorCtx->newFrameEvent = CreateEventW(nullptr, FALSE, FALSE, nullptr);

    auto* monitorWrapper = WdfObjectGet_MonitorContextWrapper(monitorOut.MonitorObject);
    monitorWrapper->context = monitorCtx.get();

    IDARG_OUT_MONITORARRIVAL arrivalOut = {};
    IddCxMonitorArrival(monitorOut.MonitorObject, &arrivalOut);

    monitors.push_back(std::move(monitorCtx));
}

// ---------------------------------------------------------------------------
// Mode enumeration
// ---------------------------------------------------------------------------

NTSTATUS NebulaAdapterCommitModes(IDDCX_ADAPTER, const IDARG_IN_COMMITMODES*) {
    // Mode state is fully derived from the swap-chain dimensions when it is
    // (re)assigned; nothing extra to track here.
    return STATUS_SUCCESS;
}

NTSTATUS NebulaParseMonitorDescription(const IDARG_IN_PARSEMONITORDESCRIPTION* inArgs,
                                       IDARG_OUT_PARSEMONITORDESCRIPTION* outArgs) {
    // We do not expose a hardware EDID; report zero modes so IddCx falls
    // back to EvtMonitorGetDefaultDescriptionModes.
    UNREFERENCED_PARAMETER(inArgs);
    outArgs->MonitorModeBufferOutputCount = 0;
    return STATUS_SUCCESS;
}

NTSTATUS NebulaMonitorGetDefaultModes(IDDCX_MONITOR,
                                      const IDARG_IN_GETDEFAULTDESCRIPTIONMODES* inArgs,
                                      IDARG_OUT_GETDEFAULTDESCRIPTIONMODES* outArgs) {
    const UINT32 total = ARRAYSIZE(kModes);
    outArgs->DefaultMonitorModeBufferOutputCount = total;
    if (inArgs->DefaultMonitorModeBufferInputCount == 0) {
        return STATUS_SUCCESS;  // size query
    }
    if (inArgs->DefaultMonitorModeBufferInputCount < total) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    for (UINT32 i = 0; i < total; i++) {
        const bool preferred = kModes[i].w == kPreferredMode.w &&
                               kModes[i].h == kPreferredMode.h &&
                               kModes[i].hz == kPreferredMode.hz;
        inArgs->pDefaultMonitorModes[i] = MakeMonitorMode(
            kModes[i], preferred ? IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR
                                 : IDDCX_MONITOR_MODE_ORIGIN_DRIVER);
    }
    outArgs->PreferredMonitorModeIdx = 4;  // 1920x1080@60 in kModes
    return STATUS_SUCCESS;
}

NTSTATUS NebulaMonitorQueryModes(IDDCX_MONITOR,
                                 const IDARG_IN_QUERYTARGETMODES* inArgs,
                                 IDARG_OUT_QUERYTARGETMODES* outArgs) {
    const UINT32 total = ARRAYSIZE(kModes);
    outArgs->TargetModeBufferOutputCount = total;
    if (inArgs->TargetModeBufferInputCount == 0) {
        return STATUS_SUCCESS;
    }
    if (inArgs->TargetModeBufferInputCount < total) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    for (UINT32 i = 0; i < total; i++) {
        inArgs->pTargetModes[i] = MakeTargetMode(kModes[i]);
    }
    return STATUS_SUCCESS;
}

// ---------------------------------------------------------------------------
// Swap-chain assignment
// ---------------------------------------------------------------------------

NTSTATUS NebulaMonitorAssignSwapChain(IDDCX_MONITOR monitor,
                                      const IDARG_IN_SETSWAPCHAIN* inArgs) {
    auto* wrapper = WdfObjectGet_MonitorContextWrapper(monitor);
    MonitorContext* ctx = wrapper->context;

    // Tear down any previous processor first (mode change path).
    ctx->processor.reset();

    auto device = std::make_shared<Direct3DDevice>();
    if (FAILED(device->Init(inArgs->RenderAdapterLuid))) {
        // Device creation can fail transiently during adapter resets;
        // reporting failure makes IddCx retry with a new swap-chain.
        return STATUS_GRAPHICS_DRIVER_MISMATCH;
    }

    ctx->processor = std::make_unique<SwapChainProcessor>(
        inArgs->hSwapChain, device, ctx->writer, ctx->newFrameEvent);
    return STATUS_SUCCESS;
}

NTSTATUS NebulaMonitorUnassignSwapChain(IDDCX_MONITOR monitor) {
    auto* wrapper = WdfObjectGet_MonitorContextWrapper(monitor);
    wrapper->context->processor.reset();
    return STATUS_SUCCESS;
}

// ---------------------------------------------------------------------------
// Direct3DDevice
// ---------------------------------------------------------------------------

HRESULT nebula::Direct3DDevice::Init(LUID luid) {
    adapterLuid = luid;
    HRESULT hr = CreateDXGIFactory2(0, IID_PPV_ARGS(&dxgiFactory));
    if (FAILED(hr)) return hr;
    hr = dxgiFactory->EnumAdapterByLuid(adapterLuid, IID_PPV_ARGS(&adapter));
    if (FAILED(hr)) return hr;
    return D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                             D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                             D3D11_SDK_VERSION, &device, nullptr, &context);
}

// ---------------------------------------------------------------------------
// SwapChainProcessor
// ---------------------------------------------------------------------------

nebula::SwapChainProcessor::SwapChainProcessor(IDDCX_SWAPCHAIN swapChain,
                                               std::shared_ptr<Direct3DDevice> device,
                                               std::shared_ptr<SharedFrameWriter> writer,
                                               HANDLE newFrameEvent)
    : m_hSwapChain(swapChain),
      m_device(std::move(device)),
      m_writer(std::move(writer)),
      m_hAvailableBufferEvent(newFrameEvent) {
    m_hTerminateEvent = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    m_hThread = CreateThread(nullptr, 0, RunThread, this, 0, nullptr);
}

nebula::SwapChainProcessor::~SwapChainProcessor() {
    if (m_hTerminateEvent) SetEvent(m_hTerminateEvent);
    if (m_hThread) {
        WaitForSingleObject(m_hThread, 3000);
        CloseHandle(m_hThread);
    }
    if (m_hTerminateEvent) CloseHandle(m_hTerminateEvent);
}

DWORD CALLBACK nebula::SwapChainProcessor::RunThread(LPVOID argument) {
    reinterpret_cast<SwapChainProcessor*>(argument)->Run();
    return 0;
}

void nebula::SwapChainProcessor::Run() {
    // Boost to multimedia scheduling for consistent frame pacing.
    DWORD avTask = 0;
    HANDLE avTaskHandle = AvSetMmThreadCharacteristicsW(L"Distribution", &avTask);
    RunCore();
    // Always flush pending work before the swap-chain is destroyed.
    WdfObjectDelete(reinterpret_cast<WDFOBJECT>(m_hSwapChain));
    m_hSwapChain = nullptr;
    if (avTaskHandle) AvRevertMmThreadCharacteristics(avTaskHandle);
}

void nebula::SwapChainProcessor::RunCore() {
    // Hand our D3D device to the swap-chain.
    IDARG_IN_SWAPCHAINSETDEVICE setDevice = {};
    ComPtr<IDXGIDevice> dxgiDevice;
    if (FAILED(m_device->device.As(&dxgiDevice))) return;
    setDevice.pDevice = dxgiDevice.Get();
    if (FAILED(IddCxSwapChainSetDevice(m_hSwapChain, &setDevice))) return;

    for (;;) {
        ComPtr<IDXGIResource> acquiredBuffer;
        IDARG_OUT_RELEASEANDACQUIREBUFFER bufferOut = {};
        HRESULT hr = IddCxSwapChainReleaseAndAcquireBuffer(m_hSwapChain, &bufferOut);

        if (hr == E_PENDING) {
            // No new buffer yet: wait for buffer-available or termination.
            HANDLE handles[] = {m_hAvailableBufferEvent, m_hTerminateEvent};
            DWORD wait = WaitForMultipleObjects(2, handles, FALSE, 17);
            if (wait == WAIT_OBJECT_0 + 1) break;  // terminate
            continue;
        }
        if (FAILED(hr)) break;

        acquiredBuffer.Attach(bufferOut.MetaData.pSurface);

        // Copy GPU surface -> staging -> shared memory.
        ComPtr<ID3D11Texture2D> texture;
        if (SUCCEEDED(acquiredBuffer.As(&texture))) {
            D3D11_TEXTURE2D_DESC desc = {};
            texture->GetDesc(&desc);

            if (!m_staging || m_stagingW != desc.Width || m_stagingH != desc.Height) {
                D3D11_TEXTURE2D_DESC stagingDesc = desc;
                stagingDesc.Usage = D3D11_USAGE_STAGING;
                stagingDesc.BindFlags = 0;
                stagingDesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
                stagingDesc.MiscFlags = 0;
                m_staging.Reset();
                if (SUCCEEDED(m_device->device->CreateTexture2D(&stagingDesc, nullptr,
                                                                &m_staging))) {
                    m_stagingW = desc.Width;
                    m_stagingH = desc.Height;
                }
            }

            if (m_staging) {
                m_device->context->CopyResource(m_staging.Get(), texture.Get());
                D3D11_MAPPED_SUBRESOURCE mapped = {};
                if (SUCCEEDED(m_device->context->Map(m_staging.Get(), 0, D3D11_MAP_READ, 0,
                                                     &mapped))) {
                    m_writer->WriteFrame(static_cast<const BYTE*>(mapped.pData), desc.Width,
                                         desc.Height, mapped.RowPitch);
                    m_device->context->Unmap(m_staging.Get(), 0);
                }
            }
        }

        acquiredBuffer.Reset();
        if (FAILED(IddCxSwapChainFinishedProcessingFrame(m_hSwapChain))) break;

        if (WaitForSingleObject(m_hTerminateEvent, 0) == WAIT_OBJECT_0) break;
    }
}

// ---------------------------------------------------------------------------
// SharedFrameWriter
// ---------------------------------------------------------------------------

HRESULT nebula::SharedFrameWriter::Create(UINT32 monitorIndex) {
    WCHAR sectionName[64];
    WCHAR eventName[64];
    swprintf_s(sectionName, L"Global\\NebulaDisplay.Frame.%u", monitorIndex);
    swprintf_s(eventName, L"Global\\NebulaDisplay.FrameReady.%u", monitorIndex);

    const SIZE_T size = sizeof(SharedFrameHeader) +
                        static_cast<SIZE_T>(kMaxWidth) * kMaxHeight * 4;
    m_section = CreateFileMappingW(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE,
                                   static_cast<DWORD>(size >> 32),
                                   static_cast<DWORD>(size & 0xFFFFFFFF), sectionName);
    if (!m_section) return HRESULT_FROM_WIN32(GetLastError());

    m_view = static_cast<BYTE*>(MapViewOfFile(m_section, FILE_MAP_WRITE, 0, 0, 0));
    if (!m_view) {
        Close();
        return HRESULT_FROM_WIN32(GetLastError());
    }

    m_event = CreateEventW(nullptr, FALSE, FALSE, eventName);
    if (!m_event) {
        Close();
        return HRESULT_FROM_WIN32(GetLastError());
    }

    auto* header = reinterpret_cast<SharedFrameHeader*>(m_view);
    header->magic = kFrameMagic;
    header->version = kFrameVersion;
    header->width = 0;
    header->height = 0;
    header->stride = 0;
    header->format = kFormatBgra8;
    header->frameSeq = 0;
    return S_OK;
}

void nebula::SharedFrameWriter::Close() {
    if (m_view) {
        UnmapViewOfFile(m_view);
        m_view = nullptr;
    }
    if (m_section) {
        CloseHandle(m_section);
        m_section = nullptr;
    }
    if (m_event) {
        CloseHandle(m_event);
        m_event = nullptr;
    }
}

void nebula::SharedFrameWriter::WriteFrame(const BYTE* src, UINT32 width, UINT32 height,
                                           UINT32 srcPitch) {
    if (!m_view || width == 0 || height > kMaxHeight || width > kMaxWidth) return;

    auto* header = reinterpret_cast<SharedFrameHeader*>(m_view);
    BYTE* dst = m_view + sizeof(SharedFrameHeader);
    const UINT32 dstPitch = width * 4;

    for (UINT32 row = 0; row < height; row++) {
        memcpy(dst + static_cast<SIZE_T>(row) * dstPitch,
               src + static_cast<SIZE_T>(row) * srcPitch, dstPitch);
    }

    header->width = width;
    header->height = height;
    header->stride = dstPitch;
    // Publish the sequence number last (release ordering via Interlocked).
    InterlockedExchange64(reinterpret_cast<volatile LONG64*>(&header->frameSeq),
                          static_cast<LONG64>(++m_seq));
    if (m_event) SetEvent(m_event);
}

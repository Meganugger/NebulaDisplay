// NebulaDisplay Indirect Display Driver (IddCx / UMDF2)
//
// Creates real virtual monitors that Windows treats exactly like physical
// displays (they appear in Display Settings, support Extend/Duplicate, DPI,
// orientation, HDR-awareness through mode lists, etc).
//
// Frame flow:
//   Windows composes the virtual monitor's desktop
//     -> IddCx swap-chain (this driver acquires buffers on a worker thread)
//     -> CPU staging copy
//     -> named shared-memory section + "frame ready" event
//     -> nebula-host service (VirtualMonitorSource) encodes & streams.
//
// Clean-room note: written from Microsoft's public IddCx documentation and
// the MIT-licensed Microsoft indirect display sample's architectural
// patterns. No third-party proprietary code.

#pragma once

#include <windows.h>
#include <bugcodes.h>
#include <wudfwdm.h>
#include <wdf.h>
#include <iddcx.h>

#include <dxgi1_5.h>
#include <d3d11_2.h>
#include <avrt.h>

#include <memory>
#include <vector>
#include <atomic>

namespace nebula {

// ---------------------------------------------------------------------------
// Shared-memory frame export (contract shared with the host service;
// see crates/nebula-host/src/capture/idd.rs)
// ---------------------------------------------------------------------------

constexpr UINT32 kFrameMagic = 0x4E444653;  // "NDFS"
constexpr UINT32 kFrameVersion = 1;
constexpr UINT32 kFormatBgra8 = 1;
constexpr UINT32 kMaxWidth = 3840;
constexpr UINT32 kMaxHeight = 2160;

#pragma pack(push, 1)
struct SharedFrameHeader {
    UINT32 magic;
    UINT32 version;
    UINT32 width;
    UINT32 height;
    UINT32 stride;
    UINT32 format;
    UINT64 frameSeq;
};
#pragma pack(pop)

static_assert(sizeof(SharedFrameHeader) == 32, "header layout is a wire contract");

// Writes frames into the named section and pulses the frame-ready event.
class SharedFrameWriter {
public:
    ~SharedFrameWriter() { Close(); }

    HRESULT Create(UINT32 monitorIndex);
    void Close();
    // Copies one BGRA frame (srcPitch bytes per row) into the section.
    void WriteFrame(const BYTE* src, UINT32 width, UINT32 height, UINT32 srcPitch);

private:
    HANDLE m_section = nullptr;
    HANDLE m_event = nullptr;
    BYTE* m_view = nullptr;
    UINT64 m_seq = 0;
};

// ---------------------------------------------------------------------------
// Swap-chain processing
// ---------------------------------------------------------------------------

// Owns the D3D11 device used to read swap-chain buffers.
struct Direct3DDevice {
    HRESULT Init(LUID adapterLuid);

    LUID adapterLuid{};
    Microsoft::WRL::ComPtr<IDXGIFactory5> dxgiFactory;
    Microsoft::WRL::ComPtr<IDXGIAdapter1> adapter;
    Microsoft::WRL::ComPtr<ID3D11Device> device;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
};

// Dedicated thread that acquires swap-chain buffers and exports them.
class SwapChainProcessor {
public:
    SwapChainProcessor(IDDCX_SWAPCHAIN swapChain,
                       std::shared_ptr<Direct3DDevice> device,
                       std::shared_ptr<SharedFrameWriter> writer,
                       HANDLE newFrameEvent);
    ~SwapChainProcessor();

private:
    static DWORD CALLBACK RunThread(LPVOID argument);
    void Run();
    void RunCore();

    IDDCX_SWAPCHAIN m_hSwapChain;
    std::shared_ptr<Direct3DDevice> m_device;
    std::shared_ptr<SharedFrameWriter> m_writer;
    HANDLE m_hAvailableBufferEvent;
    HANDLE m_hTerminateEvent = nullptr;
    HANDLE m_hThread = nullptr;

    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_staging;
    UINT m_stagingW = 0;
    UINT m_stagingH = 0;
};

// ---------------------------------------------------------------------------
// Monitor / adapter context
// ---------------------------------------------------------------------------

struct MonitorContext {
    IDDCX_MONITOR monitor = nullptr;
    UINT32 index = 0;
    std::shared_ptr<SharedFrameWriter> writer;
    std::unique_ptr<SwapChainProcessor> processor;
    HANDLE newFrameEvent = nullptr;
};

struct AdapterContext {
    IDDCX_ADAPTER adapter = nullptr;
    WDFDEVICE wdfDevice = nullptr;
    std::vector<std::unique_ptr<MonitorContext>> monitors;

    void FinishInit(UINT connectorIndex);
};

struct DeviceContextWrapper {
    AdapterContext* context;
    ~DeviceContextWrapper() { delete context; }
};

struct MonitorContextWrapper {
    MonitorContext* context;
};

}  // namespace nebula

WDF_DECLARE_CONTEXT_TYPE(nebula::DeviceContextWrapper);
WDF_DECLARE_CONTEXT_TYPE(nebula::MonitorContextWrapper);

// WDF / IddCx callbacks.
extern "C" DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD NebulaDeviceAdd;
EVT_WDF_DEVICE_D0_ENTRY NebulaDeviceD0Entry;

EVT_IDD_CX_ADAPTER_INIT_FINISHED NebulaAdapterInitFinished;
EVT_IDD_CX_ADAPTER_COMMIT_MODES NebulaAdapterCommitModes;
EVT_IDD_CX_PARSE_MONITOR_DESCRIPTION NebulaParseMonitorDescription;
EVT_IDD_CX_MONITOR_GET_DEFAULT_DESCRIPTION_MODES NebulaMonitorGetDefaultModes;
EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES NebulaMonitorQueryModes;
EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN NebulaMonitorAssignSwapChain;
EVT_IDD_CX_MONITOR_UNASSIGN_SWAPCHAIN NebulaMonitorUnassignSwapChain;

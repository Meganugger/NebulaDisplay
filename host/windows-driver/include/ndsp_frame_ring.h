// NebulaDisplay virtual display driver — shared frame-transport contract.
//
// The driver (UMDF/IddCx, session 0) and the nebulad service exchange frames
// through a named shared-memory ring + events, one pair per virtual monitor.
// This header is the single source of truth for that ABI;
// host/service/src/capture/windows_idd.rs mirrors it field-for-field (keep
// in sync — bump NDSP_RING_VERSION on any layout change).

#pragma once
#include <cstdint>
#include <cwchar>

namespace ndsp {

constexpr uint32_t NDSP_RING_MAGIC = 0x4E445352;   // "NDSR"
constexpr uint32_t NDSP_RING_VERSION = 2;          // v2: per-monitor rings
constexpr uint32_t NDSP_RING_SLOTS = 3;            // triple buffer
constexpr uint32_t NDSP_MAX_WIDTH = 4096;
constexpr uint32_t NDSP_MAX_HEIGHT = 2304;
constexpr uint32_t NDSP_MAX_MONITORS = 4;

// Object name patterns; %u = monitor index (0-based). Local\ scope: both the
// driver (running as the logged-on user's UMDF host) and the service session
// can open them; the installer documents running nebulad in that session.
constexpr const wchar_t* NDSP_RING_NAME_FMT = L"Local\\NebulaDisplay.FrameRing.v2.%u";
constexpr const wchar_t* NDSP_FRAME_EVENT_FMT = L"Local\\NebulaDisplay.FrameReady.v2.%u";

// Format a ring/event name for a monitor index into `out` (>= 64 wchars).
inline void ring_name(uint32_t index, wchar_t* out, size_t cap) {
    swprintf(out, cap, NDSP_RING_NAME_FMT, index);
}
inline void frame_event_name(uint32_t index, wchar_t* out, size_t cap) {
    swprintf(out, cap, NDSP_FRAME_EVENT_FMT, index);
}

#pragma pack(push, 8)
struct SlotHeader {
    // Sequence protocol (seqlock): odd while the producer is writing.
    volatile uint32_t seq;
    uint32_t width;
    uint32_t height;
    uint32_t pitch_bytes;      // bytes per row in the slot payload
    uint64_t timestamp_qpc;    // QueryPerformanceCounter at present time
    uint64_t frame_number;
};

struct RingHeader {
    uint32_t magic;            // NDSP_RING_MAGIC
    uint32_t version;          // NDSP_RING_VERSION
    uint32_t slots;            // NDSP_RING_SLOTS
    uint32_t slot_stride;      // bytes between slot payloads
    volatile uint32_t latest_slot;   // index of the most recently completed slot
    volatile uint32_t connected;     // driver sets 1 when a monitor is attached
    uint32_t width;            // current mode
    uint32_t height;
    uint32_t refresh_hz;
    uint32_t monitor_index;    // which virtual monitor this ring belongs to
    uint32_t reserved[6];
    SlotHeader slot_headers[NDSP_RING_SLOTS];
    // Payloads follow at: sizeof(RingHeader) + i * slot_stride  (BGRA8)
};
#pragma pack(pop)

constexpr uint64_t ring_payload_bytes() {
    return static_cast<uint64_t>(NDSP_MAX_WIDTH) * NDSP_MAX_HEIGHT * 4;
}

constexpr uint64_t ring_total_bytes() {
    return sizeof(RingHeader) + NDSP_RING_SLOTS * ring_payload_bytes();
}

} // namespace ndsp

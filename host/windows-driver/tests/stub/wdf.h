
#pragma once
struct WDFDEVICE__; typedef WDFDEVICE__* WDFDEVICE;
struct WDFDRIVER__; typedef WDFDRIVER__* WDFDRIVER;
struct WDFOBJECT__; typedef WDFOBJECT__* WDFOBJECT;
struct WDFKEY__; typedef WDFKEY__* WDFKEY;
struct WDFSTRING__; typedef WDFSTRING__* WDFSTRING;
struct WDFCOLLECTION__; typedef WDFCOLLECTION__* WDFCOLLECTION;
struct WDFDEVICE_INIT; typedef WDFDEVICE_INIT* PWDFDEVICE_INIT;
struct _DRIVER_OBJECT; typedef _DRIVER_OBJECT* PDRIVER_OBJECT;
enum WDF_POWER_DEVICE_STATE { WdfPowerDeviceD0 = 1 };
typedef NTSTATUS DRIVER_INITIALIZE(PDRIVER_OBJECT, PUNICODE_STRING);
typedef NTSTATUS EVT_WDF_DRIVER_DEVICE_ADD(WDFDRIVER, PWDFDEVICE_INIT);
typedef NTSTATUS EVT_WDF_DEVICE_D0_ENTRY(WDFDEVICE, WDF_POWER_DEVICE_STATE);
typedef void (*PFN_WDF_OBJECT_CONTEXT_CLEANUP)(WDFOBJECT);
struct WDF_DRIVER_CONFIG { EVT_WDF_DRIVER_DEVICE_ADD* EvtDriverDeviceAdd; };
inline void WDF_DRIVER_CONFIG_INIT(WDF_DRIVER_CONFIG* c, EVT_WDF_DRIVER_DEVICE_ADD* f){c->EvtDriverDeviceAdd=f;}
struct WDF_OBJECT_ATTRIBUTES { PFN_WDF_OBJECT_CONTEXT_CLEANUP EvtCleanupCallback; const struct WDF_OBJECT_CONTEXT_TYPE_INFO* ContextTypeInfo; };
struct WDF_OBJECT_CONTEXT_TYPE_INFO { const char* ContextName; size_t ContextSize; };
#define WDF_NO_OBJECT_ATTRIBUTES nullptr
#define WDF_NO_HANDLE nullptr
struct WDF_PNPPOWER_EVENT_CALLBACKS { EVT_WDF_DEVICE_D0_ENTRY* EvtDeviceD0Entry; };
inline void WDF_PNPPOWER_EVENT_CALLBACKS_INIT(WDF_PNPPOWER_EVENT_CALLBACKS* c){c->EvtDeviceD0Entry=nullptr;}
inline void WdfDeviceInitSetPnpPowerEventCallbacks(PWDFDEVICE_INIT, WDF_PNPPOWER_EVENT_CALLBACKS*){}
inline NTSTATUS WdfDriverCreate(PDRIVER_OBJECT, PUNICODE_STRING, WDF_OBJECT_ATTRIBUTES*, WDF_DRIVER_CONFIG*, WDFDRIVER*){return 0;}
inline NTSTATUS WdfDeviceCreate(PWDFDEVICE_INIT*, WDF_OBJECT_ATTRIBUTES*, WDFDEVICE*){return 0;}
// Context machinery (approximates the real macros closely enough to catch misuse)
// Real WDF handles are all interchangeable "opaque object handles"; model
// that with a handle-generic template.
#define WDF_DECLARE_CONTEXT_TYPE(T) \
  template<class H> inline T* WdfObjectGet_##T(H){ static thread_local char buf[sizeof(T)]; return (T*)buf; } \
  extern const WDF_OBJECT_CONTEXT_TYPE_INFO WDF_##T##_TYPE_INFO;
#define WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(a, T) do { (a)->EvtCleanupCallback=nullptr; } while(0)
inline NTSTATUS WdfDeviceOpenRegistryKey(WDFDEVICE, ULONG, ULONG, WDF_OBJECT_ATTRIBUTES*, WDFKEY*){return -1;}
inline void WdfRegistryClose(WDFKEY){}
inline NTSTATUS WdfRegistryQueryULong(WDFKEY, const UNICODE_STRING*, ULONG*){return -1;}
inline NTSTATUS WdfRegistryQueryMultiString(WDFKEY, const UNICODE_STRING*, WDF_OBJECT_ATTRIBUTES*, WDFCOLLECTION){return -1;}
inline NTSTATUS WdfCollectionCreate(WDF_OBJECT_ATTRIBUTES*, WDFCOLLECTION*){return 0;}
inline ULONG WdfCollectionGetCount(WDFCOLLECTION){return 0;}
inline WDFOBJECT WdfCollectionGetItem(WDFCOLLECTION, ULONG){return nullptr;}
inline void WdfStringGetUnicodeString(WDFSTRING, UNICODE_STRING*){}
inline void WdfObjectDelete(WDFOBJECT){}
inline void WdfObjectDelete(WDFCOLLECTION){}
#define DECLARE_CONST_UNICODE_STRING(n, s) const UNICODE_STRING n = { sizeof(s)-2, sizeof(s), const_cast<PWSTR>(s) }

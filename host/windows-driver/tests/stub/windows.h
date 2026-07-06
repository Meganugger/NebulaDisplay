
#pragma once
#include <cstdint>
#include <cstring>
#include <cstdio>
#include <cwchar>
typedef uint8_t BYTE; typedef uint16_t UINT16; typedef uint32_t UINT32; typedef uint64_t UINT64;
typedef int32_t LONG; typedef uint32_t ULONG; typedef uint32_t DWORD; typedef int BOOL;
typedef unsigned int UINT; typedef wchar_t WCHAR; typedef void* HANDLE; typedef void* LPVOID;
typedef void* PVOID; typedef long HRESULT;
#define TRUE 1
#define FALSE 0
typedef long NTSTATUS; typedef unsigned short USHORT;
typedef const wchar_t* PCWSTR; typedef wchar_t* PWSTR;
struct LUID { DWORD LowPart; LONG HighPart; };
struct GUID { uint32_t Data1; uint16_t Data2; uint16_t Data3; uint8_t Data4[8]; };
typedef GUID IID; 
#define REFIID const IID&
union LARGE_INTEGER { struct { DWORD LowPart; LONG HighPart; } u; int64_t QuadPart; };
struct UNICODE_STRING { USHORT Length; USHORT MaximumLength; PWSTR Buffer; };
typedef UNICODE_STRING* PUNICODE_STRING;
struct POINTL { LONG x; LONG y; };
#define STATUS_SUCCESS ((NTSTATUS)0)
#define STATUS_BUFFER_TOO_SMALL ((NTSTATUS)0xC0000023L)
#define NT_SUCCESS(s) ((s) >= 0)
#define S_OK ((HRESULT)0)
#define E_PENDING ((HRESULT)0x8000000AL)
#define SUCCEEDED(hr) ((hr) >= 0)
#define FAILED(hr) ((hr) < 0)
#define HRESULT_FROM_WIN32(x) ((HRESULT)(x) <= 0 ? (HRESULT)(x) : (HRESULT)(((x) & 0xFFFF) | 0x80070000))
#define CALLBACK
#define WINAPI
#define INVALID_HANDLE_VALUE ((HANDLE)(intptr_t)-1)
#define PAGE_READWRITE 0x04
#define FILE_MAP_ALL_ACCESS 0xF001F
#define WAIT_OBJECT_0 0
#define WAIT_TIMEOUT 258
#define INFINITE 0xFFFFFFFF
#define KEY_READ 0x20019
#define PLUGPLAY_REGKEY_DEVICE 1
#define IID_PPV_ARGS(p) __uuidof(**(p)), (void**)(p)
template<class T> const IID& __uuidof_helper();
#define __uuidof(x) (*(const IID*)nullptr)
inline DWORD GetLastError() { return 0; }
inline HANDLE CreateFileMappingW(HANDLE,void*,DWORD,DWORD,DWORD,PCWSTR){return nullptr;}
inline void* MapViewOfFile(HANDLE,DWORD,DWORD,DWORD,size_t){return nullptr;}
inline BOOL UnmapViewOfFile(const void*){return 1;}
inline BOOL CloseHandle(HANDLE){return 1;}
inline HANDLE CreateEventW(void*,BOOL,BOOL,PCWSTR){return nullptr;}
inline BOOL SetEvent(HANDLE){return 1;}
inline HANDLE CreateThread(void*,size_t,DWORD(CALLBACK*)(LPVOID),LPVOID,DWORD,DWORD*){return nullptr;}
inline DWORD WaitForSingleObject(HANDLE,DWORD){return 0;}
inline DWORD WaitForMultipleObjects(DWORD,const HANDLE*,BOOL,DWORD){return 0;}
inline void MemoryBarrier(){}
inline BOOL QueryPerformanceCounter(LARGE_INTEGER* p){p->QuadPart=0;return 1;}
template<class T> T min(T a, T b){return a<b?a:b;}
template<class T, class U> T min(T a, U b){return a<(T)b?a:(T)b;}

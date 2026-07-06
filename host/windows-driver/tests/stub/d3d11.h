
#pragma once
#include "windows.h"
struct IUnknown { virtual ULONG AddRef()=0; virtual ULONG Release()=0; virtual HRESULT QueryInterface(REFIID, void**)=0; };
enum D3D_DRIVER_TYPE { D3D_DRIVER_TYPE_UNKNOWN=0, D3D_DRIVER_TYPE_HARDWARE=1, D3D_DRIVER_TYPE_WARP=5 };
enum D3D_FEATURE_LEVEL { D3D_FEATURE_LEVEL_11_0 = 0xb000 };
enum D3D11_USAGE { D3D11_USAGE_DEFAULT=0, D3D11_USAGE_STAGING=3 };
#define D3D11_CREATE_DEVICE_BGRA_SUPPORT 0x20
#define D3D11_CPU_ACCESS_READ 0x20000
#define D3D11_SDK_VERSION 7
enum D3D11_MAP { D3D11_MAP_READ = 1 };
enum DXGI_FORMAT { DXGI_FORMAT_B8G8R8A8_UNORM = 87 };
struct DXGI_SAMPLE_DESC { UINT Count; UINT Quality; };
struct D3D11_TEXTURE2D_DESC { UINT Width; UINT Height; UINT MipLevels; UINT ArraySize; DXGI_FORMAT Format; DXGI_SAMPLE_DESC SampleDesc; D3D11_USAGE Usage; UINT BindFlags; UINT CPUAccessFlags; UINT MiscFlags; };
struct D3D11_MAPPED_SUBRESOURCE { void* pData; UINT RowPitch; UINT DepthPitch; };
struct ID3D11Resource : IUnknown {};
struct ID3D11Texture2D : ID3D11Resource { virtual void GetDesc(D3D11_TEXTURE2D_DESC*)=0; };
struct ID3D11Device : IUnknown { virtual HRESULT CreateTexture2D(const D3D11_TEXTURE2D_DESC*, const void*, ID3D11Texture2D**)=0; };
struct ID3D11DeviceContext : IUnknown {
  virtual void CopyResource(ID3D11Resource*, ID3D11Resource*)=0;
  virtual HRESULT Map(ID3D11Resource*, UINT, D3D11_MAP, UINT, D3D11_MAPPED_SUBRESOURCE*)=0;
  virtual void Unmap(ID3D11Resource*, UINT)=0;
};
struct IDXGIObject : IUnknown {};
struct IDXGIDevice : IDXGIObject {};
struct IDXGIAdapter : IDXGIObject {};
struct IDXGIAdapter1 : IDXGIAdapter {};
struct IDXGIResource : IDXGIObject {};
struct IDXGIFactory : IDXGIObject {};
struct IDXGIFactory1 : IDXGIFactory {};
struct IDXGIFactory2 : IDXGIFactory1 {};
struct IDXGIFactory3 : IDXGIFactory2 {};
struct IDXGIFactory4 : IDXGIFactory3 { virtual HRESULT EnumAdapterByLuid(LUID, REFIID, void**)=0; };
inline HRESULT D3D11CreateDevice(IDXGIAdapter*, D3D_DRIVER_TYPE, void*, UINT, const D3D_FEATURE_LEVEL*, UINT, UINT, ID3D11Device**, D3D_FEATURE_LEVEL*, ID3D11DeviceContext**){return 0;}
inline HRESULT CreateDXGIFactory2(UINT, REFIID, void**){return 0;}

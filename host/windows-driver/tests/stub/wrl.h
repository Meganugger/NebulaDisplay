
#pragma once
#include "windows.h"
namespace Microsoft { namespace WRL {
template<class T> class ComPtr;
namespace Details {
template<class T> struct ComPtrRef {
  ComPtr<T>* ptr;
  operator typename T::element_type**() const { return ptr->GetAddressOf(); }
  operator void**() const { return (void**)ptr->GetAddressOf(); }
};
}
// Simplified but call-compatible model of WRL::ComPtr.
template<class T> class ComPtr {
  T* p = nullptr;
public:
  typedef T element_type;
  ComPtr() = default;
  ComPtr(T* q): p(q) {}
  T* Get() const { return p; }
  T** GetAddressOf() { return &p; }
  // operator& in real WRL releases + returns ComPtrRef convertible to T**/void**.
  T** operator&() { return &p; }
  T* operator->() const { return p; }
  explicit operator bool() const { return p != nullptr; }
  void Reset() { p = nullptr; }
  void Attach(T* q) { p = q; }
  template<class U> HRESULT As(ComPtr<U>* out) const { out->Attach((U*)(void*)p); return 0; }
  template<class U> HRESULT As(U** out) const { *out = (U*)(void*)p; return 0; }
};
}}

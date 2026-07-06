#pragma once
#include "windows.h"
inline HANDLE AvSetMmThreadCharacteristicsW(PCWSTR, DWORD*){return nullptr;}
inline BOOL AvRevertMmThreadCharacteristics(HANDLE){return 1;}

@echo off
rem Build the NebulaDisplay tray companion. Run from a VS Developer prompt.
cl /nologo /O2 /W4 /EHsc /DUNICODE /D_UNICODE tray.cpp ^
   /Fe:NebulaDisplayTray.exe ^
   /link shell32.lib advapi32.lib user32.lib /SUBSYSTEM:WINDOWS
if %errorlevel% neq 0 exit /b %errorlevel%
echo Built NebulaDisplayTray.exe

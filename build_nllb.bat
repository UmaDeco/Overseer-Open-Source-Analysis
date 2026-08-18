@echo off
setlocal
rem Build the unified DLL WITH the in-process NLLB engine. Needs the VS C++ env (cl.exe) + CMake +
rem Ninja for the vendored CTranslate2 build. +crt-static comes from native\.cargo\config.toml.
rem
rem Nothing here is tied to one machine: the repo is found relative to this file (%~dp0) and Visual
rem Studio is located with vswhere.exe, which every VS 2017+ installer drops in a fixed location. So
rem any edition (Community/Professional/Enterprise/BuildTools), any version, any drive works.

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" set "VSWHERE=%ProgramFiles%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" (
    echo ERROR: vswhere.exe not found - install Visual Studio or the C++ Build Tools.
    exit /b 1
)

rem -latest plus the C++ toolset component: the newest install that can actually compile CTranslate2.
set "VSDIR="
for /f "usebackq tokens=*" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSDIR=%%i"
if not defined VSDIR (
    echo ERROR: no Visual Studio install carries the C++ toolset ^(VC.Tools.x86.x64^).
    exit /b 1
)
echo === visual studio: %VSDIR% ===

call "%VSDIR%\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 (
    echo ERROR: vcvars64.bat failed.
    exit /b 1
)

rem VS ships CMake + Ninja inside the install; prepend them so a stray system CMake can't win.
set "VSCMAKE=%VSDIR%\Common7\IDE\CommonExtensions\Microsoft\CMake"
set "PATH=%VSCMAKE%\CMake\bin;%VSCMAKE%\Ninja;%USERPROFILE%\.cargo\bin;%PATH%"
set "CMAKE_GENERATOR=Ninja"
set "CMAKE_POLICY_VERSION_MINIMUM=3.5"

cd /d "%~dp0native"
echo === toolchain ===
where cl
where cmake
where ninja
where cargo
echo === building (release + nllb) ===
cargo build --release --features nllb
echo === EXIT %ERRORLEVEL% ===

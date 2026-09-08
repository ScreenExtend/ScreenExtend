@echo off
REM Regenerate the NVENC layout assertions. Run from src-tauri\tools on a Windows box
REM with the MSVC Build Tools installed; see README.md for the full recipe.
setlocal
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
if errorlevel 1 (
  echo could not enter the MSVC x64 environment - check the vcvars64.bat path above
  exit /b 1
)
cl /nologo /TC /W3 /I ..\resources\nvenc-headers nvenc_layout_gen.c /Fe:nvenc_layout_gen.exe || exit /b 1
"%CD%\nvenc_layout_gen.exe" > nvenc_layout_generated.txt || exit /b 1
echo wrote nvenc_layout_generated.txt
endlocal

@echo off
cd /d "%~dp0"
if exist "voxara.exe" (
    start "" "voxara.exe"
) else if exist "target\release\voxara.exe" (
    start "" "target\release\voxara.exe"
) else (
    echo Mengompilasi Voxara Studio...
    "%USERPROFILE%\.cargo\bin\cargo.exe" build --release
    if exist "target\release\voxara.exe" (
        copy /Y "target\release\voxara.exe" "voxara.exe" >nul
        start "" "voxara.exe"
    ) else (
        echo Gagal menjalankan kompilasi. Pastikan Rust terpasang.
        pause
    )
)

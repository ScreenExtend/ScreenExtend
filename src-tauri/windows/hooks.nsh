!macro NSIS_HOOK_PREUNINSTALL
  ${If} $UpdateMode <> 1
    DetailPrint "Removing ScreenExtend virtual display driver and certificate..."
    ExecShellWait "runas" "$INSTDIR\ScreenExtend.exe" "removedrivers" SW_HIDE
  ${EndIf}
!macroend

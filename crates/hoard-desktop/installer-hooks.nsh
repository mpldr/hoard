; hoardd (ADR 0021) sigue vivo tras cerrar la app — install/uninstall pisan su
; propio .exe en AppData\Local\Hoard mientras el proceso lo tiene abierto, y
; NSIS falla con "Error opening file for writing". Tauri solo cierra el exe
; principal por su cuenta; los sidecars hay que matarlos a mano aquí.
!macro NSIS_HOOK_PREINSTALL
  ExecWait 'taskkill /F /IM hoardd.exe'
  ExecWait 'taskkill /F /IM hoard-screen.exe'
  ExecWait 'taskkill /F /IM hoard-desktop.exe'
  Sleep 500
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ExecWait 'taskkill /F /IM hoardd.exe'
  ExecWait 'taskkill /F /IM hoard-screen.exe'
  ExecWait 'taskkill /F /IM hoard-desktop.exe'
  Sleep 500
!macroend

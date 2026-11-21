; Installer hooks wired via bundle.windows.nsis.installerHooks in
; tauri.conf.json. Tauri's NSIS template hands only the main binary to the
; Windows Restart Manager; the bundled daemon (jcode.exe) is invisible to
; that check and would keep the install directory's files in use across an
; upgrade or uninstall. Kill it (and the GUI as a backstop) best-effort
; before the template's own app check.
;
; GUI binary names: the NSIS bundle installs the main binary under its
; cargo package name (jcode-cowork-gui.exe), while the MSI/WiX bundle and
; the product branding use the productName ("jcode Cowork.exe"). Kill both
; so an uninstaller always reaches the GUI of either install flavor. When
; taskkill lacks permission against an instance running elevated, the
; template's "Insufficient permissions to close ..." message still guides
; the user to close it.

!macro NSIS_HOOK_PREINSTALL
  nsExec::Exec 'taskkill /F /T /IM "jcode.exe"'
  Pop $0
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec 'taskkill /F /T /IM "jcode.exe"'
  Pop $0
  nsExec::Exec 'taskkill /F /T /IM "jcode-cowork-gui.exe"'
  Pop $0
  nsExec::Exec 'taskkill /F /T /IM "jcode Cowork.exe"'
  Pop $0
!macroend

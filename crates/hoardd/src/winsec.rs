//! ACL del named pipe en Windows: sólo este usuario (y SYSTEM).
//!
//! Sin descriptor de seguridad explícito, el DACL por defecto de un named pipe
//! da control total al creador, a los administradores y a LocalSystem, **y
//! lectura al grupo `Everyone`**. Por ese pipe viajan nombres de juego, rutas
//! locales y los comandos del sync, así que "lectura para todo el mundo" no vale:
//! la ADR 0021 pide permisos solo-usuario (0600 en unix / ACL aquí).
//!
//! El descriptor se construye desde SDDL con el **SID del usuario actual**, no
//! con el alias `OW` (CREATOR OWNER). `OW` funcionaría en la mayoría de los
//! casos, pero su resolución depende de que el dueño del objeto sea quien lo
//! creó; el SID explícito no depende de nada. `OW` se queda como plan B para el
//! caso raro de no poder leer el token, y entonces se dice en el log — degradar
//! en silencio a un pipe abierto es justo lo que no queremos.

use std::ffi::c_void;
use std::ptr;

use anyhow::{bail, Result};
use windows_sys::core::{PCWSTR, PWSTR};
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Descriptor de seguridad "sólo yo y SYSTEM", listo para pasárselo a
/// `CreateNamedPipe`. Dueño de su memoria (la libera con `LocalFree`).
pub struct SecurityDescriptor {
    psd: PSECURITY_DESCRIPTOR,
}

// SAFETY: tras construirse, el descriptor es de sólo lectura para nosotros —
// sólo se lo pasamos a `CreateNamedPipe`, que lo copia al objeto. Nadie lo
// muta, así que compartirlo entre hilos (el `Listener` vive en una task de
// tokio) es seguro.
unsafe impl Send for SecurityDescriptor {}
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    /// `D:P(A;;GA;;;<sid>)(A;;GA;;;SY)`: DACL protegido (`P`, sin herencia),
    /// acceso total (`GA`) para el usuario actual y para LocalSystem, y **nada**
    /// para nadie más. Que `Everyone` no aparezca es el punto.
    pub fn user_only() -> Result<Self> {
        let sddl = match current_user_sid() {
            Ok(sid) => format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)"),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "hoardd: couldn't read this process's user SID; falling back to the \
                     CREATOR OWNER ACE for the pipe ACL"
                );
                "D:P(A;;GA;;;OW)(A;;GA;;;SY)".to_string()
            }
        };
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `wide` es una cadena UTF-16 terminada en NUL viva durante la
        // llamada, y `psd` un puntero de salida válido.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr() as PCWSTR,
                SDDL_REVISION_1,
                &mut psd,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            bail!("building the pipe security descriptor from SDDL failed: {err}");
        }
        Ok(Self { psd })
    }

    /// `SECURITY_ATTRIBUTES` apuntando a este descriptor. El llamante la pasa
    /// por puntero a `CreateNamedPipe` y no debe sobrevivir a `self`.
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.psd,
            // El pipe no se hereda: un proceso hijo (un instalador, un juego
            // lanzado desde la app) no tiene por qué heredar el handle del
            // canal de control del sync.
            bInheritHandle: 0,
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.psd.is_null() {
            // SAFETY: `psd` lo asignó `ConvertStringSecurityDescriptorToSecurityDescriptorW`,
            // que documenta `LocalFree` como su liberador.
            unsafe { LocalFree(self.psd as HLOCAL) };
        }
    }
}

/// Handle del token, cerrado al salir del scope.
struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: handle válido obtenido de `OpenProcessToken`.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// SID del usuario de este proceso, en forma de cadena (`S-1-5-21-…`).
fn current_user_sid() -> Result<String> {
    // SAFETY: cada llamada se comprueba antes de usar su salida, y los búferes
    // que se pasan viven más que la llamada.
    unsafe {
        let mut raw: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) == 0 {
            bail!(
                "OpenProcessToken failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let token = TokenHandle(raw);

        // Patrón de dos llamadas: la primera sólo devuelve el tamaño.
        let mut needed: u32 = 0;
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            bail!(
                "GetTokenInformation couldn't size the token user: {}",
                std::io::Error::last_os_error()
            );
        }
        let mut buf = vec![0u8; needed as usize];
        if GetTokenInformation(
            token.0,
            TokenUser,
            buf.as_mut_ptr() as *mut c_void,
            needed,
            &mut needed,
        ) == 0
        {
            bail!(
                "GetTokenInformation failed: {}",
                std::io::Error::last_os_error()
            );
        }

        // `buf` es un `Vec<u8>` (alineado a 1) y `TOKEN_USER` necesita
        // alineación de puntero: se lee sin alinear a propósito. El SID en sí
        // sigue viviendo dentro de `buf`, que está vivo hasta el final.
        let user: TOKEN_USER = ptr::read_unaligned(buf.as_ptr() as *const TOKEN_USER);
        let mut sid_str: PWSTR = ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid_str) == 0 {
            bail!(
                "ConvertSidToStringSidW failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let text = wide_to_string(sid_str);
        LocalFree(sid_str as HLOCAL);
        Ok(text)
    }
}

/// Cadena UTF-16 terminada en NUL → `String`.
///
/// # Safety
/// `ptr` debe apuntar a una cadena UTF-16 válida terminada en NUL.
unsafe fn wide_to_string(ptr: PWSTR) -> String {
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
}
